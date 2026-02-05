//! An async counting semaphore.

use std::sync::{Arc, Mutex};
use std::task::Waker;

pub struct Semaphore {
    inner: Mutex<SemaphoreInner>,
}

struct SemaphoreInner {
    available: usize,
    waiters: Vec<(usize, Waker)>,
}

impl Semaphore {
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    pub fn new(permits: usize) -> Self {
        Self {
            inner: Mutex::new(SemaphoreInner {
                available: permits,
                waiters: Vec::new(),
            }),
        }
    }

    pub fn available_permits(&self) -> usize {
        self.inner.lock().unwrap().available
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        loop {
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.available > 0 {
                    inner.available -= 1;
                    return Ok(SemaphorePermit {
                        sem: self,
                        permits: 1,
                    });
                }
            }
            tau::drive();
            std::thread::yield_now();
        }
    }

    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.available > 0 {
            inner.available -= 1;
            Ok(SemaphorePermit {
                sem: self,
                permits: 1,
            })
        } else {
            Err(TryAcquireError::NoPermits)
        }
    }

    pub async fn acquire_owned(
        self: Arc<Self>,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        loop {
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.available > 0 {
                    inner.available -= 1;
                    return Ok(OwnedSemaphorePermit {
                        sem: self.clone(),
                        permits: 1,
                    });
                }
            }
            tau::drive();
            std::thread::yield_now();
        }
    }

    pub fn try_acquire_owned(
        self: Arc<Self>,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.available > 0 {
            inner.available -= 1;
            Ok(OwnedSemaphorePermit {
                sem: self.clone(),
                permits: 1,
            })
        } else {
            Err(TryAcquireError::NoPermits)
        }
    }

    pub async fn acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        let n = n as usize;
        loop {
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.available >= n {
                    inner.available -= n;
                    return Ok(OwnedSemaphorePermit {
                        sem: self.clone(),
                        permits: n,
                    });
                }
            }
            tau::drive();
            std::thread::yield_now();
        }
    }

    pub fn try_acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        let n = n as usize;
        let mut inner = self.inner.lock().unwrap();
        if inner.available >= n {
            inner.available -= n;
            Ok(OwnedSemaphorePermit {
                sem: self.clone(),
                permits: n,
            })
        } else {
            Err(TryAcquireError::NoPermits)
        }
    }

    pub fn add_permits(&self, n: usize) {
        self.inner.lock().unwrap().available += n;
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        for (_, waker) in inner.waiters.drain(..) {
            waker.wake();
        }
    }

    pub fn is_closed(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Semaphore").finish()
    }
}

// ── Permits ─────────────────────────────────────────────────────────────────

pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.inner.lock().unwrap().available += self.permits;
    }
}

pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: usize,
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.inner.lock().unwrap().available += self.permits;
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AcquireError;

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "semaphore closed")
    }
}

impl std::error::Error for AcquireError {}

#[derive(Debug)]
pub enum TryAcquireError {
    Closed,
    NoPermits,
}

impl std::fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryAcquireError::Closed => write!(f, "semaphore closed"),
            TryAcquireError::NoPermits => write!(f, "no permits available"),
        }
    }
}

impl std::error::Error for TryAcquireError {}
