//! Synchronization primitives.

pub mod mpsc;
pub mod mutex;
pub mod notify;
pub mod oneshot;
pub mod rwlock;
pub mod semaphore;
pub mod watch;

// Re-export key types at the `sync` level (matching real tokio).
pub use self::mutex::{Mutex, MutexGuard, MutexLockFuture};
pub use self::notify::{Notified, Notify};
pub use self::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
pub use self::semaphore::{
    AcquireError, OwnedSemaphorePermit, Semaphore, SemaphorePermit, TryAcquireError,
};

/// Sub-module so `tokio::sync::futures::Notified` resolves
/// (used by tokio-util's CancellationToken).
pub mod futures {
    pub use super::notify::Notified;
}
