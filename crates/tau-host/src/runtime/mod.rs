//! Runtime Implementation — hosts the executor and IO reactor.
//!
//! # Why the Executor Lives Here
//!
//! The executor implementation lives in the host (not tau-rt) because tau-rt
//! is a dylib that plugins compile against. If tau-rt had dependencies like
//! `crossbeam-queue`, plugins would fail to compile with "can't find crate"
//! errors since the dependency rmeta isn't available during plugin compilation.
//!
//! By keeping the executor here:
//! - tau-rt is dependency-free (only std) and plugins can compile against it
//! - The host provides the actual implementation via exported functions
//! - crossbeam-queue is statically linked into the host, invisible to plugins
//!
//! See `executor.rs` for the full explanation.
//!
//! The host provides:
//! - **Executor** (slab-based async executor with timers)
//! - IO reactor (epoll/kqueue via `polling` crate)
//! - Resource registry
//! - SIGCHLD handling
//! - Plugin lifecycle management

pub mod sync;
pub mod executor;
pub mod reactor;
pub mod resources;
pub mod plugin_guard;

use reactor::{init_reactor, with_reactor};
use tau_rt::types::FfiWaker;
use std::os::fd::RawFd;
use std::time::Duration;

// =============================================================================
// Runtime initialization
// =============================================================================

/// Initialize the runtime: executor + IO reactor.
pub fn tau_runtime_init() {
    // Initialize the executor (in this module, not tau-rt)
    executor::init();

    // Initialize the IO reactor
    if let Err(e) = init_reactor() {
        eprintln!("[rt] Failed to initialize reactor: {}", e);
    }
}

// =============================================================================
// Drive / block_on
// =============================================================================

/// Drive the executor for one cycle.
#[no_mangle]
pub extern "C" fn tau_drive() -> u32 {
    let mut completed = 0u32;
    while executor::drive_cycle() {
        completed += 1;
    }
    completed
}

/// Block on a task until completion (packed task ID from JoinHandle::task_id).
pub fn tau_block_on(packed_task_id: u64) {
    executor::wait_for_task(packed_task_id);
}

// =============================================================================
// Plugin lifecycle management
// =============================================================================

pub fn allocate_plugin_id() -> u64 {
    executor::allocate_plugin_id()
}

pub fn set_current_plugin(id: u64) {
    executor::set_current_plugin(id);
}

pub fn clear_current_plugin() {
    executor::clear_current_plugin();
}

/// Drop all tasks owned by a plugin (for unload).
pub fn drop_plugin_tasks(plugin_id: u64) -> usize {
    executor::drop_plugin_tasks(plugin_id)
}

/// Drop all resources owned by a plugin.
pub fn drop_plugin_resources(plugin_id: u64) -> usize {
    resources::drop_plugin_resources(plugin_id)
}

// =============================================================================
// IO Reactor exports (provided by host, used by tau-rt via extern "C")
// =============================================================================

#[no_mangle]
pub extern "C" fn tau_io_register(fd: RawFd, interest: u8) -> u64 {
    with_reactor(|reactor| {
        match reactor.register(fd, interest) {
            Ok(token) => token as u64,
            Err(e) => {
                eprintln!("[rt] tau_io_register error: {}", e);
                0
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn tau_io_poll_ready(handle: u64, direction: u8, waker: FfiWaker) -> u8 {
    let waker = waker.into_waker();
    with_reactor(|reactor| {
        if reactor.poll_ready(handle as usize, direction, &waker) { 1 } else { 0 }
    })
}

#[no_mangle]
pub extern "C" fn tau_io_clear_ready(handle: u64, direction: u8) {
    with_reactor(|reactor| {
        reactor.clear_ready(handle as usize, direction);
    })
}

#[no_mangle]
pub extern "C" fn tau_io_deregister(handle: u64) {
    with_reactor(|reactor| {
        let _ = reactor.deregister(handle as usize);
    })
}

#[no_mangle]
pub extern "C" fn tau_reactor_notify() {
    with_reactor(|reactor| {
        let _ = reactor.notify();
    })
}

/// Poll the IO reactor for pending events.
pub fn poll_reactor(timeout: Option<Duration>) {
    with_reactor(|reactor| {
        let _ = reactor.poll(timeout);
    })
}

#[no_mangle]
pub extern "C" fn tau_react(millis: u64) {
    poll_reactor(Some(Duration::from_millis(millis)));
}
