//! An async notification primitive.

use std::sync::Mutex;
use std::task::Waker;

/// Notifies one or many waiting tasks.
pub struct Notify {
    inner: Mutex<NotifyInner>,
}

struct NotifyInner {
    notified: bool,
    waiters: Vec<Waker>,
}

impl Notify {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(NotifyInner {
                notified: false,
                waiters: Vec::new(),
            }),
        }
    }

    /// Notifies a single waiting task.
    pub fn notify_one(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(waker) = inner.waiters.pop() {
            waker.wake();
        } else {
            inner.notified = true;
        }
    }

    /// Notifies all waiting tasks.
    pub fn notify_waiters(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.notified = true;
        for waker in inner.waiters.drain(..) {
            waker.wake();
        }
    }

    /// Returns a future that completes when this `Notify` is notified.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            registered: false,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Notify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notify").finish()
    }
}

/// Future returned by [`Notify::notified`].
pub struct Notified<'a> {
    notify: &'a Notify,
    registered: bool,
}

impl std::future::Future for Notified<'_> {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let mut inner = self.notify.inner.lock().unwrap();
        if inner.notified {
            inner.notified = false;
            std::task::Poll::Ready(())
        } else {
            if !self.registered {
                inner.waiters.push(cx.waker().clone());
                self.registered = true;
            }
            std::task::Poll::Pending
        }
    }
}
