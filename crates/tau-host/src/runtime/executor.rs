//! Single-threaded async executor with timer support.
//!
//! # Borrow discipline
//!
//! CRITICAL DESIGN RULE: Never hold the RUNTIME RefCell borrow while calling
//! plugin poll functions. Plugin code can (and does) call back into the runtime
//! (tau_timer_create, tau_spawn, etc.) which would cause a RefCell panic.
//!
//! The pattern: borrow → extract what we need → drop borrow → call plugin → re-borrow → update state.
//!
//! # Task storage and the cross-plugin boundary
//!
//! Each [`Task`] stores a type-erased future as `(*mut (), poll_fn, drop_fn)`.
//! The `poll_fn` and `drop_fn` are `extern "C"` function pointers that live in
//! the spawning plugin's `.text` section. This means:
//!
//! - A Task's function pointers are only valid while the owning plugin is loaded.
//! - The host tracks `plugin_id` on every task.
//! - [`drop_plugin_tasks`] MUST be called before `dlclose` to ensure all tasks
//!   owned by a plugin are dropped while their function pointers are still valid.
//!
//! The future's **result type `T`** is never seen by the executor. It lives in a
//! `TaskCell<T>` on the plugin's heap, written by the plugin's `WrapperFuture`
//! and read by the plugin's `JoinHandle<T>`. The executor only sees `FfiPoll`
//! (Ready/Pending) — concrete data, not a dynamic type.
//!
//! Concrete data (structs, String, Vec, etc.) CAN safely cross between plugins
//! because the same-compiler invariant guarantees identical layout, and all plugins
//! share the same global allocator. Every plugin gets its own monomorphized copy
//! of drop glue, so the receiving plugin can drop data it didn't allocate.
//!
//! Dynamic types (trait objects, closures, function pointers) CANNOT cross plugin
//! boundaries because they contain pointers into a specific plugin's `.text` section.
//! If that plugin is unloaded, those pointers dangle.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tau_rt::types::{FfiPoll, FfiWaker, RawDropFn, RawPollFn};

// =============================================================================
// Plugin tracking - which plugin is currently active (for task ownership)
// =============================================================================

pub(crate) static CURRENT_PLUGIN: AtomicU64 = AtomicU64::new(0);
static NEXT_PLUGIN_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a new unique plugin ID.
pub fn allocate_plugin_id() -> u64 {
    NEXT_PLUGIN_ID.fetch_add(1, Ordering::Relaxed)
}

/// Set the current plugin ID (call before submitting requests).
pub fn set_current_plugin(id: u64) {
    CURRENT_PLUGIN.store(id, Ordering::Relaxed);
}

/// Clear the current plugin ID.
pub fn clear_current_plugin() {
    CURRENT_PLUGIN.store(0, Ordering::Relaxed);
}

/// Get the current plugin ID.
pub fn current_plugin_id() -> u64 {
    CURRENT_PLUGIN.load(Ordering::Relaxed)
}

// =============================================================================
// Debug logging (TAU_DEBUG=1 to enable)
// =============================================================================

pub fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TAU_DEBUG").map_or(false, |v| v == "1"))
}

macro_rules! rt_debug {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[rt] {}", format!($($arg)*));
        }
    };
}

// =============================================================================
// Global wake queue — Mutex so wakers are Send+Sync
// =============================================================================

static WAKE_QUEUE: OnceLock<Mutex<VecDeque<u64>>> = OnceLock::new();

fn wake_queue() -> &'static Mutex<VecDeque<u64>> {
    WAKE_QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// C-ABI wake function: pushes task_id to the global wake queue.
/// Also notifies the reactor to wake from any blocking poll.
extern "C" fn wake_task_fn(data: *mut ()) {
    let task_id = data as u64;
    rt_debug!("wake task_id={}", task_id);
    wake_queue().lock().unwrap().push_back(task_id);
    
    // Notify the reactor to wake from blocking poll
    // Use try_lock to avoid deadlock if we're called from within reactor.poll()
    crate::runtime::reactor::try_notify_reactor();
}

// =============================================================================
// Thread-local: current task being polled (for timer association)
// =============================================================================

thread_local! {
    pub(crate) static CURRENT_TASK: Cell<u64> = const { Cell::new(0) };
}

// =============================================================================
// Task
// =============================================================================

pub(crate) struct Task {
    future_ptr: *mut (),
    poll_fn: RawPollFn,
    drop_fn: RawDropFn,
    completed: bool,
    aborted: bool,
    plugin_id: u64,  // Which plugin owns this task (0 = host)
}

impl Drop for Task {
    fn drop(&mut self) {
        if !self.future_ptr.is_null() {
            unsafe { (self.drop_fn)(self.future_ptr) };
        }
    }
}

// =============================================================================
// Timer
// =============================================================================

struct TimerEntry {
    task_id: u64,
}

// =============================================================================
// Snapshot of a task for polling (extracted while borrow is held, used after drop)
// =============================================================================

pub struct TaskSnapshot {
    pub task_id: u64,
    pub plugin_id: u64,
    future_ptr: *mut (),
    poll_fn: RawPollFn,
}

// =============================================================================
// Runtime
// =============================================================================

pub struct Runtime {
    tasks: HashMap<u64, Task>,
    ready_queue: VecDeque<u64>,
    timers: BTreeMap<(Instant, u64), TimerEntry>,
    next_task_id: u64,
    next_timer_id: u64,
    initialized: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            tasks: HashMap::new(),
            ready_queue: VecDeque::new(),
            timers: BTreeMap::new(),
            next_task_id: 1,
            next_timer_id: 1,
            initialized: false,
        }
    }
}

impl Runtime {
    pub fn init(&mut self) {
        self.tasks.clear();
        self.ready_queue.clear();
        self.timers.clear();
        self.next_task_id = 1;
        self.next_timer_id = 1;
        self.initialized = true;
    }

    /// Spawn a task. Returns task ID.
    pub fn spawn(
        &mut self,
        future_ptr: *mut (),
        poll_fn: RawPollFn,
        drop_fn: RawDropFn,
    ) -> u64 {
        let task_id = self.next_task_id;
        self.next_task_id += 1;
        let plugin_id = CURRENT_PLUGIN.load(Ordering::Relaxed);

        let task = Task {
            future_ptr,
            poll_fn,
            drop_fn,
            completed: false,
            aborted: false,
            plugin_id,
        };

        self.tasks.insert(task_id, task);
        self.ready_queue.push_back(task_id);
        rt_debug!("spawn task_id={} plugin_id={}", task_id, plugin_id);
        task_id
    }

    /// Create a timer. Returns timer handle.
    pub fn timer_create(&mut self, nanos_from_now: u64) -> u64 {
        let handle = self.next_timer_id;
        self.next_timer_id += 1;

        let deadline = Instant::now() + Duration::from_nanos(nanos_from_now);
        let task_id = CURRENT_TASK.with(|t| t.get());

        rt_debug!("timer_create handle={} task_id={} deadline_ms={}", handle, task_id, nanos_from_now / 1_000_000);

        self.timers.insert(
            (deadline, handle),
            TimerEntry { task_id },
        );
        handle
    }

    /// Check if a timer has fired. Returns 1 if ready, 0 if pending.
    pub fn timer_check(&mut self, handle: u64) -> u8 {
        let now = Instant::now();
        let key = self.timers.keys().find(|(_, h)| *h == handle).copied();
        match key {
            Some(k) if k.0 <= now => {
                self.timers.remove(&k);
                1
            }
            Some(_) => 0,
            None => 1, // Already fired/cancelled → treat as ready
        }
    }

    /// Cancel a timer.
    pub fn timer_cancel(&mut self, handle: u64) {
        let key = self.timers.keys().find(|(_, h)| *h == handle).copied();
        if let Some(k) = key {
            self.timers.remove(&k);
        }
    }

    /// Drop all tasks belonging to a plugin (for unload).
    /// Sets CURRENT_PLUGIN so any tau calls in destructors are tagged correctly.
    /// Returns the number of tasks dropped.
    /// Remove all tasks belonging to a plugin and return them.
    /// The caller must drop the returned tasks OUTSIDE of any RUNTIME borrow
    /// (same as `take_completed`).
    pub fn take_plugin_tasks(&mut self, plugin_id: u64) -> Vec<Task> {
        let task_ids: Vec<u64> = self.tasks
            .iter()
            .filter(|(_, t)| t.plugin_id == plugin_id)
            .map(|(id, _)| *id)
            .collect();
        
        let mut removed = Vec::with_capacity(task_ids.len());
        for task_id in &task_ids {
            rt_debug!("dropping task_id={} for plugin_id={}", task_id, plugin_id);
            if let Some(task) = self.tasks.remove(task_id) {
                removed.push(task);
            }
            self.ready_queue.retain(|id| id != task_id);
        }
        
        // Remove timers associated with dropped tasks
        let timer_keys: Vec<_> = self.timers
            .iter()
            .filter(|(_, entry)| {
                !self.tasks.contains_key(&entry.task_id)
            })
            .map(|(k, _)| *k)
            .collect();
        for key in timer_keys {
            self.timers.remove(&key);
        }
        
        removed
    }

    /// Check if a specific task is completed.
    pub fn is_task_done(&self, task_id: u64) -> bool {
        match self.tasks.get(&task_id) {
            Some(t) => t.completed,
            None => true,
        }
    }

    /// Mark a task as completed.
    pub fn mark_completed(&mut self, task_id: u64) {
        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.completed = true;
            rt_debug!("completed task_id={}", task_id);
        }
    }

    /// Abort a task: mark it aborted, remove from ready queue, clean up timers.
    /// The future is NOT dropped immediately — it is deferred to `cleanup_completed()`
    /// so that self-abort (a task aborting itself during its own poll) is safe.
    /// Returns true if the task existed and was aborted.
    pub fn abort_task(&mut self, task_id: u64) -> bool {
        let task = match self.tasks.get_mut(&task_id) {
            Some(t) if !t.completed && !t.aborted => t,
            _ => return false,
        };

        task.aborted = true;
        task.completed = true;
        rt_debug!("abort task_id={} plugin_id={}", task_id, task.plugin_id);

        // Remove from ready queue
        self.ready_queue.retain(|id| *id != task_id);

        // Clean up timers associated with this task
        let timer_keys: Vec<_> = self.timers
            .iter()
            .filter(|(_, entry)| entry.task_id == task_id)
            .map(|(k, _)| *k)
            .collect();
        for key in timer_keys {
            self.timers.remove(&key);
        }

        // Future drop is deferred to cleanup_completed() — safe even if
        // this task is currently being polled (self-abort scenario).

        true
    }

    /// Check if a task is finished (completed, aborted, or already removed).
    pub fn is_task_finished(&self, task_id: u64) -> bool {
        match self.tasks.get(&task_id) {
            Some(t) => t.completed,
            None => true, // already removed = finished
        }
    }

    /// Remove a completed task.
    pub fn remove_task(&mut self, task_id: u64) {
        self.tasks.remove(&task_id);
    }

    /// Fire expired timers: push associated tasks to ready queue.
    fn fire_timers(&mut self) {
        let now = Instant::now();
        let mut expired = Vec::new();

        for (key, entry) in &self.timers {
            if key.0 <= now {
                expired.push((*key, entry.task_id));
            } else {
                break; // BTreeMap is sorted
            }
        }

        for (key, task_id) in expired {
            self.timers.remove(&key);
            if self.tasks.contains_key(&task_id) && !self.tasks[&task_id].completed {
                self.ready_queue.push_back(task_id);
            }
        }
    }

    /// Get duration until next timer fires.
    pub fn next_timer_deadline(&self) -> Option<Duration> {
        self.timers.keys().next().map(|(deadline, _)| {
            let now = Instant::now();
            if *deadline > now {
                *deadline - now
            } else {
                Duration::ZERO
            }
        })
    }

    /// Prepare a drive cycle: drain wake queue, fire timers, collect tasks to poll.
    /// Returns snapshots of tasks that need polling.
    /// After this call, the borrow can be dropped before polling.
    pub fn prepare_drive(&mut self) -> Vec<TaskSnapshot> {
        // 1. Drain global wake queue into ready queue
        let mut woken = 0usize;
        {
            let mut wq = wake_queue().lock().unwrap();
            while let Some(task_id) = wq.pop_front() {
                self.ready_queue.push_back(task_id);
                woken += 1;
            }
        }

        // 2. Fire expired timers
        let timers_before = self.timers.len();
        self.fire_timers();
        let timers_fired = timers_before - self.timers.len();

        // 3. Snapshot tasks to poll (extract fn ptrs so we can drop the borrow)
        let task_ids: Vec<u64> = self.ready_queue.drain(..).collect();
        let mut snapshots = Vec::with_capacity(task_ids.len());

        for task_id in task_ids {
            if let Some(task) = self.tasks.get(&task_id) {
                if !task.completed {
                    snapshots.push(TaskSnapshot {
                        task_id,
                        plugin_id: task.plugin_id,
                        future_ptr: task.future_ptr,
                        poll_fn: task.poll_fn,
                    });
                }
            }
        }

        if debug_enabled() && (woken > 0 || timers_fired > 0 || !snapshots.is_empty()) {
            rt_debug!(
                "drive: woken={} timers_fired={} to_poll={} live_tasks={} pending_timers={}",
                woken, timers_fired, snapshots.len(), self.tasks.len(), self.timers.len()
            );
        }

        snapshots
    }

    /// Remove completed tasks from the map and return them.
    /// The caller must drop the returned tasks OUTSIDE of any RUNTIME borrow,
    /// because Task::drop calls (drop_fn)(future_ptr) which may call back into
    /// the runtime (e.g., SleepFuture::drop → tau_timer_cancel).
    pub fn take_completed(&mut self) -> Vec<Task> {
        let done: Vec<u64> = self.tasks.iter()
            .filter(|(_, t)| t.completed)
            .map(|(id, _)| *id)
            .collect();
        done.into_iter()
            .filter_map(|id| self.tasks.remove(&id))
            .collect()
    }

    /// Check if there are any live tasks.
    #[allow(dead_code)]
    pub fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Return (live_tasks, pending_timers, ready_queue_len) for debug logging.
    pub fn debug_stats(&self) -> (usize, usize, usize) {
        (self.tasks.len(), self.timers.len(), self.ready_queue.len())
    }
}

/// Poll a task snapshot. Called WITHOUT holding the runtime borrow.
/// Sets CURRENT_PLUGIN so any tau::resource/tau::event calls are tagged correctly.
/// Returns FfiPoll result.
pub fn poll_snapshot(snap: &TaskSnapshot) -> FfiPoll {
    CURRENT_TASK.with(|t| t.set(snap.task_id));
    let prev_plugin = CURRENT_PLUGIN.load(Ordering::Relaxed);
    CURRENT_PLUGIN.store(snap.plugin_id, Ordering::Relaxed);

    let waker = FfiWaker {
        data: snap.task_id as *mut (),
        wake_fn: Some(wake_task_fn),
    };

    rt_debug!("poll task_id={} plugin_id={} future_ptr={:?}", snap.task_id, snap.plugin_id, snap.future_ptr);
    let result = unsafe { (snap.poll_fn)(snap.future_ptr, waker) };
    rt_debug!("poll task_id={} → {:?}", snap.task_id, result);

    CURRENT_PLUGIN.store(prev_plugin, Ordering::Relaxed);
    CURRENT_TASK.with(|t| t.set(0));
    result
}

thread_local! {
    pub static RUNTIME: std::cell::RefCell<Runtime> = std::cell::RefCell::new(Runtime::default());
}
