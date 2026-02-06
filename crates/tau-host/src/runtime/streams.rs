//! Stream Registry — FFI-safe bounded streams with push/poll/close.
//!
//! Follows the same OnceLock + Mutex + HashMap + u64 handle pattern as events/resources.
//! Each stream is a bounded queue of byte buffers with sender/receiver wakers.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::Waker;

use tau_rt::types::FfiWaker;

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

struct StreamState {
    /// Buffered items (byte vectors).
    buffer: VecDeque<Vec<u8>>,
    /// Maximum number of items in the buffer.
    capacity: usize,
    /// Waker for the receiver (set by poll_next when buffer is empty).
    receiver_waker: Option<Waker>,
    /// Waker for the sender (set by push when buffer is full).
    sender_waker: Option<Waker>,
    /// True once the sender has called close — no more items will be pushed.
    sender_closed: bool,
    /// True once the receiver has been dropped — sender should stop pushing.
    receiver_dropped: bool,
}

static STREAMS: OnceLock<Mutex<HashMap<u64, StreamState>>> = OnceLock::new();

fn streams() -> &'static Mutex<HashMap<u64, StreamState>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

// =============================================================================
// FFI exports
// =============================================================================

/// Create a new bounded stream. Returns a handle.
#[no_mangle]
pub extern "C" fn tau_stream_create(capacity: u32) -> u64 {
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    let cap = if capacity == 0 { 1 } else { capacity as usize }; // min capacity 1

    let state = StreamState {
        buffer: VecDeque::new(),
        capacity: cap,
        receiver_waker: None,
        sender_waker: None,
        sender_closed: false,
        receiver_dropped: false,
    };

    streams().lock().unwrap().insert(id, state);
    id
}

/// Push an item to the stream.
/// Returns: 0 = ok, 1 = full, 2 = closed (receiver dropped or stream not found).
#[no_mangle]
pub extern "C" fn tau_stream_push(handle: u64, data_ptr: *const u8, data_len: usize) -> u8 {
    let data = if data_len > 0 && !data_ptr.is_null() {
        unsafe { std::slice::from_raw_parts(data_ptr, data_len) }.to_vec()
    } else {
        Vec::new()
    };

    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return 2; // not found → treat as closed
    };

    if state.receiver_dropped {
        return 2; // closed
    }

    if state.buffer.len() >= state.capacity {
        return 1; // full
    }

    state.buffer.push_back(data);

    // Wake receiver if it's waiting
    if let Some(waker) = state.receiver_waker.take() {
        drop(map); // release lock before waking
        waker.wake();
    }

    0 // ok
}

/// Poll for the next item in the stream.
/// Returns: 0 = pending (waker stored), 1 = ready (out_ptr/out_len set), 2 = done (stream closed and empty).
///
/// When returning 1 (ready), the caller owns the allocated buffer at *out_ptr with length *out_len.
/// The caller must free it with `tau_stream_free_item`.
#[no_mangle]
pub extern "C" fn tau_stream_poll_next(
    handle: u64,
    waker: FfiWaker,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> u8 {
    let std_waker = waker.into_waker();

    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return 2; // not found → done
    };

    if let Some(item) = state.buffer.pop_front() {
        // Wake sender if it was blocked on a full buffer
        let sender_waker = state.sender_waker.take();

        // Write out the item
        let len = item.len();
        let ptr = if len > 0 {
            let boxed = item.into_boxed_slice();
            Box::into_raw(boxed) as *const u8
        } else {
            std::ptr::null()
        };

        unsafe {
            *out_ptr = ptr;
            *out_len = len;
        }

        drop(map);
        if let Some(w) = sender_waker {
            w.wake();
        }

        1 // ready
    } else if state.sender_closed {
        2 // done — no more items and sender closed
    } else {
        // Store waker and return pending
        state.receiver_waker = Some(std_waker);
        0 // pending
    }
}

/// Free an item buffer returned by `tau_stream_poll_next`.
#[no_mangle]
pub extern "C" fn tau_stream_free_item(ptr: *const u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        unsafe {
            let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, len);
            drop(Box::from_raw(slice as *mut [u8]));
        }
    }
}

/// Close the sender side — no more items will be pushed.
/// The receiver will get remaining buffered items, then `done`.
#[no_mangle]
pub extern "C" fn tau_stream_close(handle: u64) {
    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return;
    };

    state.sender_closed = true;

    // Wake receiver so it can see the stream is done
    let receiver_waker = state.receiver_waker.take();

    // If receiver is also gone, clean up the entry entirely
    if state.receiver_dropped {
        map.remove(&handle);
    }

    drop(map);
    if let Some(waker) = receiver_waker {
        waker.wake();
    }
}

/// Drop the stream (receiver side). Signals the sender that the receiver is gone.
/// If both sides are done (sender closed + receiver dropped), the stream state is removed.
#[no_mangle]
pub extern "C" fn tau_stream_drop(handle: u64) {
    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return;
    };

    state.receiver_dropped = true;

    // Wake sender if it was waiting (push will now return 'closed')
    let sender_waker = state.sender_waker.take();

    // If sender is also closed, clean up the entry entirely
    if state.sender_closed {
        map.remove(&handle);
    }

    drop(map);
    if let Some(w) = sender_waker {
        w.wake();
    }
}

/// Store a sender waker (for async send when buffer is full).
/// Returns: 0 = stored (pending), 1 = space available (retry push), 2 = closed.
#[no_mangle]
pub extern "C" fn tau_stream_poll_flush(handle: u64, waker: FfiWaker) -> u8 {
    let std_waker = waker.into_waker();

    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return 2; // not found → closed
    };

    if state.receiver_dropped {
        return 2; // closed
    }

    if state.buffer.len() < state.capacity {
        return 1; // space available
    }

    state.sender_waker = Some(std_waker);
    0 // pending
}
