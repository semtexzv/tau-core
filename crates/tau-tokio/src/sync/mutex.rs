//! An async-aware mutex.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

/// An async-aware mutual exclusion lock.
pub struct Mutex<T: ?Sized> {
    locked: AtomicBool,
    waiters: StdMutex<Vec<Waker>>,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new mutex in an unlocked state.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: StdMutex::new(Vec::new()),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the mutex, returning the underlying data.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Locks the mutex, blocking until it becomes available.
    pub fn lock(&self) -> MutexLockFuture<'_, T> {
        MutexLockFuture { mutex: self }
    }

    /// Attempts to acquire the lock without waiting.
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Ok(MutexGuard { mutex: self })
        } else {
            Err(TryLockError(()))
        }
    }

    /// Returns a mutable reference to the underlying data.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
        // Wake one waiter
        if let Ok(mut waiters) = self.waiters.lock() {
            if let Some(waker) = waiters.pop() {
                waker.wake();
            }
        }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mutex").finish_non_exhaustive()
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Future returned by [`Mutex::lock`].
pub struct MutexLockFuture<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<'a, T: ?Sized> Future for MutexLockFuture<'a, T> {
    type Output = MutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mutex.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Poll::Ready(MutexGuard { mutex: self.mutex })
        } else {
            // Add waker to list
            if let Ok(mut waiters) = self.mutex.waiters.lock() {
                waiters.push(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

/// RAII guard returned by [`Mutex::lock`].
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// Error returned by [`Mutex::try_lock`].
#[derive(Debug)]
pub struct TryLockError(());

impl std::fmt::Display for TryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock is already held")
    }
}

impl std::error::Error for TryLockError {}
