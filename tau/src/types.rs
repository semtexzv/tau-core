//! FFI-safe types for µTokio

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Task identifier
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

/// Poll result across FFI
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiPoll {
    Ready = 0,
    Pending = 1,
}

/// FFI-safe waker
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
}

/// Type-erased poll function/// Raw poll function type
pub type RawPollFn = unsafe extern "C" fn(*mut (), FfiWaker) -> FfiPoll;

/// Raw drop function type
pub type RawDropFn = unsafe extern "C" fn(*mut ());

/// Join handle returned by spawn
pub struct JoinHandle<T> {
    task_id: TaskId,
    _marker: std::marker::PhantomData<T>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }
}

// JoinHandle is a future that waits for task completion
impl<T: 'static> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // This would call into runtime to check task status
        // For now, simplified - real impl would store result
        todo!("JoinHandle::poll requires runtime support")
    }
}
