//! An async-aware read-write lock.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

const WRITE_LOCKED: usize = usize::MAX;

/// An async-aware reader-writer lock.
pub struct RwLock<T: ?Sized> {
    /// 0 = unlocked, 1..MAX-1 = readers, MAX = write-locked
    state: AtomicUsize,
    waiters: StdMutex<Vec<Waker>>,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    /// Creates a new `RwLock`.
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            waiters: StdMutex::new(Vec::new()),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Locks for reading.
    pub fn read(&self) -> RwLockReadFuture<'_, T> {
        RwLockReadFuture { lock: self }
    }

    /// Locks for writing.
    pub fn write(&self) -> RwLockWriteFuture<'_, T> {
        RwLockWriteFuture { lock: self }
    }

    /// Attempts to acquire the read lock without waiting.
    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state == WRITE_LOCKED {
                return Err(TryLockError(()));
            }
            if self.state.compare_exchange(state, state + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return Ok(RwLockReadGuard { lock: self });
            }
        }
    }

    /// Attempts to acquire the write lock without waiting.
    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        if self.state.compare_exchange(0, WRITE_LOCKED, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            Ok(RwLockWriteGuard { lock: self })
        } else {
            Err(TryLockError(()))
        }
    }

    /// Returns a mutable reference to the underlying data.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn wake_waiters(&self) {
        if let Ok(mut waiters) = self.waiters.lock() {
            for waker in waiters.drain(..) {
                waker.wake();
            }
        }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

// =============================================================================
// Read future and guard
// =============================================================================

/// Future returned by [`RwLock::read`].
pub struct RwLockReadFuture<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}

impl<'a, T: ?Sized> Future for RwLockReadFuture<'a, T> {
    type Output = RwLockReadGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let state = self.lock.state.load(Ordering::Acquire);
            if state == WRITE_LOCKED {
                if let Ok(mut waiters) = self.lock.waiters.lock() {
                    waiters.push(cx.waker().clone());
                }
                return Poll::Pending;
            }
            if self.lock.state.compare_exchange(state, state + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return Poll::Ready(RwLockReadGuard { lock: self.lock });
            }
        }
    }
}

/// RAII guard for read access.
pub struct RwLockReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let prev = self.lock.state.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last reader, wake writers
            self.lock.wake_waiters();
        }
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

// =============================================================================
// Write future and guard
// =============================================================================

/// Future returned by [`RwLock::write`].
pub struct RwLockWriteFuture<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}

impl<'a, T: ?Sized> Future for RwLockWriteFuture<'a, T> {
    type Output = RwLockWriteGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.lock.state.compare_exchange(0, WRITE_LOCKED, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            Poll::Ready(RwLockWriteGuard { lock: self.lock })
        } else {
            if let Ok(mut waiters) = self.lock.waiters.lock() {
                waiters.push(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

/// RAII guard for write access.
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.state.store(0, Ordering::Release);
        self.lock.wake_waiters();
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// Error returned by try_read/try_write.
#[derive(Debug)]
pub struct TryLockError(());

impl std::fmt::Display for TryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock is already held")
    }
}

impl std::error::Error for TryLockError {}
