//! Async stream primitives.
//!
//! Re-exports the standard [`Stream`] trait from `futures-core` so that plugins
//! use the exact same trait the wider async ecosystem depends on (hyper, tower,
//! tonic, kube, reqwest, etc.).
//!
//! Provides typed [`StreamSender<T>`] / [`StreamReceiver<T>`] for producing and
//! consuming `Stream<Item = T>` across the FFI boundary. Items are stored in an
//! inline ring buffer on the host side — no per-item heap allocation, no
//! serialization. A Rust move (`ptr::copy_nonoverlapping`) transfers ownership.

pub use futures_core::Stream;

use std::future::Future;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::pin::Pin;
use std::task::{Context, Poll};

use tau_rt::types::FfiWaker;

// =============================================================================
// FFI declarations — symbols provided by the host binary
// =============================================================================

extern "C" {
    fn tau_stream_create(
        capacity: u32,
        item_size: usize,
        item_align: usize,
        drop_in_place_fn: unsafe extern "C" fn(*mut ()),
    ) -> u64;
    fn tau_stream_push(handle: u64, item_ptr: *const u8) -> u8;
    fn tau_stream_poll_next(handle: u64, waker: FfiWaker, out_ptr: *mut u8) -> u8;
    fn tau_stream_close(handle: u64);
    fn tau_stream_drop(handle: u64);
    fn tau_stream_poll_flush(handle: u64, waker: FfiWaker) -> u8;
}

// =============================================================================
// Monomorphized drop_in_place — one per T, passed to host at stream creation
// =============================================================================

/// Calls `ptr::drop_in_place::<T>()` on the given pointer.
/// Runs T's destructor without freeing memory (the ring buffer owns the storage).
unsafe extern "C" fn drop_in_place<T>(ptr: *mut ()) {
    std::ptr::drop_in_place(ptr as *mut T);
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
// StreamSender<T>
// =============================================================================

/// Sender half of a typed FFI stream.
///
/// Push items with [`push`](StreamSender::push) (non-blocking) or
/// [`send`](StreamSender::send) (async, waits if full). Call
/// [`close`](StreamSender::close) to signal completion.
pub struct StreamSender<T: Send + 'static> {
    handle: u64,
    _marker: PhantomData<T>,
}

// Safety: handle is just a u64 key into a global Mutex-protected map.
unsafe impl<T: Send + 'static> Send for StreamSender<T> {}
unsafe impl<T: Send + 'static> Sync for StreamSender<T> {}

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

impl<T: Send + 'static> StreamSender<T> {
    /// Push an item into the stream (non-blocking).
    ///
    /// On success, ownership is transferred to the ring buffer (the value is
    /// `mem::forget`-ed). On failure (`Full` or `Closed`), the value is
    /// returned to the caller via the `PushResult`.
    pub fn push(&self, value: T) -> Result<(), (PushResult, T)> {
        let item_ptr = &value as *const T as *const u8;
        let result = unsafe { tau_stream_push(self.handle, item_ptr) };
        match result {
            0 => {
                // Ownership transferred to ring buffer — don't drop the original.
                mem::forget(value);
                Ok(())
            }
            1 => Err((PushResult::Full, value)),
            _ => Err((PushResult::Closed, value)),
        }
    }

    /// Send an item into the stream, waiting asynchronously if the buffer is full.
    /// Returns `true` on success, `false` if the stream was closed (receiver dropped).
    pub async fn send(&self, value: T) -> bool {
        // First try a non-blocking push
        let value = match self.push(value) {
            Ok(()) => return true,
            Err((PushResult::Closed, _)) => return false,
            Err((PushResult::Full, v)) => v,
            Err((PushResult::Ok, _)) => unreachable!(),
        };

        SendFuture {
            handle: self.handle,
            value: Some(value),
        }
        .await
    }

    /// Close the sender — no more items will be pushed.
    /// The receiver will get remaining buffered items, then `None`.
    pub fn close(self) {
        unsafe { tau_stream_close(self.handle) };
        // Don't run Drop (which would also close).
        mem::forget(self);
    }
}

impl<T: Send + 'static> Drop for StreamSender<T> {
    fn drop(&mut self) {
        unsafe { tau_stream_close(self.handle) };
    }
}

/// Future that waits for space in the stream buffer, then pushes.
struct SendFuture<T: Send + 'static> {
    handle: u64,
    value: Option<T>,
}

impl<T: Send + 'static> Future for SendFuture<T> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let this = unsafe { self.get_unchecked_mut() };
        let ffi_waker = make_ffi_waker(cx);

        // Check if there's space.
        let status = unsafe { tau_stream_poll_flush(this.handle, ffi_waker) };
        match status {
            0 => Poll::Pending, // waker stored, will be woken when space available
            2 => Poll::Ready(false), // closed
            _ => {
                // Space available (status=1), try push.
                let value = this.value.take().expect("SendFuture polled after completion");
                let item_ptr = &value as *const T as *const u8;
                let result = unsafe { tau_stream_push(this.handle, item_ptr) };
                match result {
                    0 => {
                        mem::forget(value);
                        Poll::Ready(true)
                    }
                    1 => {
                        // Race: full again. Store value back and re-register waker.
                        this.value = Some(value);
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
// StreamReceiver<T>
// =============================================================================

/// Receiver half of a typed FFI stream.
///
/// Implements [`Stream<Item = T>`](Stream) — poll it to receive items.
/// Dropping the receiver cancels the stream (sender will see `Closed`).
pub struct StreamReceiver<T: Send + 'static> {
    handle: u64,
    _marker: PhantomData<T>,
}

// Safety: handle is just a u64 key into a global Mutex-protected map.
unsafe impl<T: Send + 'static> Send for StreamReceiver<T> {}
unsafe impl<T: Send + 'static> Sync for StreamReceiver<T> {}

impl<T: Send + 'static> Stream for StreamReceiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let handle = self.handle;
        let ffi_waker = make_ffi_waker(cx);

        // Use MaybeUninit as the output slot — the host copies item_size bytes into it.
        let mut slot = MaybeUninit::<T>::uninit();
        let out_ptr = slot.as_mut_ptr() as *mut u8;

        let status = unsafe { tau_stream_poll_next(handle, ffi_waker, out_ptr) };

        match status {
            0 => Poll::Pending,
            1 => {
                // Ready — the host copied the item bytes into our slot.
                let value = unsafe { slot.assume_init() };
                Poll::Ready(Some(value))
            }
            _ => Poll::Ready(None), // done
        }
    }
}

impl<T: Send + 'static> Drop for StreamReceiver<T> {
    fn drop(&mut self) {
        unsafe { tau_stream_drop(self.handle) };
    }
}

// =============================================================================
// Channel constructor
// =============================================================================

/// Create a bounded typed stream channel.
///
/// Returns a `(StreamSender<T>, StreamReceiver<T>)` pair. The sender can push
/// up to `capacity` items before blocking (or returning `Full`). The receiver
/// implements [`Stream<Item = T>`](Stream).
///
/// Items are stored in an inline ring buffer — no per-item heap allocation.
/// A Rust move (`ptr::copy_nonoverlapping`) transfers ownership across the FFI
/// boundary.
///
/// # Example
///
/// ```rust,ignore
/// let (tx, mut rx) = tau::stream::channel::<String>(8);
/// tx.push("hello".to_string()).ok();
/// tx.close();
/// // rx.next().await == Some("hello".to_string())
/// // rx.next().await == None
/// ```
pub fn channel<T: Send + 'static>(capacity: u32) -> (StreamSender<T>, StreamReceiver<T>) {
    let handle = unsafe {
        tau_stream_create(
            capacity,
            mem::size_of::<T>(),
            mem::align_of::<T>(),
            drop_in_place::<T>,
        )
    };
    (
        StreamSender {
            handle,
            _marker: PhantomData,
        },
        StreamReceiver {
            handle,
            _marker: PhantomData,
        },
    )
}
