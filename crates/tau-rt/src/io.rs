//! IO Reactor FFI interface.
//!
//! These functions are exported by the host runtime and resolved at plugin load time.

use crate::types::FfiWaker;
use std::os::fd::RawFd;

// Interest flags (must match host's reactor.rs)
pub const READABLE: u8 = 0b01;
pub const WRITABLE: u8 = 0b10;

// Direction flags
pub const DIR_READ: u8 = 0;
pub const DIR_WRITE: u8 = 1;

// FFI declarations — these are exported by the host
extern "C" {
    /// Register a file descriptor with the IO reactor.
    /// Returns a handle (token) for use in other reactor calls.
    /// Returns 0 on error.
    pub fn tau_io_register(fd: RawFd, interest: u8) -> u64;

    /// Poll for IO readiness in a direction.
    /// direction: 0 = read, 1 = write
    /// Returns: 1 if ready, 0 if pending (waker stored)
    pub fn tau_io_poll_ready(handle: u64, direction: u8, waker: FfiWaker) -> u8;

    /// Clear readiness for a direction (call after WouldBlock).
    pub fn tau_io_clear_ready(handle: u64, direction: u8);

    /// Deregister a file descriptor from the reactor.
    pub fn tau_io_deregister(handle: u64);
}

/// Safe wrapper: register a fd with the reactor.
pub fn register(fd: RawFd, interest: u8) -> Option<u64> {
    let handle = unsafe { tau_io_register(fd, interest) };
    if handle == 0 {
        None
    } else {
        Some(handle)
    }
}

/// Safe wrapper: poll for readiness.
/// Returns true if ready, false if pending (waker was stored).
pub fn poll_ready(handle: u64, direction: u8, waker: FfiWaker) -> bool {
    unsafe { tau_io_poll_ready(handle, direction, waker) != 0 }
}

/// Safe wrapper: clear readiness after WouldBlock.
pub fn clear_ready(handle: u64, direction: u8) {
    unsafe { tau_io_clear_ready(handle, direction) }
}

/// Safe wrapper: deregister a fd.
pub fn deregister(handle: u64) {
    unsafe { tau_io_deregister(handle) }
}
