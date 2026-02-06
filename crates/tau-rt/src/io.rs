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

    /// Poll the reactor for events (blocking up to timeout).
    /// millis: timeout in milliseconds.
    pub fn tau_react(millis: u64);
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

/// Poll the reactor for events (blocking up to timeout).
pub fn react(timeout: std::time::Duration) {
    unsafe { tau_react(timeout.as_millis() as u64) }
}

// =============================================================================
// make_ffi_waker — convert std Context waker to FfiWaker
// =============================================================================

/// Convert a `Context`'s waker into an `FfiWaker` suitable for the reactor.
///
/// Clones the waker from `cx`, boxes it, and returns an `FfiWaker` whose
/// `wake_fn` unboxes and calls `waker.wake()`. This is the canonical utility —
/// use it instead of duplicating the clone-box-wake pattern.
///
/// The returned `FfiWaker` has proper ownership semantics:
/// - `wake_fn`: unboxes the inner `Waker` and calls `wake()` (consumes)
/// - `clone_fn`: clones the inner `Waker` into a new box (independent copy)
/// - `drop_fn`: frees the inner `Waker` without waking
pub fn make_ffi_waker(cx: &std::task::Context<'_>) -> FfiWaker {
    let waker = cx.waker().clone();
    let data = Box::into_raw(Box::new(waker)) as *mut ();

    extern "C" fn wake_fn(data: *mut ()) {
        let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
        waker.wake();
    }

    extern "C" fn clone_fn(data: *mut ()) -> *mut () {
        let waker = unsafe { &*(data as *const std::task::Waker) };
        Box::into_raw(Box::new(waker.clone())) as *mut ()
    }

    extern "C" fn drop_fn(data: *mut ()) {
        unsafe { drop(Box::from_raw(data as *mut std::task::Waker)) };
    }

    FfiWaker {
        data,
        wake_fn: Some(wake_fn),
        clone_fn: Some(clone_fn),
        drop_fn: Some(drop_fn),
    }
}

// =============================================================================
// AsyncFd — safe async wrapper around a raw file descriptor
// =============================================================================

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A safe wrapper that registers a raw file descriptor with the tau IO reactor
/// and provides async readability/writability polling.
///
/// `AsyncFd` does **not** own the file descriptor. The caller is responsible
/// for opening and closing the fd. `Drop` only deregisters from the reactor.
pub struct AsyncFd {
    fd: RawFd,
    handle: u64,
}

impl AsyncFd {
    /// Register a file descriptor for both READABLE and WRITABLE interest.
    pub fn new(fd: RawFd) -> std::io::Result<Self> {
        Self::with_interest(fd, READABLE | WRITABLE)
    }

    /// Register a file descriptor with specific interest flags.
    pub fn with_interest(fd: RawFd, interest: u8) -> std::io::Result<Self> {
        let handle = register(fd, interest).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "failed to register fd with reactor")
        })?;
        Ok(Self { fd, handle })
    }

    /// Poll for read readiness.
    ///
    /// Returns `Poll::Ready(Ok(()))` if the fd is readable, or `Poll::Pending`
    /// if not (the waker is stored by the reactor).
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let waker = make_ffi_waker(cx);
        if poll_ready(self.handle, DIR_READ, waker) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    /// Poll for write readiness.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let waker = make_ffi_waker(cx);
        if poll_ready(self.handle, DIR_WRITE, waker) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    /// Clear read readiness. Call after getting `WouldBlock`.
    pub fn clear_read_ready(&self) {
        clear_ready(self.handle, DIR_READ);
    }

    /// Clear write readiness. Call after getting `WouldBlock`.
    pub fn clear_write_ready(&self) {
        clear_ready(self.handle, DIR_WRITE);
    }

    /// Async wait for readability.
    pub fn readable(&self) -> ReadableFuture<'_> {
        ReadableFuture { fd: self }
    }

    /// Async wait for writability.
    pub fn writable(&self) -> WritableFuture<'_> {
        WritableFuture { fd: self }
    }

    /// Returns the raw file descriptor (not owned).
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Returns the reactor handle for this registration.
    pub fn handle(&self) -> u64 {
        self.handle
    }
}

impl Drop for AsyncFd {
    fn drop(&mut self) {
        deregister(self.handle);
    }
}

/// Future returned by [`AsyncFd::readable`].
pub struct ReadableFuture<'a> {
    fd: &'a AsyncFd,
}

impl<'a> Future for ReadableFuture<'a> {
    type Output = std::io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.fd.poll_read_ready(cx)
    }
}

/// Future returned by [`AsyncFd::writable`].
pub struct WritableFuture<'a> {
    fd: &'a AsyncFd,
}

impl<'a> Future for WritableFuture<'a> {
    type Output = std::io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.fd.poll_write_ready(cx)
    }
}
