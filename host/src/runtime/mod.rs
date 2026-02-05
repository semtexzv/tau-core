//! µTokio Runtime Implementation
//!
//! Single-threaded async executor with C ABI exports.
//! Hosts load this as a dylib and plugins resolve symbols from it.

pub mod executor;

use executor::RUNTIME;
use std::sync::atomic::{AtomicU64, Ordering};
use tau::types::{FfiPoll, FfiWaker, TaskId};
use tau::types::{RawDropFn, RawPollFn};

// =============================================================================
// Exported C ABI functions (Internal but exported for dylib linking)
// =============================================================================

/// Initialize the runtime
#[no_mangle]
pub extern "C" fn tau_runtime_init() -> i32 {
    RUNTIME.with(|rt| {
        rt.borrow_mut().init();
    });
    0
}

/// Spawn a new task
#[no_mangle]
pub extern "C" fn tau_spawn(future_ptr: *mut (), poll_fn: RawPollFn, drop_fn: RawDropFn) -> TaskId {
    static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);
    let task_id = TaskId(TASK_COUNTER.fetch_add(1, Ordering::Relaxed));

    RUNTIME.with(|rt| {
        rt.borrow_mut().spawn(task_id, future_ptr, poll_fn, drop_fn);
    });

    task_id
}

/// Block on a task until completion
#[no_mangle]
pub extern "C" fn tau_block_on(task_id: TaskId) {
    RUNTIME.with(|rt| {
        rt.borrow_mut().block_on(task_id);
    });
}

/// Poll a specific task
#[no_mangle]
pub extern "C" fn tau_poll_task(task_id: TaskId, waker: FfiWaker) -> FfiPoll {
    RUNTIME.with(|rt| rt.borrow_mut().poll_task(task_id, waker))
}

/// Drive the runtime - poll all ready tasks
#[no_mangle]
pub extern "C" fn tau_drive() -> u32 {
    RUNTIME.with(|rt| rt.borrow_mut().drive())
}
