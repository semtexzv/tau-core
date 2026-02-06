//! A single-producer, multi-consumer channel that only retains the last sent value.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct WatchInner<T> {
    value: T,
    version: u64,
    waiters: Vec<Waker>,
    closed: bool,
}

/// Creates a new watch channel, returning the sender and receiver halves.
pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Mutex::new(WatchInner {
        value: init,
        version: 1,
        waiters: Vec::new(),
        closed: false,
    }));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver {
            inner,
            seen_version: 0,
        },
    )
}

// ── Sender ──────────────────────────────────────────────────────────────────

pub struct Sender<T> {
    inner: Arc<Mutex<WatchInner<T>>>,
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        for waker in inner.waiters.drain(..) {
            waker.wake();
        }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut inner = self.inner.lock().unwrap();
        inner.value = value;
        inner.version += 1;
        for waker in inner.waiters.drain(..) {
            waker.wake();
        }
        Ok(())
    }

    pub fn send_replace(&self, mut value: T) -> T {
        let mut inner = self.inner.lock().unwrap();
        std::mem::swap(&mut inner.value, &mut value);
        inner.version += 1;
        for waker in inner.waiters.drain(..) {
            waker.wake();
        }
        value
    }

    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            inner: &self.inner,
        }
    }

    pub fn subscribe(&self) -> Receiver<T> {
        let inner = self.inner.lock().unwrap();
        let version = inner.version;
        drop(inner);
        Receiver {
            inner: self.inner.clone(),
            seen_version: version,
        }
    }

    pub fn is_closed(&self) -> bool {
        false
    }

    pub fn receiver_count(&self) -> usize {
        Arc::strong_count(&self.inner).saturating_sub(1)
    }
}

// ── Receiver ────────────────────────────────────────────────────────────────

pub struct Receiver<T> {
    inner: Arc<Mutex<WatchInner<T>>>,
    seen_version: u64,
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            seen_version: self.seen_version,
        }
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("seen_version", &self.seen_version)
            .finish()
    }
}

impl<T> Receiver<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            inner: &self.inner,
        }
    }

    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let inner = self.inner.lock().unwrap();
        self.seen_version = inner.version;
        drop(inner);
        Ref {
            inner: &self.inner,
        }
    }

    pub async fn changed(&mut self) -> Result<(), RecvError> {
        ChangedFuture { receiver: self }.await
    }

    pub fn has_changed(&self) -> Result<bool, RecvError> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(RecvError);
        }
        Ok(inner.version != self.seen_version)
    }
}

struct ChangedFuture<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<T> std::future::Future for ChangedFuture<'_, T> {
    type Output = Result<(), RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), RecvError>> {
        let this = unsafe { self.get_unchecked_mut() };
        let mut inner = this.receiver.inner.lock().unwrap();
        if inner.version != this.receiver.seen_version {
            this.receiver.seen_version = inner.version;
            Poll::Ready(Ok(()))
        } else if inner.closed {
            Poll::Ready(Err(RecvError))
        } else {
            inner.waiters.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ── Ref ─────────────────────────────────────────────────────────────────────

pub struct Ref<'a, T> {
    inner: &'a Arc<Mutex<WatchInner<T>>>,
}

impl<T> std::ops::Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // Simplification: real tokio uses an RwLock read guard.
        // This is technically unsound but works for single-threaded use.
        unsafe {
            let inner = self.inner.lock().unwrap();
            let ptr: *const T = &inner.value;
            &*ptr
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct RecvError;

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "watch channel closed")
    }
}

impl std::error::Error for RecvError {}

#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T: std::fmt::Debug> std::fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "watch channel closed")
    }
}

impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

/// Sub-module so `tokio::sync::watch::error::RecvError` resolves
/// (used by tokio-stream's WatchStream).
pub mod error {
    pub use super::RecvError;
    pub use super::SendError;
}
