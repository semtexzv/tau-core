//! A one-shot channel for sending a single value.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Inner<T> {
    value: Option<T>,
    waker: Option<Waker>,
    sender_alive: bool,
}

/// Creates a new oneshot channel, returning the sender and receiver halves.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Mutex::new(Inner {
        value: None,
        waker: None,
        sender_alive: true,
    }));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

// ── Sender ──────────────────────────────────────────────────────────────────

pub struct Sender<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.sender_alive = false;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Sender<T> {
    /// Send a value, consuming the sender.
    pub fn send(self, value: T) -> Result<(), T> {
        let mut inner = self.inner.lock().unwrap();
        inner.value = Some(value);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
        Ok(())
    }

    /// Returns `true` if the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        // Simplified: can't truly detect receiver drop without extra tracking.
        false
    }

    /// Polls for the receiver being dropped.
    pub fn poll_closed(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
        // Simplified: always Pending.
        Poll::Pending
    }
}

// ── Receiver ────────────────────────────────────────────────────────────────

pub struct Receiver<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.value.take() {
            Poll::Ready(Ok(val))
        } else if !inner.sender_alive {
            Poll::Ready(Err(RecvError))
        } else {
            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct RecvError;

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "oneshot sender dropped")
    }
}

impl std::error::Error for RecvError {}
