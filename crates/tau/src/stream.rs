//! Async stream primitives.
//!
//! Re-exports the standard [`Stream`] trait from `futures-core` so that plugins
//! use the exact same trait the wider async ecosystem depends on (hyper, tower,
//! tonic, kube, reqwest, etc.).
//!
//! Provides FFI-safe [`StreamSender`] / [`StreamReceiver`] types for
//! producing and consuming byte streams across the plugin boundary, and a
//! [`channel`] constructor.

pub use futures_core::Stream;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tau_rt::types::FfiWaker;

// =============================================================================
// FFI declarations — symbols provided by the host binary
// =============================================================================

extern "C" {
    fn tau_stream_create(capacity: u32) -> u64;
    fn tau_stream_push(handle: u64, data_ptr: *const u8, data_len: usize) -> u8;
    fn tau_stream_poll_next(
        handle: u64,
        waker: FfiWaker,
        out_ptr: *mut *const u8,
        out_len: *mut usize,
    ) -> u8;
    fn tau_stream_close(handle: u64);
    fn tau_stream_drop(handle: u64);
    fn tau_stream_free_item(ptr: *const u8, len: usize);
    fn tau_stream_poll_flush(handle: u64, waker: FfiWaker) -> u8;
}

// =============================================================================
// Waker conversion: std Context → FfiWaker
// =============================================================================

fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
    let waker = cx.waker().clone();
    let boxed = Box::new(waker);
    let data = Box::into_raw(boxed) as *mut ();

    extern "C" fn wake_fn(data: *mut ()) {
        let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
        waker.wake();
    }

    FfiWaker {
        data,
        wake_fn: Some(wake_fn),
    }
}

// =============================================================================
// StreamSender
// =============================================================================

/// Sender half of an FFI-safe byte stream.
///
/// Push bytes into the stream with [`push`](StreamSender::push) (non-blocking)
/// or [`send`](StreamSender::send) (async, waits if full). Call
/// [`close`](StreamSender::close) to signal completion.
pub struct StreamSender {
    handle: u64,
}

// Safety: handle is just a u64 key into a global Mutex-protected map
unsafe impl Send for StreamSender {}
unsafe impl Sync for StreamSender {}

/// Result of a non-blocking push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// Item was accepted.
    Ok,
    /// Buffer is full — try again later or use `send()`.
    Full,
    /// Stream is closed (receiver dropped).
    Closed,
}

impl StreamSender {
    /// Push data into the stream (non-blocking).
    pub fn push(&self, data: &[u8]) -> PushResult {
        let result = unsafe {
            tau_stream_push(self.handle, data.as_ptr(), data.len())
        };
        match result {
            0 => PushResult::Ok,
            1 => PushResult::Full,
            _ => PushResult::Closed,
        }
    }

    /// Send data into the stream, waiting asynchronously if the buffer is full.
    /// Returns `false` if the stream was closed (receiver dropped).
    pub async fn send(&self, data: &[u8]) -> bool {
        // First try a non-blocking push
        match self.push(data) {
            PushResult::Ok => return true,
            PushResult::Closed => return false,
            PushResult::Full => {}
        }

        // Wait for space, then retry
        let handle = self.handle;
        let data = data.to_vec(); // need to own for the async boundary

        SendFuture {
            handle,
            data,
        }
        .await
    }

    /// Close the sender — no more items will be pushed.
    /// The receiver will get remaining buffered items, then `None`.
    pub fn close(self) {
        unsafe { tau_stream_close(self.handle) };
        // Don't run Drop (which would also close)
        std::mem::forget(self);
    }
}

impl Drop for StreamSender {
    fn drop(&mut self) {
        unsafe { tau_stream_close(self.handle) };
    }
}

/// Future that waits for space in the stream buffer, then pushes data.
struct SendFuture {
    handle: u64,
    data: Vec<u8>,
}

impl Future for SendFuture {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let this = self.get_mut();
        let ffi_waker = make_ffi_waker(cx);

        // Check if there's space
        let status = unsafe { tau_stream_poll_flush(this.handle, ffi_waker) };
        match status {
            0 => Poll::Pending, // waker stored, will be woken when space available
            2 => Poll::Ready(false), // closed
            _ => {
                // Space available (status=1), try push
                let result = unsafe {
                    tau_stream_push(this.handle, this.data.as_ptr(), this.data.len())
                };
                match result {
                    0 => Poll::Ready(true),
                    1 => {
                        // Race: full again. Re-register waker.
                        let ffi_waker = make_ffi_waker(cx);
                        let _ = unsafe { tau_stream_poll_flush(this.handle, ffi_waker) };
                        Poll::Pending
                    }
                    _ => Poll::Ready(false), // closed
                }
            }
        }
    }
}

// =============================================================================
// StreamReceiver
// =============================================================================

/// Receiver half of an FFI-safe byte stream.
///
/// Implements [`Stream<Item = Vec<u8>>`](Stream) — poll it to receive items.
/// Dropping the receiver cancels the stream (sender will see `Closed`).
pub struct StreamReceiver {
    handle: u64,
}

// Safety: handle is just a u64 key into a global Mutex-protected map
unsafe impl Send for StreamReceiver {}
unsafe impl Sync for StreamReceiver {}

impl Stream for StreamReceiver {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let handle = self.handle;
        let ffi_waker = make_ffi_waker(cx);

        let mut out_ptr: *const u8 = std::ptr::null();
        let mut out_len: usize = 0;

        let status = unsafe {
            tau_stream_poll_next(handle, ffi_waker, &mut out_ptr, &mut out_len)
        };

        match status {
            0 => Poll::Pending,
            1 => {
                // Ready — take ownership of the allocated buffer
                let data = if out_len > 0 && !out_ptr.is_null() {
                    let vec = unsafe {
                        std::slice::from_raw_parts(out_ptr, out_len).to_vec()
                    };
                    // Free the host-allocated buffer
                    unsafe { tau_stream_free_item(out_ptr, out_len) };
                    vec
                } else {
                    // Free even zero-length allocations if pointer is non-null
                    if !out_ptr.is_null() {
                        unsafe { tau_stream_free_item(out_ptr, out_len) };
                    }
                    Vec::new()
                };
                Poll::Ready(Some(data))
            }
            _ => Poll::Ready(None), // done
        }
    }
}

impl Drop for StreamReceiver {
    fn drop(&mut self) {
        unsafe { tau_stream_drop(self.handle) };
    }
}

// =============================================================================
// Channel constructor
// =============================================================================

/// Create a bounded byte stream channel.
///
/// Returns a `(StreamSender, StreamReceiver)` pair. The sender can push up to
/// `capacity` items before blocking (or returning `Full`). The receiver
/// implements [`Stream<Item = Vec<u8>>`](Stream).
///
/// # Example
///
/// ```rust,ignore
/// let (tx, mut rx) = tau::stream::channel(8);
/// tx.push(b"hello");
/// tx.close();
/// // rx.next().await == Some(b"hello".to_vec())
/// // rx.next().await == None
/// ```
pub fn channel(capacity: u32) -> (StreamSender, StreamReceiver) {
    let handle = unsafe { tau_stream_create(capacity) };
    (
        StreamSender { handle },
        StreamReceiver { handle },
    )
}
