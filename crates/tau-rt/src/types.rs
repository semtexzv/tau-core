//! FFI-safe types shared between host and plugins.

/// Poll result across FFI boundary.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiPoll {
    Ready = 0,
    Pending = 1,
}

/// FFI-safe waker.
///
/// `data` is opaque — either a non-owned value (e.g. task ID cast to pointer)
/// or an owned heap allocation (e.g. a boxed `std::task::Waker`).
///
/// Ownership semantics are determined by the optional `clone_fn` and `drop_fn`:
/// - **Non-owned** (task ID): both `None` — `data` is just a number, no cleanup needed.
/// - **Owned** (heap pointer): both `Some` — `clone_fn` produces an independent copy,
///   `drop_fn` frees the allocation without waking.
///
/// `wake_fn(data)` wakes AND consumes `data` for owned wakers. For non-owned wakers
/// it just pushes the task ID to the ready queue (no ownership transfer).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiWaker {
    pub data: *mut (),
    pub wake_fn: Option<unsafe extern "C" fn(*mut ())>,
    /// Clone the inner `data`, returning a new independent pointer.
    /// `None` for non-owned data (shallow copy is safe).
    pub clone_fn: Option<unsafe extern "C" fn(*mut ()) -> *mut ()>,
    /// Drop the inner `data` without waking.
    /// `None` for non-owned data (nothing to free).
    pub drop_fn: Option<unsafe extern "C" fn(*mut ())>,
}

unsafe impl Send for FfiWaker {}
unsafe impl Sync for FfiWaker {}

impl FfiWaker {
    pub const fn null() -> Self {
        Self {
            data: std::ptr::null_mut(),
            wake_fn: None,
            clone_fn: None,
            drop_fn: None,
        }
    }

    /// Wake the task. For owned data, this consumes the inner allocation.
    pub fn wake(&self) {
        if let Some(f) = self.wake_fn {
            unsafe { f(self.data) };
        }
    }

    /// Convert to a `std::task::Waker`.
    ///
    /// The vtable correctly handles owned data:
    /// - `clone` produces an independent copy via `clone_fn`
    /// - `drop` frees inner data via `drop_fn`
    /// - `wake` (by value) calls `wake_fn` (consumes inner) then frees outer
    /// - `wake_by_ref` clones inner, then calls `wake_fn` on the clone
    pub fn into_waker(self) -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable, Waker};

        let boxed = Box::new(self);
        let data = Box::into_raw(boxed) as *const ();

        static VTABLE: RawWakerVTable = RawWakerVTable::new(
            // clone: deep-clone inner data if clone_fn present
            |data| {
                let ffi = unsafe { &*(data as *const FfiWaker) };
                let cloned_data = match ffi.clone_fn {
                    Some(cf) => unsafe { cf(ffi.data) },
                    None => ffi.data,
                };
                let cloned = Box::new(FfiWaker {
                    data: cloned_data,
                    wake_fn: ffi.wake_fn,
                    clone_fn: ffi.clone_fn,
                    drop_fn: ffi.drop_fn,
                });
                RawWaker::new(Box::into_raw(cloned) as *const (), &VTABLE)
            },
            // wake (by value): wake_fn consumes inner data, then free outer box
            |data| {
                let ffi = unsafe { Box::from_raw(data as *mut FfiWaker) };
                ffi.wake();
                // wake_fn consumed inner data; outer Box<FfiWaker> dropped here
            },
            // wake_by_ref: clone inner data, call wake_fn on the clone
            |data| {
                let ffi = unsafe { &*(data as *const FfiWaker) };
                match ffi.clone_fn {
                    Some(cf) => {
                        // Clone inner data so wake_fn can consume the clone
                        let cloned_data = unsafe { cf(ffi.data) };
                        if let Some(wf) = ffi.wake_fn {
                            unsafe { wf(cloned_data) };
                        }
                    }
                    None => {
                        // Non-owned data: wake_fn doesn't consume, safe to call directly
                        ffi.wake();
                    }
                }
            },
            // drop: free inner data via drop_fn, then free outer Box<FfiWaker>
            |data| {
                let ffi = unsafe { Box::from_raw(data as *mut FfiWaker) };
                if let Some(df) = ffi.drop_fn {
                    unsafe { df(ffi.data) };
                }
                // outer Box<FfiWaker> dropped here
            },
        );

        unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
    }
}

/// Raw poll function type — plugin provides this to poll a type-erased future.
pub type RawPollFn = unsafe extern "C" fn(*mut (), FfiWaker) -> FfiPoll;

/// Raw drop function type — plugin provides this to drop a type-erased future.
pub type RawDropFn = unsafe extern "C" fn(*mut ());
