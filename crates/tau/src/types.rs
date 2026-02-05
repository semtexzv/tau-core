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
/// `data` is opaque (typically a task ID cast to pointer).
/// `wake_fn(data)` pushes the task back to the executor's ready queue.
///
/// This is Copy because data is not owned — it's just a number (task ID).
/// The host's wake function uses data as a task ID, not as a real pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiWaker {
    pub data: *mut (),
    pub wake_fn: Option<unsafe extern "C" fn(*mut ())>,
}

unsafe impl Send for FfiWaker {}
unsafe impl Sync for FfiWaker {}

impl FfiWaker {
    pub const fn null() -> Self {
        Self {
            data: std::ptr::null_mut(),
            wake_fn: None,
        }
    }

    pub fn wake(&self) {
        if let Some(f) = self.wake_fn {
            unsafe { f(self.data) };
        }
    }

    /// Convert to a std::task::Waker.
    /// The FfiWaker is copied into the waker's data pointer.
    pub fn into_waker(self) -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable, Waker};

        // Store FfiWaker in a Box, use the box pointer as waker data
        let boxed = Box::new(self);
        let data = Box::into_raw(boxed) as *const ();

        static VTABLE: RawWakerVTable = RawWakerVTable::new(
            // clone
            |data| {
                let ffi = unsafe { &*(data as *const FfiWaker) };
                let cloned = Box::new(*ffi);
                RawWaker::new(Box::into_raw(cloned) as *const (), &VTABLE)
            },
            // wake (by value)
            |data| {
                let ffi = unsafe { Box::from_raw(data as *mut FfiWaker) };
                ffi.wake();
            },
            // wake_by_ref
            |data| {
                let ffi = unsafe { &*(data as *const FfiWaker) };
                ffi.wake();
            },
            // drop
            |data| {
                unsafe { drop(Box::from_raw(data as *mut FfiWaker)) };
            },
        );

        unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
    }
}

/// Raw poll function type — plugin provides this to poll a type-erased future.
pub type RawPollFn = unsafe extern "C" fn(*mut (), FfiWaker) -> FfiPoll;

/// Raw drop function type — plugin provides this to drop a type-erased future.
pub type RawDropFn = unsafe extern "C" fn(*mut ());
