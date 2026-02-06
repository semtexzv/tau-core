//! Runtime Implementation — C ABI exports
//!
//! CRITICAL: The RUNTIME RefCell must NOT be held while calling plugin poll functions.
//! All #[no_mangle] functions that call into plugin code follow the pattern:
//!   borrow → extract → drop borrow → call plugin → re-borrow → update

pub mod executor;
pub mod reactor;
pub mod resources;
pub mod events;
pub mod plugin_guard;
pub mod process;

use executor::{poll_snapshot, RUNTIME};
use reactor::{init_reactor, with_reactor};
use tau_rt::types::{FfiPoll, FfiWaker, RawDropFn, RawPollFn};
use std::os::fd::RawFd;
use std::time::Duration;

// =============================================================================
// Core runtime exports
// =============================================================================

#[no_mangle]
pub extern "C" fn tau_runtime_init() -> i32 {
    RUNTIME.with(|rt| {
        rt.borrow_mut().init();
    });
    // Initialize the IO reactor
    if let Err(e) = init_reactor() {
        eprintln!("[rt] Failed to initialize reactor: {}", e);
        return -1;
    }
    0
}

#[no_mangle]
pub extern "C" fn tau_spawn(future_ptr: *mut (), poll_fn: RawPollFn, drop_fn: RawDropFn) -> u64 {
    RUNTIME.with(|rt| {
        rt.borrow_mut().spawn(future_ptr, poll_fn, drop_fn)
    })
}

/// Drive the runtime: drain wake queue, fire timers, poll ready tasks.
/// Safe for re-entrant calls from plugin code.
#[no_mangle]
pub extern "C" fn tau_drive() -> u32 {
    // Phase 1: prepare (holds borrow briefly)
    let snapshots = RUNTIME.with(|rt| {
        rt.borrow_mut().prepare_drive()
    });
    // Borrow is dropped here!

    // Phase 2: poll each task (NO borrow held — plugin code can call back)
    let mut completed = 0u32;
    for snap in &snapshots {
        // Guard: skip if task was already completed by an earlier poll in this cycle
        let already_done = RUNTIME.with(|rt| rt.borrow().is_task_done(snap.task_id));
        if already_done {
            continue;
        }

        let result = poll_snapshot(snap);

        // Phase 3: record result (brief borrow)
        if result == FfiPoll::Ready {
            RUNTIME.with(|rt| {
                rt.borrow_mut().mark_completed(snap.task_id);
            });
            completed += 1;
        }
    }

    // Phase 4: extract completed tasks (brief borrow)
    let dead_tasks = RUNTIME.with(|rt| {
        rt.borrow_mut().take_completed()
    });
    // Borrow is dropped here! Now dropping tasks is safe — their destructors
    // can call back into the runtime (e.g., SleepFuture::drop → tau_timer_cancel).
    drop(dead_tasks);

    completed
}

/// Block on a task until completion, driving the runtime.
#[no_mangle]
pub extern "C" fn tau_block_on(task_id: u64) {
    let mut cycles = 0u64;
    loop {
        // Check if done
        let done = RUNTIME.with(|rt| rt.borrow().is_task_done(task_id));
        if done {
            RUNTIME.with(|rt| rt.borrow_mut().remove_task(task_id));
            return;
        }

        // Drive one cycle (this drops/re-acquires borrow internally)
        tau_drive();
        cycles += 1;

        // Check again after driving
        let done = RUNTIME.with(|rt| rt.borrow().is_task_done(task_id));
        if done {
            RUNTIME.with(|rt| rt.borrow_mut().remove_task(task_id));
            return;
        }

        // Check if there's pending work (tasks in ready queue)
        let ready_count = RUNTIME.with(|rt| {
            rt.borrow().debug_stats().2  // ready_queue.len()
        });

        if ready_count > 0 {
            // Tasks are ready, don't block
            continue;
        }

        // Log every 1000 cycles if TAU_DEBUG is set
        #[cfg(debug_assertions)]
        if cycles % 1000 == 0 && std::env::var("TAU_DEBUG").is_ok() {
            let (live, timers, ready) = RUNTIME.with(|rt| {
                rt.borrow().debug_stats()
            });
            eprintln!(
                "[rt] block_on task_id={} cycles={} live_tasks={} timers={} ready={}",
                task_id, cycles, live, timers, ready
            );
        }

        // Calculate timeout: min of next timer and 10ms (for responsiveness)
        let timeout = RUNTIME.with(|rt| {
            rt.borrow().next_timer_deadline()
                .map(|d| d.min(Duration::from_millis(10)))
                .unwrap_or(Duration::from_millis(10))
        });

        // Poll the IO reactor (this is the proper blocking point)
        let _ = with_reactor(|reactor| {
            reactor.poll(Some(timeout))
        });
    }
}

// =============================================================================
// Task abort export
// =============================================================================

/// Abort a task by ID. Returns 1 (accepted).
///
/// The abort is queued and processed on the main thread during the next drive cycle.
/// This makes it safe to call from any thread (e.g., `spawn_blocking` background threads)
/// where the thread-local RUNTIME is not available.
///
/// Always returns 1 ("accepted"). Use `tau_task_is_finished` to check if the task
/// was actually aborted.
#[no_mangle]
pub extern "C" fn tau_task_abort(task_id: u64) -> u8 {
    executor::queue_abort(task_id);
    1
}

/// Check if a task is finished (completed, aborted, or removed).
#[no_mangle]
pub extern "C" fn tau_task_is_finished(task_id: u64) -> u8 {
    RUNTIME.with(|rt| {
        if rt.borrow().is_task_finished(task_id) { 1 } else { 0 }
    })
}

// =============================================================================
// Plugin ID export (used by tau::resource and tau::event)
// =============================================================================

/// Get the current plugin ID. Called by tau crate to tag resources/subscriptions.
#[no_mangle]
pub extern "C" fn tau_current_plugin_id() -> u64 {
    executor::current_plugin_id()
}

// =============================================================================
// Plugin lifecycle management
// =============================================================================

/// Allocate a unique plugin ID for tracking task ownership.
pub fn allocate_plugin_id() -> u64 {
    executor::allocate_plugin_id()
}

/// Set the current plugin (tasks spawned will be owned by this plugin).
pub fn set_current_plugin(id: u64) {
    executor::set_current_plugin(id);
}

/// Clear the current plugin.
pub fn clear_current_plugin() {
    executor::clear_current_plugin();
}

/// Drop all tasks owned by a plugin (for unload).
/// SAFETY: Call this BEFORE dlclose() to avoid dangling pointers.
pub fn drop_plugin_tasks(plugin_id: u64) -> usize {
    let tasks = RUNTIME.with(|rt| rt.borrow_mut().take_plugin_tasks(plugin_id));
    let count = tasks.len();
    // Set current plugin so destructors can identify their plugin context
    let prev_plugin = executor::CURRENT_PLUGIN.load(std::sync::atomic::Ordering::Relaxed);
    executor::CURRENT_PLUGIN.store(plugin_id, std::sync::atomic::Ordering::Relaxed);
    // Drop tasks OUTSIDE the borrow — their destructors may call back into the runtime
    drop(tasks);
    executor::CURRENT_PLUGIN.store(prev_plugin, std::sync::atomic::Ordering::Relaxed);
    count
}

/// Drop all event subscriptions for a plugin.
pub fn drop_plugin_events(plugin_id: u64) -> usize {
    events::drop_plugin_events(plugin_id)
}

/// Drop all resources owned by a plugin.
pub fn drop_plugin_resources(plugin_id: u64) -> usize {
    resources::drop_plugin_resources(plugin_id)
}

// =============================================================================
// Timer exports (simple borrows, no plugin callbacks)
// =============================================================================

#[no_mangle]
pub extern "C" fn tau_timer_create(nanos_from_now: u64) -> u64 {
    RUNTIME.with(|rt| {
        rt.borrow_mut().timer_create(nanos_from_now)
    })
}

#[no_mangle]
pub extern "C" fn tau_timer_check(handle: u64) -> u8 {
    RUNTIME.with(|rt| {
        rt.borrow_mut().timer_check(handle)
    })
}

#[no_mangle]
pub extern "C" fn tau_timer_cancel(handle: u64) {
    RUNTIME.with(|rt| {
        rt.borrow_mut().timer_cancel(handle)
    })
}

// =============================================================================
// IO Reactor exports
// =============================================================================

/// Register a file descriptor with the IO reactor.
/// Returns a handle (token) for use in other reactor calls.
/// interest: READABLE (0x01) | WRITABLE (0x02)
#[no_mangle]
pub extern "C" fn tau_io_register(fd: RawFd, interest: u8) -> u64 {
    with_reactor(|reactor| {
        match reactor.register(fd, interest) {
            Ok(token) => token as u64,
            Err(e) => {
                eprintln!("[rt] tau_io_register error: {}", e);
                0  // 0 indicates error
            }
        }
    })
}

/// Poll for IO readiness in a direction.
/// direction: 0 = read, 1 = write
/// Returns: 1 if ready, 0 if pending (waker stored)
#[no_mangle]
pub extern "C" fn tau_io_poll_ready(handle: u64, direction: u8, waker: FfiWaker) -> u8 {
    // Convert FfiWaker to std::task::Waker
    let waker = waker.into_waker();
    
    with_reactor(|reactor| {
        if reactor.poll_ready(handle as usize, direction, &waker) {
            1
        } else {
            0
        }
    })
}

/// Clear readiness for a direction (call after WouldBlock).
/// direction: 0 = read, 1 = write
#[no_mangle]
pub extern "C" fn tau_io_clear_ready(handle: u64, direction: u8) {
    with_reactor(|reactor| {
        reactor.clear_ready(handle as usize, direction);
    })
}

/// Deregister a file descriptor from the reactor.
#[no_mangle]
pub extern "C" fn tau_io_deregister(handle: u64) {
    with_reactor(|reactor| {
        let _ = reactor.deregister(handle as usize);
    })
}

/// Wake the reactor from another thread.
/// Call this after pushing to the wake queue from a background thread.
#[no_mangle]
pub extern "C" fn tau_reactor_notify() {
    with_reactor(|reactor| {
        let _ = reactor.notify();
    })
}

/// Poll the IO reactor for pending events.
/// This is used by the host's `drive` command to progress IO tasks that aren't
/// being waited on by `block_on`.
pub fn poll_reactor(timeout: Option<Duration>) {
    with_reactor(|reactor| {
        let _ = reactor.poll(timeout);
    })
}

/// Poll the reactor from FFI (used by plugins implementing custom event loops).
/// millis: timeout in milliseconds.
#[no_mangle]
pub extern "C" fn tau_react(millis: u64) {
    poll_reactor(Some(Duration::from_millis(millis)));
}
