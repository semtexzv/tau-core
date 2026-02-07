//! Executor API — thin wrapper over host-provided implementation.
//!
//! # Why This Is a Thin Wrapper
//!
//! The actual executor implementation lives in `tau-host`, not here. This is
//! intentional and load-bearing:
//!
//! **The Problem**: tau-rt is a dylib that plugins compile against (via
//! `--extern=tau_rt=/path/to/libtau_rt.dylib`). When Rust compiles against a
//! dylib, it needs the rmeta for ALL of that dylib's dependencies. If tau-rt
//! depended on `crossbeam-queue`, plugins would fail with:
//!
//! ```text
//! error[E0463]: can't find crate for `crossbeam_queue` which `tau_rt` depends on
//! ```
//!
//! **The Solution**: Keep tau-rt dependency-free (only std). The executor
//! implementation (with crossbeam-queue for lock-free cross-thread wakes)
//! lives in the host. This module provides thin wrappers around extern
//! functions that the host exports.
//!
//! # Same-Compiler Guarantee
//!
//! We use `extern "Rust"` (not `extern "C"`) because the same-compiler
//! invariant guarantees identical ABI between host and plugins. This
//! preserves Rust's calling conventions and allows returning complex
//! types like `Option<Duration>` directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Waker;
use std::time::{Duration, Instant};

use crate::ffi;

// Re-export function pointer types from ffi
pub use crate::ffi::{PollFn, DropResultFn, DropFutureFn, ReadFn, DeallocFn};

// =============================================================================
// Constants (duplicated from host, must stay in sync)
// =============================================================================

pub const SEGMENT_SIZE: usize = 256;
pub const MAX_SEGMENTS: usize = 64;
pub const INLINE_SIZE: usize = 80;
pub const MAX_EXECUTORS: usize = 256;

pub const STATE_EMPTY: u8 = 0;
pub const STATE_PENDING: u8 = 1;
pub const STATE_READY: u8 = 2;
pub const STATE_COMPLETE: u8 = 3;
pub const STATE_CANCELLED: u8 = 4;

// =============================================================================
// Plugin tracking (local atomic, shared via dylib)
// =============================================================================

pub static CURRENT_PLUGIN: AtomicU64 = AtomicU64::new(0);

// =============================================================================
// Public API — thin wrappers around extern functions
// =============================================================================

pub fn init() -> u16 {
    unsafe { ffi::tau_exec_init() }
}

pub fn current_executor() -> u16 {
    unsafe { ffi::tau_exec_current_executor() }
}

pub fn alloc_raw() -> (u32, u32, *mut u8, u16) {
    unsafe { ffi::tau_exec_alloc_raw() }
}

pub fn commit_spawn(
    id: u32,
    poll_fn: PollFn,
    drop_result_fn: Option<DropResultFn>,
    drop_future_fn: Option<DropFutureFn>,
    read_fn: Option<ReadFn>,
    dealloc_fn: Option<DeallocFn>,
    plugin_id: u64,
) {
    unsafe { ffi::tau_exec_commit_spawn(id, poll_fn, drop_result_fn, drop_future_fn, read_fn, dealloc_fn, plugin_id) }
}

pub fn drive_cycle() -> bool {
    unsafe { ffi::tau_exec_drive_cycle() }
}

pub fn cancel_task(id: u32, gen: u32, executor_idx: u16) {
    unsafe { ffi::tau_exec_cancel_task(id, gen, executor_idx) }
}

pub fn is_task_finished(id: u32, gen: u32, executor_idx: u16) -> bool {
    unsafe { ffi::tau_exec_is_task_finished(id, gen, executor_idx) }
}

pub fn detach_or_cleanup(id: u32, gen: u32, executor_idx: u16) {
    unsafe { ffi::tau_exec_detach_or_cleanup(id, gen, executor_idx) }
}

pub fn timer_register(deadline: Instant, waker: Waker, old_handle: u64) -> u64 {
    let now = Instant::now();
    let duration = deadline.saturating_duration_since(now);
    unsafe { ffi::tau_exec_timer_register(duration.as_secs(), duration.subsec_nanos(), waker, old_handle) }
}

pub fn timer_cancel(handle: u64) {
    unsafe { ffi::tau_exec_timer_cancel(handle) }
}

pub fn next_timer_deadline() -> Option<Duration> {
    unsafe { ffi::tau_exec_next_timer_deadline() }
}

pub fn current_plugin_id() -> u64 {
    // Read from local atomic (shared via dylib)
    CURRENT_PLUGIN.load(Ordering::Relaxed)
}

pub fn allocate_plugin_id() -> u64 {
    unsafe { ffi::tau_exec_allocate_plugin_id() }
}

pub fn set_current_plugin(id: u64) {
    CURRENT_PLUGIN.store(id, Ordering::Relaxed);
    unsafe { ffi::tau_exec_set_current_plugin(id) }
}

pub fn clear_current_plugin() {
    CURRENT_PLUGIN.store(0, Ordering::Relaxed);
    unsafe { ffi::tau_exec_clear_current_plugin() }
}

pub fn drop_plugin_tasks(plugin_id: u64) -> usize {
    unsafe { ffi::tau_exec_drop_plugin_tasks(plugin_id) }
}

pub fn wait_for_task(packed_id: u64) {
    unsafe { ffi::tau_exec_wait_for_task(packed_id) }
}

pub fn make_waker(executor_idx: u16, task_id: u32) -> Waker {
    unsafe { ffi::tau_exec_make_waker(executor_idx, task_id) }
}

pub fn pack_task_id(gen: u32, slot_id: u32) -> u64 {
    unsafe { ffi::tau_exec_pack_task_id(gen, slot_id) }
}

pub fn unpack_task_id(packed: u64) -> (u32, u32) {
    unsafe { ffi::tau_exec_unpack_task_id(packed) }
}

pub fn debug_stats() -> (usize, usize, usize) {
    unsafe { ffi::tau_exec_debug_stats() }
}

pub fn debug_enabled() -> bool {
    unsafe { ffi::tau_exec_debug_enabled() }
}

pub fn slot_state(id: u32, executor_idx: u16) -> (u8, u32) {
    unsafe { ffi::tau_exec_slot_state(id, executor_idx) }
}

pub fn set_join_waker(id: u32, executor_idx: u16, waker: Waker) {
    unsafe { ffi::tau_exec_set_join_waker(id, executor_idx, waker) }
}

pub fn take_result(id: u32, executor_idx: u16) -> (*mut u8, Option<DeallocFn>) {
    unsafe { ffi::tau_exec_take_result(id, executor_idx) }
}

pub fn dealloc_storage(id: u32, executor_idx: u16) {
    unsafe { ffi::tau_exec_dealloc_storage(id, executor_idx) }
}

// =============================================================================
// FFI exports — backward-compatible symbols for plugins using extern "C"
// =============================================================================

#[no_mangle]
pub extern "C" fn tau_task_abort(packed_id: u64) -> u8 {
    if packed_id == 0 { return 0; }
    let (gen, slot_id) = unpack_task_id(packed_id);
    let executor_idx = current_executor();
    cancel_task(slot_id, gen, executor_idx);
    1
}

#[no_mangle]
pub extern "C" fn tau_task_is_finished(packed_id: u64) -> u8 {
    if packed_id == 0 { return 1; }
    let (gen, slot_id) = unpack_task_id(packed_id);
    let executor_idx = current_executor();
    if is_task_finished(slot_id, gen, executor_idx) { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn tau_current_plugin_id() -> u64 {
    current_plugin_id()
}
