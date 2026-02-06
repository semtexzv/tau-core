//! Runtime interface — extern declarations resolved at load time from host.
//!
//! Plugins call these wrappers. The actual implementations live in the host binary
//! and are resolved via dynamic linking (-Wl,-undefined,dynamic_lookup + -rdynamic).
//!
//! # Cross-plugin safety of spawn / JoinHandle
//!
//! A `JoinHandle<T>` must NOT be sent to another plugin. It contains function
//! pointers (the `TaskVtable`) that point into the spawning plugin's `.text`
//! section. If the spawning plugin is unloaded, those pointers dangle.
//!
//! To get a result from Plugin A to Plugin B, spawn the work in Plugin A and
//! send the **concrete result `T`** through a channel or resource. The `T` itself
//! is safe to send — every plugin has its own copy of `T`'s drop glue and the
//! same allocator. Only the dynamic dispatch wrappers (vtable, poll_fn) are unsafe
//! to share.

use crate::types::{FfiPoll, FfiWaker, RawDropFn, RawPollFn};
use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

// =============================================================================
// Extern declarations — symbols provided by the host binary
// =============================================================================

extern "C" {
    fn tau_runtime_init() -> i32;
    fn tau_spawn(future_ptr: *mut (), poll_fn: RawPollFn, drop_fn: RawDropFn) -> u64;
    #[allow(dead_code)]
    fn tau_block_on(task_id: u64);
    fn tau_drive() -> u32;
    fn tau_timer_create(nanos_from_now: u64) -> u64;
    fn tau_timer_check(handle: u64) -> u8;
    fn tau_timer_cancel(handle: u64);
    fn tau_task_abort(task_id: u64) -> u8;
}

// =============================================================================
// Public API
// =============================================================================

/// Initialize the runtime.
pub fn init() -> Result<(), i32> {
    let result = unsafe { tau_runtime_init() };
    if result == 0 { Ok(()) } else { Err(result) }
}

/// Drive the runtime (poll ready tasks, fire timers). Returns tasks completed.
pub fn drive() -> u32 {
    unsafe { tau_drive() }
}

// =============================================================================
// Task Cell - reference counted, stores result inline (like tokio)
// =============================================================================

/// The stage of task execution.
enum Stage<T> {
    Running,
    Finished(T),
    Aborted,
    Consumed,
}

/// Task cell - reference counted, contains result slot.
/// Shared between JoinHandle and the running task.
struct TaskCell<T> {
    /// Reference count (JoinHandle + running task).
    ref_count: AtomicUsize,
    /// The result stage.
    stage: UnsafeCell<Stage<T>>,
    /// Waker to notify when complete.
    waker: UnsafeCell<Option<Waker>>,
    /// Whether the task is complete (for fast check without locking).
    complete: std::sync::atomic::AtomicBool,
}

impl<T> TaskCell<T> {
    fn new() -> *mut Self {
        Box::into_raw(Box::new(TaskCell {
            ref_count: AtomicUsize::new(2), // JoinHandle + WrapperFuture
            stage: UnsafeCell::new(Stage::Running),
            waker: UnsafeCell::new(None),
            complete: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Decrement ref count. Returns true if this was the last ref.
    unsafe fn dec_ref(ptr: *mut Self) -> bool {
        if (*ptr).ref_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            true
        } else {
            false
        }
    }

    unsafe fn drop_cell(ptr: *mut Self) {
        drop(Box::from_raw(ptr));
    }
}

/// Vtable for type-erased task operations.
struct TaskVtable {
    try_read_output: unsafe fn(NonNull<()>, *mut ()),
    set_waker: unsafe fn(NonNull<()>, Option<Waker>),
    is_complete: unsafe fn(NonNull<()>) -> bool,
    dec_ref: unsafe fn(NonNull<()>) -> bool,
    drop_cell: unsafe fn(NonNull<()>),
}

/// Raw task handle - pointer + vtable, no T stored.
struct RawTask {
    ptr: NonNull<()>,
    vtable: &'static TaskVtable,
}

impl Clone for RawTask {
    fn clone(&self) -> Self {
        RawTask { ptr: self.ptr, vtable: self.vtable }
    }
}
impl Copy for RawTask {}

// Monomorphized vtable functions
unsafe fn try_read_output<T>(cell_ptr: NonNull<()>, dst: *mut ()) {
    let cell = cell_ptr.as_ptr() as *mut TaskCell<T>;
    let stage = &mut *(*cell).stage.get();
    let out = &mut *(dst as *mut Poll<Option<T>>);
    
    match std::mem::replace(stage, Stage::Consumed) {
        Stage::Finished(result) => *out = Poll::Ready(Some(result)),
        Stage::Aborted => *out = Poll::Ready(None),
        Stage::Running => {
            *stage = Stage::Running;
            *out = Poll::Pending;
        }
        Stage::Consumed => panic!("JoinHandle polled after completion"),
    }
}

unsafe fn set_waker<T>(cell_ptr: NonNull<()>, waker: Option<Waker>) {
    let cell = cell_ptr.as_ptr() as *mut TaskCell<T>;
    *(*cell).waker.get() = waker;
}

unsafe fn is_complete<T>(cell_ptr: NonNull<()>) -> bool {
    let cell = cell_ptr.as_ptr() as *mut TaskCell<T>;
    (*cell).complete.load(Ordering::Acquire)
}

unsafe fn dec_ref<T>(cell_ptr: NonNull<()>) -> bool {
    TaskCell::<T>::dec_ref(cell_ptr.as_ptr() as *mut TaskCell<T>)
}

unsafe fn drop_cell<T>(cell_ptr: NonNull<()>) {
    TaskCell::<T>::drop_cell(cell_ptr.as_ptr() as *mut TaskCell<T>);
}

fn make_vtable<T>() -> &'static TaskVtable {
    &TaskVtable {
        try_read_output: try_read_output::<T>,
        set_waker: set_waker::<T>,
        is_complete: is_complete::<T>,
        dec_ref: dec_ref::<T>,
        drop_cell: drop_cell::<T>,
    }
}

// =============================================================================
// JoinHandle - stores raw pointer + PhantomData (like tokio)
// =============================================================================

/// Handle returned by `spawn`. Implements Future to await the task's result.
/// 
/// Like tokio, T is NOT stored directly - only via PhantomData.
/// The actual T lives in the TaskCell, accessed via vtable.
pub struct JoinHandle<T> {
    task_id: u64,
    raw: RawTask,
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}
impl<T> Unpin for JoinHandle<T> {}  // T is behind a pointer, not inline

impl<T> JoinHandle<T> {
    pub fn task_id(&self) -> u64 {
        self.task_id
    }

    pub fn abort(&self) {
        unsafe { tau_task_abort(self.task_id) };
    }

    pub fn is_finished(&self) -> bool {
        unsafe { (self.raw.vtable.is_complete)(self.raw.ptr) }
    }
}

// Future impl - NO 'static bound on T!
impl<T> Future for JoinHandle<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if !unsafe { (self.raw.vtable.is_complete)(self.raw.ptr) } {
            unsafe { (self.raw.vtable.set_waker)(self.raw.ptr, Some(cx.waker().clone())) };
            return Poll::Pending;
        }
        
        let mut result: Poll<Option<T>> = Poll::Pending;
        unsafe {
            (self.raw.vtable.try_read_output)(
                self.raw.ptr,
                &mut result as *mut Poll<Option<T>> as *mut (),
            );
        }
        result
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        unsafe {
            if (self.raw.vtable.dec_ref)(self.raw.ptr) {
                (self.raw.vtable.drop_cell)(self.raw.ptr);
            }
        }
    }
}

// =============================================================================
// spawn
// =============================================================================

/// Spawn a future as a task on the shared runtime. Returns a JoinHandle.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Create ref-counted task cell (starts with count = 2)
    let cell_ptr = TaskCell::<F::Output>::new();
    
    let raw = RawTask {
        ptr: unsafe { NonNull::new_unchecked(cell_ptr as *mut ()) },
        vtable: make_vtable::<F::Output>(),
    };
    
    // Create wrapper future that writes result to cell
    let wrapper = WrapperFuture {
        future,
        cell_ptr,
        vtable: raw.vtable,
    };

    // Type-erase for FFI
    let boxed: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(wrapper);
    let future_ptr = Box::into_raw(Box::new(boxed)) as *mut ();

    unsafe extern "C" fn poll_erased(future_ptr: *mut (), waker: FfiWaker) -> FfiPoll {
        let future = &mut *(future_ptr as *mut Pin<Box<dyn Future<Output = ()> + Send>>);
        let waker = ffi_waker_to_std(waker);
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => FfiPoll::Ready,
            Poll::Pending => FfiPoll::Pending,
        }
    }

    unsafe extern "C" fn drop_erased(future_ptr: *mut ()) {
        drop(Box::from_raw(
            future_ptr as *mut Pin<Box<dyn Future<Output = ()> + Send>>,
        ));
    }

    let task_id = unsafe { tau_spawn(future_ptr, poll_erased, drop_erased) };
    
    JoinHandle {
        task_id,
        raw,
        _marker: PhantomData,
    }
}

/// Wrapper future that writes result to TaskCell when complete.
struct WrapperFuture<F: Future> {
    future: F,
    cell_ptr: *mut TaskCell<F::Output>,
    vtable: &'static TaskVtable,
}

unsafe impl<F: Future + Send> Send for WrapperFuture<F> where F::Output: Send {}

impl<F: Future> Future for WrapperFuture<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = unsafe { self.get_unchecked_mut() };
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        
        match future.poll(cx) {
            Poll::Ready(output) => {
                unsafe {
                    let cell = &*this.cell_ptr;
                    *cell.stage.get() = Stage::Finished(output);
                    cell.complete.store(true, Ordering::Release);
                    
                    if let Some(waker) = (*cell.waker.get()).take() {
                        waker.wake();
                    }
                }
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F: Future> Drop for WrapperFuture<F> {
    fn drop(&mut self) {
        unsafe {
            let cell = &*self.cell_ptr;
            // If the task is still running when the wrapper is dropped,
            // it means the task was aborted. Mark it as such and wake the JoinHandle.
            let stage = &mut *cell.stage.get();
            if matches!(stage, Stage::Running) {
                *stage = Stage::Aborted;
                cell.complete.store(true, Ordering::Release);
                if let Some(waker) = (*cell.waker.get()).take() {
                    waker.wake();
                }
            }

            // Decrement ref count when wrapper is dropped
            if (self.vtable.dec_ref)(NonNull::new_unchecked(self.cell_ptr as *mut ())) {
                (self.vtable.drop_cell)(NonNull::new_unchecked(self.cell_ptr as *mut ()));
            }
        }
    }
}

// =============================================================================
// block_on
// =============================================================================

/// Block the current thread until a future completes, driving the runtime.
pub fn block_on<F: Future + 'static>(future: F) -> F::Output
where
    F::Output: 'static,
{
    // Create ref-counted task cell
    let cell_ptr = TaskCell::<F::Output>::new();
    
    let raw = RawTask {
        ptr: unsafe { NonNull::new_unchecked(cell_ptr as *mut ()) },
        vtable: make_vtable::<F::Output>(),
    };
    
    // Create wrapper
    let wrapper = WrapperFuture {
        future,
        cell_ptr,
        vtable: raw.vtable,
    };

    // Transmute to add Send bound (safe for single-threaded runtime)
    let boxed: Pin<Box<dyn Future<Output = ()> + Send>> = unsafe {
        let boxed: Pin<Box<dyn Future<Output = ()>>> = Box::pin(wrapper);
        std::mem::transmute(boxed)
    };
    let future_ptr = Box::into_raw(Box::new(boxed)) as *mut ();

    unsafe extern "C" fn poll_erased(future_ptr: *mut (), waker: FfiWaker) -> FfiPoll {
        let future = &mut *(future_ptr as *mut Pin<Box<dyn Future<Output = ()> + Send>>);
        let waker = ffi_waker_to_std(waker);
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => FfiPoll::Ready,
            Poll::Pending => FfiPoll::Pending,
        }
    }

    unsafe extern "C" fn drop_erased(future_ptr: *mut ()) {
        drop(Box::from_raw(
            future_ptr as *mut Pin<Box<dyn Future<Output = ()> + Send>>,
        ));
    }

    let _task_id = unsafe { tau_spawn(future_ptr, poll_erased, drop_erased) };
    
    // Drive until complete
    loop {
        if unsafe { (raw.vtable.is_complete)(raw.ptr) } {
            break;
        }
        unsafe { tau_drive() };
        std::thread::sleep(Duration::from_micros(100));
    }
    
    // Read result
    let mut result: Poll<Option<F::Output>> = Poll::Pending;
    unsafe {
        (raw.vtable.try_read_output)(raw.ptr, &mut result as *mut _ as *mut ());
    }
    
    // Decrement our ref count
    unsafe {
        if (raw.vtable.dec_ref)(raw.ptr) {
            (raw.vtable.drop_cell)(raw.ptr);
        }
    }
    
    match result {
        Poll::Ready(Some(val)) => val,
        Poll::Ready(None) => panic!("block_on: task was aborted"),
        Poll::Pending => panic!("block_on: task did not complete"),
    }
}

// =============================================================================
// Sleep / Timer
// =============================================================================

/// Sleep for the given duration.
pub async fn sleep(duration: Duration) {
    SleepFuture::new(duration).await
}

/// Future that completes after a duration.
pub struct SleepFuture {
    nanos: u64,
    handle: u64,
}

impl SleepFuture {
    pub fn new(duration: Duration) -> Self {
        Self {
            nanos: duration.as_nanos() as u64,
            handle: 0,
        }
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.handle == 0 {
            self.handle = unsafe { tau_timer_create(self.nanos) };
            return Poll::Pending;
        }
        if unsafe { tau_timer_check(self.handle) } == 1 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { tau_timer_cancel(self.handle) };
        }
    }
}

// =============================================================================
// FfiWaker conversion
// =============================================================================

fn ffi_waker_to_std(ffi: FfiWaker) -> Waker {
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
