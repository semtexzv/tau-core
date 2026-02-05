//! Task spawning and utilities.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use crate::JoinHandle;

/// Yields execution back to the runtime.
pub async fn yield_now() {
    struct YieldNow(bool);

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    YieldNow(false).await
}

/// Runs a blocking closure on a dedicated thread.
pub fn spawn_blocking<F, T>(f: F) -> crate::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    use std::sync::{Arc, Mutex};

    let shared = Arc::new(Mutex::new(SpawnBlockingState::<T> {
        result: None,
        waker: None,
    }));

    let shared2 = shared.clone();
    std::thread::spawn(move || {
        if std::env::var("TAU_DEBUG").as_deref() == Ok("1") {
            eprintln!("[task] spawn_blocking thread started");
        }
        let result = f();
        if std::env::var("TAU_DEBUG").as_deref() == Ok("1") {
            eprintln!("[task] spawn_blocking thread completed, waking");
        }
        let mut state = shared2.lock().unwrap();
        state.result = Some(result);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    });

    let fut = SpawnBlockingFuture { shared };
    crate::spawn(fut)
}

struct SpawnBlockingState<T> {
    result: Option<T>,
    waker: Option<std::task::Waker>,
}

struct SpawnBlockingFuture<T> {
    shared: std::sync::Arc<std::sync::Mutex<SpawnBlockingState<T>>>,
}

impl<T> Future for SpawnBlockingFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.shared.lock().unwrap();
        if let Some(result) = state.result.take() {
            Poll::Ready(result)
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// =============================================================================
// JoinSet
// =============================================================================

/// A collection of tasks spawned on the runtime.
/// 
/// Tasks can be added with [`spawn`](JoinSet::spawn) and awaited with
/// [`join_next`](JoinSet::join_next).
pub struct JoinSet<T: 'static> {
    handles: Vec<crate::JoinHandle<T>>,
}

impl<T: 'static> Default for JoinSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> JoinSet<T> {
    /// Create a new empty JoinSet.
    pub fn new() -> Self {
        Self { handles: Vec::new() }
    }

    /// Returns the number of tasks in the set.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

impl<T: Send + 'static> JoinSet<T> {
    /// Spawn a task onto the runtime and add it to the set.
    pub fn spawn<F>(&mut self, task: F) -> crate::AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
    {
        let handle = crate::spawn(task);
        let abort_handle = handle.abort_handle();
        self.handles.push(handle);
        abort_handle
    }

    /// Wait for one of the tasks in the set to complete.
    /// 
    /// Returns `None` if the set is empty.
    pub async fn join_next(&mut self) -> Option<Result<T, crate::JoinError>> {
        if self.handles.is_empty() {
            return None;
        }

        // Poll all handles to find one that's ready
        JoinNextFuture { join_set: self }.await
    }

    /// Abort all tasks in the set.
    pub fn abort_all(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }

    /// Detach all tasks, allowing them to continue running.
    pub fn detach_all(&mut self) {
        self.handles.clear();
    }

    /// Shut down the set, aborting all tasks.
    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }
}

struct JoinNextFuture<'a, T: 'static> {
    join_set: &'a mut JoinSet<T>,
}

impl<'a, T: 'static> Future for JoinNextFuture<'a, T> {
    type Output = Option<Result<T, crate::JoinError>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.join_set.handles.is_empty() {
            return Poll::Ready(None);
        }

        // Check each handle for completion
        for i in 0..self.join_set.handles.len() {
            let handle = &mut self.join_set.handles[i];
            // Safety: we're not moving the handle, just polling it
            let handle_pin = unsafe { Pin::new_unchecked(handle) };
            
            if let Poll::Ready(result) = handle_pin.poll(cx) {
                // Remove the completed handle
                self.join_set.handles.swap_remove(i);
                return Poll::Ready(Some(result));
            }
        }

        Poll::Pending
    }
}

impl<T: 'static> Drop for JoinSet<T> {
    fn drop(&mut self) {
        // Cancel all remaining tasks
        for handle in &self.handles {
            handle.abort();
        }
    }
}

// =============================================================================
// AbortHandle
// =============================================================================

/// A handle that can be used to abort a spawned task.
#[derive(Clone)]
pub struct AbortHandle {
    // Simplified: just a marker, actual abort not fully implemented
    _phantom: std::marker::PhantomData<()>,
}

impl AbortHandle {
    pub(crate) fn new() -> Self {
        Self { _phantom: std::marker::PhantomData }
    }

    /// Abort the associated task.
    pub fn abort(&self) {
        // Simplified: abort not fully implemented
    }

    /// Check if the task was aborted.
    pub fn is_finished(&self) -> bool {
        false
    }
}
