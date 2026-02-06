//! Tokio compatibility shim backed by the Tau runtime.
//!
//! This crate is named `tokio` (package name) so that plugins depending on
//! `tokio` get this shim instead. Symbol names match real tokio, so
//! `use tokio::spawn` works transparently.
//!
//! ## What works now
//! - `tokio::spawn` / `JoinHandle`
//! - `tokio::time::sleep`, `interval`
//! - `tokio::sync::{mpsc, oneshot, watch, Notify, Semaphore, Mutex}`
//! - `tokio::task::{yield_now, spawn_blocking, JoinSet}`
//! - `tokio::net::{TcpStream, TcpListener, TcpSocket, UdpSocket}`
//! - `tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt}`
//! - `tokio::fs` (via spawn_blocking)

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

// =============================================================================
// Debug logging (TAU_DEBUG=1 to enable)
// =============================================================================

fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TAU_DEBUG").map_or(false, |v| v == "1"))
}

#[doc(hidden)]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::debug_enabled() {
            eprintln!("[tokio] {}", format!($($arg)*));
        }
    };
}

// ── Modules (matching real tokio's structure) ───────────────────────────────

pub mod fs;
pub mod io;
pub mod net;
pub mod runtime;
pub mod signal;
pub mod sync;
pub mod task;
pub mod time;

// ── Proc-macro re-exports (#[tokio::main], #[tokio::test]) ─────────────────

#[cfg(feature = "macros")]
pub use tokio_macros::main;
#[cfg(feature = "macros")]
pub use tokio_macros::test;

// Always export (many crates use these without feature gates)
pub use tokio_macros::main_rt as main_rt_impl;

// ── Top-level re-exports ────────────────────────────────────────────────────

pub use time::sleep;
pub use task::{AbortHandle, JoinSet};

// =============================================================================
// spawn + JoinHandle
// =============================================================================

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let inner = tau::spawn(future);
    JoinHandle { inner }
}

pub struct JoinHandle<T> {
    inner: tau::JoinHandle<T>,
}

// JoinHandle is Unpin because T is behind a pointer, not stored inline
impl<T> Unpin for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    /// Abort the task. Currently a no-op (task cancellation not yet implemented).
    pub fn abort(&self) {
        self.inner.abort();
    }

    /// Get an AbortHandle for this task.
    pub fn abort_handle(&self) -> task::AbortHandle {
        task::AbortHandle::new()
    }

    /// Check if the task has finished.
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
}

// Future impl - no 'static bound needed, T is accessed via vtable
impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = unsafe { &mut self.get_unchecked_mut().inner };
        match unsafe { Pin::new_unchecked(inner) }.poll(cx) {
            Poll::Ready(Some(val)) => Poll::Ready(Ok(val)),
            Poll::Ready(None) => Poll::Ready(Err(JoinError { cancelled: true })),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
pub struct JoinError {
    cancelled: bool,
}

impl JoinError {
    /// Returns `true` if the task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task join error")
    }
}

impl std::error::Error for JoinError {}

// =============================================================================
// pin! macro
// =============================================================================

/// Pins a value on the stack.
#[macro_export]
macro_rules! pin {
    ($($x:ident),*) => {
        $(
            let mut $x = $x;
            #[allow(unused_mut)]
            let mut $x = unsafe { std::pin::Pin::new_unchecked(&mut $x) };
        )*
    };
}

// =============================================================================
// join! macro
// =============================================================================

/// Waits on multiple concurrent branches, returning when **all** complete.
#[macro_export]
macro_rules! join {
    ($($fut:expr),+ $(,)?) => {{
        $(let mut $crate::__internal_named!($fut) = $fut;)*
        loop {
            let mut all_done = true;
            tau::drive();
            let waker = $crate::__noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            $(
                {
                    let fut = unsafe { std::pin::Pin::new_unchecked(&mut $crate::__internal_named!($fut)) };
                    if fut.poll(&mut cx).is_pending() {
                        all_done = false;
                    }
                }
            )*
            if all_done { break; }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }};
}

// Helper to create unique names from expressions (simplified — works for ident exprs)
#[doc(hidden)]
#[macro_export]
macro_rules! __internal_named {
    ($e:expr) => { _fut };
}

// =============================================================================
// try_join! macro
// =============================================================================

/// Waits on multiple concurrent branches, returning when **all** complete
/// or when one returns an error.
#[macro_export]
macro_rules! try_join {
    ($($fut:expr),+ $(,)?) => {
        // Simplified: just await sequentially for now
        (|| async {
            Ok(($($fut.await?,)+))
        })().await
    };
}

// =============================================================================
// select! macro (simplified, 2-branch)
// =============================================================================

#[macro_export]
macro_rules! select {
    (
        $bind1:pat = $fut1:expr => $body1:expr,
        $bind2:pat = $fut2:expr => $body2:expr $(,)?
    ) => {{
        let mut f1 = $fut1;
        let mut f2 = $fut2;
        let mut f1 = unsafe { std::pin::Pin::new_unchecked(&mut f1) };
        let mut f2 = unsafe { std::pin::Pin::new_unchecked(&mut f2) };

        loop {
            tau::drive();
            let waker = $crate::__noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            if let std::task::Poll::Ready(v) = f1.as_mut().poll(&mut cx) {
                let $bind1 = v;
                break $body1;
            }
            if let std::task::Poll::Ready(v) = f2.as_mut().poll(&mut cx) {
                let $bind2 = v;
                break $body2;
            }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }};
}

#[doc(hidden)]
pub fn __noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    unsafe { std::task::Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
