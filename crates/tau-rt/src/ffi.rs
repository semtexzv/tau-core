//! Consolidated extern declarations for host-provided functions.
//!
//! All runtime functionality is provided by tau-host at runtime. This module
//! declares the extern functions that plugins call. tau-rt is a thin dylib
//! with NO external dependencies — it only re-exports these declarations and
//! provides thin wrappers.
//!
//! # Organization
//!
//! - **Executor** (`tau_exec_*`): Task spawning, cancellation, timers, wakers
//! - **Resources** (`tau_resource_*`): Named resource registry
//! - **IO** (`tau_io_*`): Reactor integration for async IO
//! - **Process** (`tau_host_*`): Process/signal handling
//!
//! # ABI
//!
//! We use `extern "Rust"` for executor functions (same-compiler invariant ensures
//! ABI compatibility) and `extern "C"` for others (stable ABI across compilations).

use std::os::fd::RawFd;
use std::task::Waker;
use std::time::Duration;

use crate::types::FfiWaker;

// =============================================================================
// Executor function pointer types (for type-erased task storage)
// =============================================================================

/// Poll a task's future. Returns true if ready, false if pending.
pub type PollFn = unsafe fn(*mut u8, &Waker) -> std::task::Poll<()>;

/// Drop the future (before completion).
pub type DropFutureFn = unsafe fn(*mut u8);

/// Drop the result (after completion, if not taken).
pub type DropResultFn = unsafe fn(*mut u8);

/// Read the result out of storage.
pub type ReadFn = unsafe fn(*mut u8) -> *mut u8;

/// Deallocate boxed storage (for large futures).
pub type DeallocFn = unsafe fn(*mut u8);

// =============================================================================
// Executor — task spawning, scheduling, timers, plugin tracking
// =============================================================================

#[allow(dead_code)] // Runtime-resolved, not all called from this crate
#[allow(improper_ctypes)] // Waker, Duration are same-compiler safe
extern "Rust" {
    /// Initialize executor thread-local state. Returns executor index.
    pub fn tau_exec_init() -> u16;

    /// Get current executor index.
    pub fn tau_exec_current_executor() -> u16;

    /// Allocate a task slot. Returns (slot_id, generation, storage_ptr, executor_idx).
    pub fn tau_exec_alloc_raw() -> (u32, u32, *mut u8, u16);

    /// Commit a spawned task after writing the future to storage.
    pub fn tau_exec_commit_spawn(
        id: u32,
        poll_fn: PollFn,
        drop_result_fn: Option<DropResultFn>,
        drop_future_fn: Option<DropFutureFn>,
        read_fn: Option<ReadFn>,
        dealloc_fn: Option<DeallocFn>,
        plugin_id: u64,
    );

    /// Drive one cycle of the executor. Returns true if work was done.
    pub fn tau_exec_drive_cycle() -> bool;

    /// Cancel a task.
    pub fn tau_exec_cancel_task(id: u32, gen: u32, executor_idx: u16);

    /// Check if a task has finished.
    pub fn tau_exec_is_task_finished(id: u32, gen: u32, executor_idx: u16) -> bool;

    /// Detach or cleanup a task slot.
    pub fn tau_exec_detach_or_cleanup(id: u32, gen: u32, executor_idx: u16);

    /// Register a timer. Returns a handle for cancellation.
    pub fn tau_exec_timer_register(
        deadline_secs: u64,
        deadline_nanos: u32,
        waker: Waker,
        old_handle: u64,
    ) -> u64;

    /// Cancel a timer by handle.
    pub fn tau_exec_timer_cancel(handle: u64);

    /// Get the next timer deadline (for reactor timeout).
    pub fn tau_exec_next_timer_deadline() -> Option<Duration>;

    /// Get the current plugin ID (0 if none).
    pub fn tau_exec_current_plugin_id() -> u64;

    /// Allocate a new unique plugin ID.
    pub fn tau_exec_allocate_plugin_id() -> u64;

    /// Set the current plugin ID for this thread.
    pub fn tau_exec_set_current_plugin(id: u64);

    /// Clear the current plugin ID.
    pub fn tau_exec_clear_current_plugin();

    /// Drop all tasks belonging to a plugin. Returns count dropped.
    pub fn tau_exec_drop_plugin_tasks(plugin_id: u64) -> usize;

    /// Block until a task finishes (for JoinHandle::blocking_join).
    pub fn tau_exec_wait_for_task(packed_id: u64);

    /// Create a std::task::Waker for a task.
    pub fn tau_exec_make_waker(executor_idx: u16, task_id: u32) -> Waker;

    /// Pack (generation, slot_id) into a u64 for FFI.
    pub fn tau_exec_pack_task_id(gen: u32, slot_id: u32) -> u64;

    /// Unpack a u64 into (generation, slot_id).
    pub fn tau_exec_unpack_task_id(packed: u64) -> (u32, u32);

    /// Get executor stats: (ready_count, live_tasks, timer_count).
    pub fn tau_exec_debug_stats() -> (usize, usize, usize);

    /// Check if debug output is enabled.
    pub fn tau_exec_debug_enabled() -> bool;

    /// Get task slot state for JoinHandle polling. Returns (state, gen).
    pub fn tau_exec_slot_state(id: u32, executor_idx: u16) -> (u8, u32);

    /// Set the join waker for a task.
    pub fn tau_exec_set_join_waker(id: u32, executor_idx: u16, waker: Waker);

    /// Take the result from a completed task. Returns (ptr, dealloc_fn).
    pub fn tau_exec_take_result(id: u32, executor_idx: u16) -> (*mut u8, Option<DeallocFn>);

    /// Deallocate task storage.
    pub fn tau_exec_dealloc_storage(id: u32, executor_idx: u16);
}

// =============================================================================
// Resources — named resource registry
// =============================================================================

extern "C" {
    /// Store a resource in the registry.
    pub fn tau_resource_put(
        name_ptr: *const u8,
        name_len: usize,
        ptr: *mut (),
        type_id: u128,
        plugin_id: u64,
        drop_fn: unsafe extern "C" fn(*mut ()),
    );

    /// Get a shared reference to a resource (returns null if not found or type mismatch).
    pub fn tau_resource_get(name_ptr: *const u8, name_len: usize, type_id: u128) -> *const ();

    /// Take ownership of a resource (removes from registry).
    pub fn tau_resource_take(name_ptr: *const u8, name_len: usize, type_id: u128) -> *mut ();
}

// =============================================================================
// IO — reactor integration for async IO
// =============================================================================

extern "C" {
    /// Register a file descriptor with the IO reactor.
    /// interest: bit flags for read/write interest.
    /// Returns a handle (token) for use in other reactor calls, 0 on error.
    pub fn tau_io_register(fd: RawFd, interest: u8) -> u64;

    /// Poll for IO readiness in a direction.
    /// direction: 0 = read, 1 = write
    /// Returns: 1 if ready, 0 if pending (waker stored).
    pub fn tau_io_poll_ready(handle: u64, direction: u8, waker: FfiWaker) -> u8;

    /// Clear readiness for a direction (call after WouldBlock).
    pub fn tau_io_clear_ready(handle: u64, direction: u8);

    /// Deregister a file descriptor from the reactor.
    pub fn tau_io_deregister(handle: u64);

    /// Poll the reactor for events (blocking up to timeout).
    /// millis: timeout in milliseconds.
    pub fn tau_react(millis: u64);
}

// =============================================================================
// Process — signal handling
// =============================================================================

extern "C" {
    /// Subscribe to SIGCHLD notifications.
    pub fn tau_host_sigchld_subscribe(waker: FfiWaker);
}
