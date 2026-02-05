//! A multi-producer, single-consumer channel.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

// =============================================================================
// Shared internals
// =============================================================================

struct ChannelInner<T> {
    queue: VecDeque<T>,
    #[allow(dead_code)]
    capacity: usize,
    receiver_waker: Option<Waker>,
    sender_count: usize,
    receiver_alive: bool,
}

// =============================================================================
// Bounded channel
// =============================================================================

pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Mutex::new(ChannelInner {
        queue: VecDeque::with_capacity(buffer),
        capacity: buffer,
        receiver_waker: None,
        sender_count: 1,
        receiver_alive: true,
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
    inner: Arc<Mutex<ChannelInner<T>>>,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.lock().unwrap().sender_count += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.sender_count -= 1;
        if inner.sender_count == 0 {
            if let Some(waker) = inner.receiver_waker.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Sender<T> {
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.receiver_alive {
            return Err(SendError(value));
        }
        inner.queue.push_back(value);
        if let Some(waker) = inner.receiver_waker.take() {
            waker.wake();
        }
        Ok(())
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.receiver_alive {
            return Err(TrySendError::Closed(value));
        }
        inner.queue.push_back(value);
        if let Some(waker) = inner.receiver_waker.take() {
            waker.wake();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        !self.inner.lock().unwrap().receiver_alive
    }

    /// Reserve capacity, consuming the sender and returning an [`OwnedPermit`].
    pub async fn reserve_owned(self) -> Result<OwnedPermit<T>, SendError<()>> {
        if self.is_closed() {
            return Err(SendError(()));
        }
        Ok(OwnedPermit {
            sender: Some(self),
        })
    }

    /// Reserve capacity (borrowed).
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        if self.is_closed() {
            return Err(SendError(()));
        }
        Ok(Permit { sender: self })
    }
}

// ── Permit ──────────────────────────────────────────────────────────────────

pub struct Permit<'a, T> {
    sender: &'a Sender<T>,
}

impl<'a, T> Permit<'a, T> {
    pub fn send(self, value: T) {
        let mut inner = self.sender.inner.lock().unwrap();
        inner.queue.push_back(value);
        if let Some(waker) = inner.receiver_waker.take() {
            waker.wake();
        }
    }
}

// ── OwnedPermit ─────────────────────────────────────────────────────────────

pub struct OwnedPermit<T> {
    sender: Option<Sender<T>>,
}

impl<T> std::fmt::Debug for OwnedPermit<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedPermit").finish()
    }
}

impl<T> OwnedPermit<T> {
    /// Send a value using the reserved permit, returning the sender back.
    pub fn send(mut self, value: T) -> Sender<T> {
        let sender = self.sender.take().expect("OwnedPermit already consumed");
        let mut inner = sender.inner.lock().unwrap();
        inner.queue.push_back(value);
        if let Some(waker) = inner.receiver_waker.take() {
            waker.wake();
        }
        drop(inner);
        sender
    }

    /// Release the permit without sending, returning the sender back.
    pub fn release(mut self) -> Sender<T> {
        self.sender.take().expect("OwnedPermit already consumed")
    }
}

impl<T> Drop for OwnedPermit<T> {
    fn drop(&mut self) {
        // permit was neither sent nor released — just drop it
    }
}

// ── Receiver ────────────────────────────────────────────────────────────────

pub struct Receiver<T> {
    inner: Arc<Mutex<ChannelInner<T>>>,
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.lock().unwrap().receiver_alive = false;
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        RecvFuture { receiver: self }.await
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.queue.pop_front() {
            Ok(val)
        } else if inner.sender_count == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn close(&mut self) {
        self.inner.lock().unwrap().receiver_alive = false;
    }

    /// Polls to receive a message on this channel.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.queue.pop_front() {
            Poll::Ready(Some(val))
        } else if inner.sender_count == 0 {
            Poll::Ready(None)
        } else {
            inner.receiver_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct RecvFuture<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<T> Future for RecvFuture<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = unsafe { self.get_unchecked_mut() };
        let mut inner = this.receiver.inner.lock().unwrap();
        if let Some(val) = inner.queue.pop_front() {
            Poll::Ready(Some(val))
        } else if inner.sender_count == 0 {
            Poll::Ready(None)
        } else {
            inner.receiver_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// =============================================================================
// Unbounded channel
// =============================================================================

pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let inner = Arc::new(Mutex::new(ChannelInner {
        queue: VecDeque::new(),
        capacity: usize::MAX,
        receiver_waker: None,
        sender_count: 1,
        receiver_alive: true,
    }));
    (
        UnboundedSender {
            inner: inner.clone(),
        },
        UnboundedReceiver { inner },
    )
}

// ── UnboundedSender ─────────────────────────────────────────────────────────

pub struct UnboundedSender<T> {
    inner: Arc<Mutex<ChannelInner<T>>>,
}

impl<T> std::fmt::Debug for UnboundedSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnboundedSender").finish()
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.inner.lock().unwrap().sender_count += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.sender_count -= 1;
        if inner.sender_count == 0 {
            if let Some(waker) = inner.receiver_waker.take() {
                waker.wake();
            }
        }
    }
}

impl<T> UnboundedSender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.receiver_alive {
            return Err(SendError(value));
        }
        inner.queue.push_back(value);
        if let Some(waker) = inner.receiver_waker.take() {
            waker.wake();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        !self.inner.lock().unwrap().receiver_alive
    }
}

// ── UnboundedReceiver ───────────────────────────────────────────────────────

pub struct UnboundedReceiver<T> {
    inner: Arc<Mutex<ChannelInner<T>>>,
}

impl<T> std::fmt::Debug for UnboundedReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnboundedReceiver").finish()
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        self.inner.lock().unwrap().receiver_alive = false;
    }
}

impl<T> UnboundedReceiver<T> {
    pub fn close(&mut self) {
        self.inner.lock().unwrap().receiver_alive = false;
    }

    pub async fn recv(&mut self) -> Option<T> {
        UnboundedRecvFuture { receiver: self }.await
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.queue.pop_front() {
            Ok(val)
        } else if inner.sender_count == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.queue.pop_front() {
            Poll::Ready(Some(val))
        } else if inner.sender_count == 0 {
            Poll::Ready(None)
        } else {
            inner.receiver_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct UnboundedRecvFuture<'a, T> {
    receiver: &'a mut UnboundedReceiver<T>,
}

impl<T> Future for UnboundedRecvFuture<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.receiver.poll_recv(cx)
    }
}

// =============================================================================
// Error types
// =============================================================================

#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> std::fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "channel closed")
    }
}

#[derive(Debug)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

impl<T> std::fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrySendError::Full(_) => write!(f, "channel full"),
            TrySendError::Closed(_) => write!(f, "channel closed"),
        }
    }
}

#[derive(Debug)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}
