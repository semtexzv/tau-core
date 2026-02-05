//! Runtime interface - extern declarations resolved at runtime

use crate::types::{FfiPoll, FfiWaker, JoinHandle, RawDropFn, RawPollFn, TaskId};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// =============================================================================
// Extern declarations - these symbols come from utokio-runtime dylib
// =============================================================================

extern "C" {
    /// Initialize the runtime (called by host before loading plugins)
    fn tau_runtime_init() -> i32;

    /// Spawn a new task
    /// - future_ptr: Pointer to a boxed, pinned future
    /// - poll_fn: Function to poll the future
    /// - drop_fn: Function to drop the future when done
    fn tau_spawn(future_ptr: *mut (), poll_fn: RawPollFn, drop_fn: RawDropFn) -> TaskId;

    /// Block on a task until completion (drives the runtime)
    fn tau_block_on(task_id: TaskId);

    /// Poll a specific task, returns Ready/Pending
    fn tau_poll_task(task_id: TaskId, waker: FfiWaker) -> FfiPoll;

    /// Drive the runtime - poll all ready tasks
    fn tau_drive() -> u32;
}

// =============================================================================
// Public API - thin wrappers around extern calls
// =============================================================================

/// Initialize the µTokio runtime
pub fn init() -> Result<(), i32> {
    let result = unsafe { tau_runtime_init() };
    if result == 0 {
        Ok(())
    } else {
        Err(result)
    }
}

/// Spawn a future as a new task
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Box and pin the future
    let boxed: Pin<Box<dyn Future<Output = F::Output> + Send>> = Box::pin(future);

    // Type-erase to raw pointer
    let future_ptr = Box::into_raw(Box::new(boxed)) as *mut ();

    // Poll function that knows the concrete type
    unsafe extern "C" fn poll_fn<T: Send + 'static>(
        future_ptr: *mut (),
        waker: FfiWaker,
    ) -> FfiPoll {
        let future = &mut *(future_ptr as *mut Pin<Box<dyn Future<Output = T> + Send>>);

        // Convert FfiWaker to std::task::Waker
        let waker = ffi_waker_to_std(waker);
        let mut cx = Context::from_waker(&waker);

        match future.as_mut().poll(&mut cx) {
            Poll::Ready(_) => FfiPoll::Ready,
            Poll::Pending => FfiPoll::Pending,
        }
    }

    // Drop function
    unsafe extern "C" fn drop_fn<T: Send + 'static>(future_ptr: *mut ()) {
        drop(Box::from_raw(
            future_ptr as *mut Pin<Box<dyn Future<Output = T> + Send>>,
        ));
    }

    let task_id = unsafe { tau_spawn(future_ptr, poll_fn::<F::Output>, drop_fn::<F::Output>) };

    JoinHandle::new(task_id)
}

/// Block on a future until completion
pub fn block_on<F: Future>(future: F) -> F::Output {
    // For simplicity, we box the future and use a sync wrapper
    // A real implementation would integrate properly with the runtime

    let mut future = Box::pin(future);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                // Drive the runtime
                unsafe { tau_drive() };
            }
        }
    }
}

// =============================================================================
// Waker utilities
// =============================================================================

#[doc(hidden)]
#[no_mangle]
pub fn ffi_waker_to_std(ffi: FfiWaker) -> Waker {
    let boxed = Box::new(ffi);
    let ptr = Box::into_raw(boxed) as *const ();

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |ptr| {
            let ffi = unsafe { &*(ptr as *const FfiWaker) };
            let cloned = Box::new(*ffi);
            RawWaker::new(Box::into_raw(cloned) as *const (), &VTABLE)
        },
        |ptr| {
            let ffi = unsafe { Box::from_raw(ptr as *mut FfiWaker) };
            ffi.wake();
        },
        |ptr| {
            let ffi = unsafe { &*(ptr as *const FfiWaker) };
            ffi.wake();
        },
        |ptr| {
            unsafe { drop(Box::from_raw(ptr as *mut FfiWaker)) };
        },
    );

    let raw = RawWaker::new(ptr, &VTABLE);
    unsafe { Waker::from_raw(raw) }
}

#[doc(hidden)]
#[no_mangle]
pub fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let raw = RawWaker::new(std::ptr::null(), &VTABLE);
    unsafe { Waker::from_raw(raw) }
}
