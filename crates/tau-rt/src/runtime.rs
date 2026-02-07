//! Public runtime API — spawn, JoinHandle, block_on, sleep.
//!
//! Generic functions here are monomorphized into each plugin (via dylib rmeta).
//! They type-erase futures into the executor's slab slots. Non-generic executor
//! core functions are resolved from the shared dylib at load time.
//!
//! # JoinHandle lifecycle
//!
//! - `await` → polls until complete, takes result, frees slot
//! - `cancel()` → drops future, frees slot
//! - `drop` (without await) → marks detached; auto-cleanup on completion
//! - `task_id()` → packed (gen, slot_id) for FFI / host block_on

use crate::executor::{self, DeallocFn, DropFutureFn, DropResultFn, PollFn, ReadFn};
use std::future::Future;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

// =============================================================================
// IO reactor FFI (provided by host)
// =============================================================================

use crate::ffi::tau_react;

// =============================================================================
// Payload union — stores either the future or its result in the same memory
// =============================================================================

union Payload<F, T> {
    future: ManuallyDrop<F>,
    result: ManuallyDrop<T>,
}

// =============================================================================
// Type-erased poll/drop/read functions (monomorphized per F, T)
// =============================================================================

// --- Inline (future stored in slot storage) ---

unsafe fn poll_inline<F: Future<Output = T>, T>(ptr: *mut u8, waker: &Waker) -> Poll<()> {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    let future = Pin::new_unchecked(&mut *payload.future);
    let mut cx = Context::from_waker(waker);
    match future.poll(&mut cx) {
        Poll::Ready(v) => {
            ManuallyDrop::drop(&mut payload.future);
            std::ptr::write(&mut payload.result, ManuallyDrop::new(v));
            Poll::Ready(())
        }
        Poll::Pending => Poll::Pending,
    }
}

unsafe fn drop_inline_result<F: Future<Output = T>, T>(ptr: *mut u8) {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    ManuallyDrop::drop(&mut payload.result);
}

unsafe fn cancel_inline<F: Future<Output = T>, T>(ptr: *mut u8) {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    ManuallyDrop::drop(&mut payload.future);
}

unsafe fn read_inline<F: Future<Output = T>, T>(ptr: *mut u8) -> *mut u8 {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    &mut *payload.result as *mut T as *mut u8
}

// --- Boxed (future stored on heap, pointer in slot storage) ---

unsafe fn poll_boxed<F: Future<Output = T>, T>(ptr: *mut u8, waker: &Waker) -> Poll<()> {
    let boxed_ptr = *(ptr as *mut *mut Payload<F, T>);
    let payload = &mut *boxed_ptr;
    let future = Pin::new_unchecked(&mut *payload.future);
    let mut cx = Context::from_waker(waker);
    match future.poll(&mut cx) {
        Poll::Ready(v) => {
            ManuallyDrop::drop(&mut payload.future);
            std::ptr::write(&mut payload.result, ManuallyDrop::new(v));
            Poll::Ready(())
        }
        Poll::Pending => Poll::Pending,
    }
}

unsafe fn drop_boxed_result<F: Future<Output = T>, T>(ptr: *mut u8) {
    let boxed_ptr = *(ptr as *mut *mut Payload<F, T>);
    let payload = &mut *boxed_ptr;
    ManuallyDrop::drop(&mut payload.result);
    // Free the Box (union drop is no-op, so this just deallocates)
    drop(Box::from_raw(boxed_ptr));
}

unsafe fn cancel_boxed<F: Future<Output = T>, T>(ptr: *mut u8) {
    let boxed_ptr = *(ptr as *mut *mut Payload<F, T>);
    let payload = &mut *boxed_ptr;
    ManuallyDrop::drop(&mut payload.future);
    drop(Box::from_raw(boxed_ptr));
}

unsafe fn read_boxed<F: Future<Output = T>, T>(ptr: *mut u8) -> *mut u8 {
    let boxed_ptr = *(ptr as *mut *mut Payload<F, T>);
    let payload = &mut *boxed_ptr;
    &mut *payload.result as *mut T as *mut u8
}

unsafe fn dealloc_boxed<F: Future<Output = T>, T>(ptr: *mut u8) {
    let boxed_ptr = *(ptr as *mut *mut Payload<F, T>);
    // Union drop is a no-op — this just frees the heap allocation.
    // The result T was already moved out via ptr::read.
    drop(Box::from_raw(boxed_ptr));
}

// =============================================================================
// spawn
// =============================================================================

/// Spawn a future as a task on the executor. Returns a JoinHandle.
///
/// The future is stored inline if small enough (≤ INLINE_SIZE bytes),
/// otherwise boxed on the heap with a pointer in the slot.
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let plugin_id = executor::current_plugin_id();
    let (id, gen, storage_ptr, executor_idx) = executor::alloc_raw();

    let payload_size = std::mem::size_of::<Payload<F, F::Output>>();
    let payload_align = std::mem::align_of::<Payload<F, F::Output>>();

    if payload_size <= executor::INLINE_SIZE && payload_align <= 128 {
        // Inline: write payload directly into slot storage
        unsafe {
            let ptr = storage_ptr as *mut Payload<F, F::Output>;
            std::ptr::write(ptr, Payload { future: ManuallyDrop::new(f) });
        }
        executor::commit_spawn(
            id,
            poll_inline::<F, F::Output> as PollFn,
            Some(drop_inline_result::<F, F::Output> as DropResultFn),
            Some(cancel_inline::<F, F::Output> as DropFutureFn),
            Some(read_inline::<F, F::Output> as ReadFn),
            None, // no dealloc needed for inline
            plugin_id,
        );
    } else {
        // Boxed: allocate on heap, store pointer in slot storage
        let boxed = Box::new(Payload { future: ManuallyDrop::new(f) });
        unsafe {
            let ptr = storage_ptr as *mut *mut Payload<F, F::Output>;
            std::ptr::write(ptr, Box::into_raw(boxed));
        }
        executor::commit_spawn(
            id,
            poll_boxed::<F, F::Output> as PollFn,
            Some(drop_boxed_result::<F, F::Output> as DropResultFn),
            Some(cancel_boxed::<F, F::Output> as DropFutureFn),
            Some(read_boxed::<F, F::Output> as ReadFn),
            Some(dealloc_boxed::<F, F::Output> as DeallocFn),
            plugin_id,
        );
    }

    JoinHandle {
        id,
        gen,
        executor_idx,
        taken: false,
        _m: PhantomData,
    }
}

// =============================================================================
// JoinHandle
// =============================================================================

/// Handle to a spawned task. Implements Future to await the result.
///
/// - `Output = Option<T>`: `Some(result)` on success, `None` if cancelled.
/// - Dropping without awaiting marks the task as detached (auto-cleanup).
pub struct JoinHandle<T> {
    id: u32,
    gen: u32,
    executor_idx: u16,
    taken: bool,
    _m: PhantomData<T>,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}
impl<T> Unpin for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    /// Get a packed task ID for FFI (host's wait_for_task).
    pub fn task_id(&self) -> u64 {
        executor::pack_task_id(self.gen, self.id)
    }

    /// Cancel the task. Drops the future if still running.
    pub fn cancel(&mut self) {
        if !self.taken {
            executor::cancel_task(self.id, self.gen, self.executor_idx);
            self.taken = true;
        }
    }

    /// Abort the task (alias for cancel, matches tokio API).
    pub fn abort(&self) {
        executor::cancel_task(self.id, self.gen, self.executor_idx);
    }

    /// Check if the task is finished.
    pub fn is_finished(&self) -> bool {
        executor::is_task_finished(self.id, self.gen, self.executor_idx)
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();

        // Get slot state via extern call
        let (state, gen) = executor::slot_state(this.id, this.executor_idx);
        
        // Check generation mismatch (slot reused)
        if gen != this.gen {
            this.taken = true;
            return Poll::Ready(None);
        }

        match state {
            executor::STATE_COMPLETE => {
                // Take result and free slot via extern call
                let (result_ptr, dealloc_fn) = executor::take_result(this.id, this.executor_idx);
                let result = unsafe { std::ptr::read(result_ptr as *mut T) };
                // Dealloc storage (for boxed) without dropping T (already moved)
                if let Some(dealloc) = dealloc_fn {
                    unsafe { dealloc(result_ptr) };
                }
                this.taken = true;
                Poll::Ready(Some(result))
            }
            executor::STATE_CANCELLED | executor::STATE_EMPTY => {
                this.taken = true;
                Poll::Ready(None)
            }
            _ => {
                // Task still running — store waker for notification
                executor::set_join_waker(this.id, this.executor_idx, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if !self.taken {
            executor::detach_or_cleanup(self.id, self.gen, self.executor_idx);
        }
    }
}

// =============================================================================
// block_on
// =============================================================================

/// Block the current thread until a future completes, driving the executor.
pub fn block_on<F: Future>(f: F) -> F::Output {
    use std::task::{RawWaker, RawWakerVTable};

    fn noop_clone(d: *const ()) -> RawWaker { RawWaker::new(d, &NOOP_VTABLE) }
    fn noop(_: *const ()) {}
    static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);

    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = std::pin::pin!(f);

    loop {
        // Poll root future
        if let Poll::Ready(v) = pinned.as_mut().poll(&mut cx) {
            return v;
        }

        // Drive executor until quiescent
        let mut drove = false;
        while executor::drive_cycle() {
            drove = true;
        }

        if drove {
            // Work was done — re-poll root future immediately
            continue;
        }

        // No work — block on IO reactor with timeout
        let timeout = executor::next_timer_deadline()
            .map(|d| d.min(Duration::from_millis(10)))
            .unwrap_or(Duration::from_millis(10));
        unsafe { tau_react(timeout.as_millis() as u64) };
    }
}

/// Drive the executor for one cycle. Returns number of tasks that completed.
/// (Backward-compatible with the old tau_drive API.)
pub fn drive() -> u32 {
    let mut completed = 0u32;
    while executor::drive_cycle() {
        completed += 1;
    }
    completed
}

/// Initialize the runtime. Call once on the main thread.
pub fn init() -> Result<(), i32> {
    executor::init();
    Ok(())
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
    deadline: std::time::Instant,
    handle: u64, // 0 = not registered
}

impl SleepFuture {
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + duration,
            handle: 0,
        }
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if std::time::Instant::now() >= self.deadline {
            return Poll::Ready(());
        }
        // Register or update timer waker
        self.handle = executor::timer_register(
            self.deadline,
            cx.waker().clone(),
            self.handle,
        );
        Poll::Pending
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        if self.handle != 0 {
            executor::timer_cancel(self.handle);
        }
    }
}
