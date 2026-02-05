//! Runtime and builder stubs for `#[tokio::main]` / `Runtime::block_on` compatibility.

use std::future::Future;

pub struct Runtime;

impl Runtime {
    pub fn block_on<F: Future>(&self, future: F) -> F::Output
    where
        F: 'static,
        F::Output: 'static,
    {
        tau::block_on(future)
    }

    pub fn handle(&self) -> Handle {
        Handle
    }

    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        crate::spawn(future)
    }
}

pub struct Builder;

impl Builder {
    pub fn new_current_thread() -> Self {
        Self
    }
    pub fn new_multi_thread() -> Self {
        Self
    }
    pub fn enable_all(&mut self) -> &mut Self {
        self
    }
    pub fn enable_io(&mut self) -> &mut Self {
        self
    }
    pub fn enable_time(&mut self) -> &mut Self {
        self
    }
    pub fn worker_threads(&mut self, _n: usize) -> &mut Self {
        self
    }
    pub fn build(&self) -> std::io::Result<Runtime> {
        Ok(Runtime)
    }
}

/// A handle to the runtime.
#[derive(Clone, Debug)]
pub struct Handle;

impl Handle {
    /// Returns a handle to the current runtime.
    pub fn current() -> Self {
        Handle
    }

    /// Try to get a handle to the current runtime.
    pub fn try_current() -> Result<Self, TryCurrentError> {
        Ok(Handle)
    }

    /// Spawn a future onto the runtime.
    pub fn spawn<F>(&self, future: F) -> crate::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        crate::spawn(future)
    }

    /// Run a future to completion on the runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output
    where
        F: 'static,
        F::Output: 'static,
    {
        tau::block_on(future)
    }

    /// Enter the runtime context.
    pub fn enter(&self) -> EnterGuard<'_> {
        EnterGuard { _handle: self }
    }
}

/// Guard returned by [`Handle::enter`].
pub struct EnterGuard<'a> {
    _handle: &'a Handle,
}

/// Error returned by [`Handle::try_current`].
#[derive(Debug)]
pub struct TryCurrentError(());

impl std::fmt::Display for TryCurrentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no runtime running")
    }
}

impl std::error::Error for TryCurrentError {}
