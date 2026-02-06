//! Stream Registry — FFI-safe bounded streams with inline ring buffer.
//!
//! Follows the same OnceLock + Mutex + HashMap + u64 handle pattern as events/resources.
//! Each stream is a bounded ring buffer of `capacity × item_size` bytes.
//! Items are copied in/out via `ptr::copy_nonoverlapping` — zero heap allocation per item.
//! The host never interprets the bytes — just manages slots.

use std::alloc::{self, Layout};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::Waker;

use tau_rt::types::FfiWaker;

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Host-side state for a single stream.
///
/// Safety: The buffer pointer is only accessed under the STREAMS mutex.
/// The drop_in_place_fn is a static function pointer (valid for the process lifetime
/// as long as the creating plugin is loaded — see US-008a for unload safety).
struct StreamState {
    /// Flat ring buffer: `capacity × item_size` bytes, aligned to `item_align`.
    buffer: *mut u8,
    /// Layout used to allocate/deallocate the buffer.
    buffer_layout: Layout,
    /// Size of each item in bytes.
    item_size: usize,
    /// Maximum number of items in the ring buffer.
    capacity: usize,
    /// Index of the next item to read (0..capacity).
    head: usize,
    /// Index of the next slot to write (0..capacity).
    tail: usize,
    /// Number of items currently in the buffer.
    count: usize,
    /// Function to call `drop_in_place::<T>()` on an item slot — runs T's destructor
    /// without freeing memory (the ring buffer owns the storage).
    drop_in_place_fn: unsafe extern "C" fn(*mut ()),
    /// Waker for the receiver (set by poll_next when buffer is empty).
    receiver_waker: Option<Waker>,
    /// Waker for the sender (set by poll_flush when buffer is full).
    sender_waker: Option<Waker>,
    /// True once the sender has called close — no more items will be pushed.
    sender_closed: bool,
    /// True once the receiver has been dropped — sender should stop pushing.
    receiver_dropped: bool,
}

impl Drop for StreamState {
    fn drop(&mut self) {
        // Drop all remaining buffered items by calling drop_in_place_fn on each slot.
        for i in 0..self.count {
            let idx = (self.head + i) % self.capacity;
            let slot_ptr = unsafe { self.buffer.add(idx * self.item_size) };
            unsafe { (self.drop_in_place_fn)(slot_ptr as *mut ()) };
        }
        // Deallocate the ring buffer.
        if self.buffer_layout.size() > 0 {
            unsafe { alloc::dealloc(self.buffer, self.buffer_layout) };
        }
    }
}

// Safety: StreamState contains a raw pointer (*mut u8) to the ring buffer,
// which is only accessed under the STREAMS mutex. The drop_in_place_fn is a
// static extern "C" function pointer — safe to send between threads.
unsafe impl Send for StreamState {}

static STREAMS: OnceLock<Mutex<HashMap<u64, StreamState>>> = OnceLock::new();

fn streams() -> &'static Mutex<HashMap<u64, StreamState>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

// =============================================================================
// FFI exports
// =============================================================================

/// Create a new bounded stream with an inline ring buffer.
///
/// - `capacity`: max number of items (min 1)
/// - `item_size`: `size_of::<T>()` in bytes
/// - `item_align`: `align_of::<T>()` in bytes
/// - `drop_in_place_fn`: function pointer to `ptr::drop_in_place::<T>()` — called on
///   each remaining slot during cleanup
///
/// Returns a handle (u64).
#[no_mangle]
pub extern "C" fn tau_stream_create(
    capacity: u32,
    item_size: usize,
    item_align: usize,
    drop_in_place_fn: unsafe extern "C" fn(*mut ()),
) -> u64 {
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    let cap = if capacity == 0 { 1 } else { capacity as usize };

    // Allocate the ring buffer with correct alignment.
    // For ZSTs (item_size == 0), we use a dangling pointer and zero-size layout.
    let (buffer, buffer_layout) = if item_size > 0 {
        let layout = Layout::from_size_align(cap * item_size, item_align)
            .expect("invalid layout for stream ring buffer");
        let ptr = unsafe { alloc::alloc(layout) };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        (ptr, layout)
    } else {
        // ZST: no allocation needed
        (item_align as *mut u8, Layout::from_size_align(0, 1).unwrap())
    };

    let state = StreamState {
        buffer,
        buffer_layout,
        item_size,
        capacity: cap,
        head: 0,
        tail: 0,
        count: 0,
        drop_in_place_fn,
        receiver_waker: None,
        sender_waker: None,
        sender_closed: false,
        receiver_dropped: false,
    };

    streams().lock().unwrap().insert(id, state);
    id
}

/// Push an item into the stream's ring buffer.
///
/// Copies `item_size` bytes from `item_ptr` into the next available slot.
/// The caller must `mem::forget` the original value after a successful push
/// (ownership of the bytes is transferred to the ring buffer).
///
/// Returns: 0 = ok, 1 = full, 2 = closed (receiver dropped or stream not found).
#[no_mangle]
pub extern "C" fn tau_stream_push(handle: u64, item_ptr: *const u8) -> u8 {
    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return 2; // not found → treat as closed
    };

    if state.receiver_dropped {
        return 2; // closed
    }

    if state.count >= state.capacity {
        return 1; // full
    }

    // Copy item bytes into the ring buffer slot at `tail`.
    if state.item_size > 0 {
        let slot_ptr = unsafe { state.buffer.add(state.tail * state.item_size) };
        unsafe {
            std::ptr::copy_nonoverlapping(item_ptr, slot_ptr, state.item_size);
        }
    }
    state.tail = (state.tail + 1) % state.capacity;
    state.count += 1;

    // Wake receiver if it's waiting.
    if let Some(waker) = state.receiver_waker.take() {
        drop(map); // release lock before waking
        waker.wake();
    }

    0 // ok
}

/// Poll for the next item in the stream's ring buffer.
///
/// Copies `item_size` bytes from the next ring buffer slot into `out_ptr`.
/// The caller owns the bytes at `out_ptr` after a successful poll.
///
/// Returns: 0 = pending (waker stored), 1 = ready (bytes written to out_ptr), 2 = done.
#[no_mangle]
pub extern "C" fn tau_stream_poll_next(
    handle: u64,
    waker: FfiWaker,
    out_ptr: *mut u8,
) -> u8 {
    let std_waker = waker.into_waker();

    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return 2; // not found → done
    };

    if state.count > 0 {
        // Copy item bytes from the ring buffer slot at `head` into out_ptr.
        if state.item_size > 0 {
            let slot_ptr = unsafe { state.buffer.add(state.head * state.item_size) };
            unsafe {
                std::ptr::copy_nonoverlapping(slot_ptr, out_ptr, state.item_size);
            }
        }
        state.head = (state.head + 1) % state.capacity;
        state.count -= 1;

        // Wake sender if it was blocked on a full buffer.
        let sender_waker = state.sender_waker.take();
        drop(map);
        if let Some(w) = sender_waker {
            w.wake();
        }

        1 // ready
    } else if state.sender_closed {
        2 // done — no more items and sender closed
    } else {
        // Store waker and return pending.
        state.receiver_waker = Some(std_waker);
        0 // pending
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

    // Wake receiver so it can see the stream is done.
    let receiver_waker = state.receiver_waker.take();

    // If receiver is also gone, clean up the entry entirely.
    let should_remove = state.receiver_dropped;
    if should_remove {
        let state = map.remove(&handle);
        drop(map);
        drop(state); // StreamState::drop runs drop_in_place_fn on remaining items
    } else {
        drop(map);
    }

    if let Some(waker) = receiver_waker {
        waker.wake();
    }
}

/// Drop the receiver side. Signals the sender that the receiver is gone.
/// If both sides are done, the stream state is removed and remaining items are dropped.
#[no_mangle]
pub extern "C" fn tau_stream_drop(handle: u64) {
    let mut map = streams().lock().unwrap();
    let Some(state) = map.get_mut(&handle) else {
        return;
    };

    state.receiver_dropped = true;

    // Wake sender if it was waiting (push will now return 'closed').
    let sender_waker = state.sender_waker.take();

    // If sender is also closed, clean up the entry entirely.
    let should_remove = state.sender_closed;
    if should_remove {
        let state = map.remove(&handle);
        drop(map);
        drop(state); // StreamState::drop runs drop_in_place_fn on remaining items
    } else {
        drop(map);
    }

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

    if state.count < state.capacity {
        return 1; // space available
    }

    state.sender_waker = Some(std_waker);
    0 // pending
}
