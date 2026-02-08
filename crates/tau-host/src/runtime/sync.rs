//! Synchronization primitives abstraction for loom testing.
//!
//! When compiled with `--cfg loom`, this module uses loom's types which allow
//! deterministic concurrency testing. Otherwise, it uses std types.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU16, AtomicU64, Ordering};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};

#[cfg(loom)]
pub(crate) use loom::thread;

#[cfg(not(loom))]
pub(crate) use std::thread;

#[cfg(loom)]
pub(crate) use loom::cell::UnsafeCell;

#[cfg(not(loom))]
pub(crate) use std::cell::UnsafeCell;

// =============================================================================
// RemoteQueue - lock-free queue for cross-thread wakes
// =============================================================================
// 
// Under loom, we use a Mutex<VecDeque> since loom doesn't provide SegQueue.
// This is fine for testing - we're testing our *usage* of the queue, not the
// queue's internal implementation (crossbeam-queue has its own tests).

#[cfg(not(loom))]
mod queue {
    use crossbeam_queue::SegQueue;
    
    pub struct RemoteQueue<T>(SegQueue<T>);
    
    impl<T> RemoteQueue<T> {
        pub const fn new() -> Self {
            Self(SegQueue::new())
        }
        
        pub fn push(&self, value: T) {
            self.0.push(value);
        }
        
        pub fn pop(&self) -> Option<T> {
            self.0.pop()
        }
        
        #[allow(dead_code)]
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }
}

#[cfg(loom)]
mod queue {
    use loom::sync::Mutex;
    use std::collections::VecDeque;
    
    pub struct RemoteQueue<T>(Mutex<VecDeque<T>>);
    
    impl<T> RemoteQueue<T> {
        pub fn new() -> Self {
            Self(Mutex::new(VecDeque::new()))
        }
        
        pub fn push(&self, value: T) {
            self.0.lock().unwrap().push_back(value);
        }
        
        pub fn pop(&self) -> Option<T> {
            self.0.lock().unwrap().pop_front()
        }
        
        pub fn is_empty(&self) -> bool {
            self.0.lock().unwrap().is_empty()
        }
    }
}

pub use queue::RemoteQueue;

// =============================================================================
// Loom-compatible thread parking
// =============================================================================

#[cfg(loom)]
#[allow(dead_code)]
pub(crate) fn park() {
    loom::thread::yield_now();
}

#[cfg(not(loom))]
#[allow(dead_code)]
pub(crate) fn park() {
    std::thread::park();
}

#[cfg(loom)]
#[allow(dead_code)]
pub(crate) fn unpark(thread: &loom::thread::Thread) {
    thread.unpark();
}

#[cfg(not(loom))]
#[allow(dead_code)]
pub(crate) fn unpark(thread: &std::thread::Thread) {
    thread.unpark();
}
