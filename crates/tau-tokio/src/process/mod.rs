//! Asynchronous process management.
//!
//! Provides [`Command`], [`Child`], [`ChildStdin`], [`ChildStdout`], and
//! [`ChildStderr`] backed by the tau IO reactor.
//!
//! Child process waiting uses the centralized SIGCHLD handler in `tau-rt`.
//! When SIGCHLD fires, all `Child::wait()` futures are woken to call `try_wait()`.

use crate::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::pin::Pin;
use std::process::{ExitStatus, Output, Stdio};
use std::task::{Context, Poll};
use tau::io as tio;
use tau::process;

fn debug() -> bool {
    crate::debug_enabled()
}

// =============================================================================
// Command
// =============================================================================

/// An async wrapper around `std::process::Command`.
///
/// The `spawn()` call is synchronous — it calls `std::process::Command::spawn()`
/// internally. The async part is waiting for the child to exit and reading
/// from its pipes.
pub struct Command {
    inner: std::process::Command,
    kill_on_drop: bool,
}

impl Command {
    /// Creates a new `Command` for launching `program`.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            inner: std::process::Command::new(program),
            kill_on_drop: false,
        }
    }

    /// Adds an argument.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Adds multiple arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Sets an environment variable.
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    /// Sets multiple environment variables.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    /// Removes an environment variable.
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    /// Clears all environment variables.
    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    /// Sets the working directory.
    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    /// Sets the stdin handle.
    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdin(cfg);
        self
    }

    /// Sets the stdout handle.
    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdout(cfg);
        self
    }

    /// Sets the stderr handle.
    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stderr(cfg);
        self
    }

    /// If `true`, the child process is killed on `Child` drop.
    pub fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    // Unix-specific methods

    /// Sets the child process's user ID.
    #[cfg(unix)]
    pub fn uid(&mut self, id: u32) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.inner.uid(id);
        self
    }

    /// Sets the child process's group ID.
    #[cfg(unix)]
    pub fn gid(&mut self, id: u32) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.inner.gid(id);
        self
    }

    /// Sets the child process's process group.
    #[cfg(unix)]
    pub fn process_group(&mut self, pgroup: i32) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.inner.process_group(pgroup);
        self
    }

    /// Schedule a closure to be run just before `exec`.
    ///
    /// # Safety
    /// The closure must not violate memory safety. It runs after `fork()` in the
    /// child process.
    #[cfg(unix)]
    pub unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        use std::os::unix::process::CommandExt;
        self.inner.pre_exec(f);
        self
    }

    /// Spawns the child process.
    ///
    /// This is **synchronous** — it calls `std::process::Command::spawn()`.
    /// The async part is waiting for exit and reading pipes.
    pub fn spawn(&mut self) -> io::Result<Child> {
        let mut child = self.inner.spawn()?;

        if debug() {
            eprintln!("[process] spawned child pid={}", child.id());
        }

        // Take ownership of pipe handles and wrap them
        let stdin = child.stdin.take().map(|s| {
            let fd = s.as_raw_fd();
            set_nonblocking(fd);
            let handle = tio::register(fd, tio::WRITABLE)
                .expect("failed to register stdin with reactor");
            // Prevent std from closing the fd
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            std::mem::forget(s);
            ChildStdin {
                fd: owned,
                reactor_handle: handle,
            }
        });

        let stdout = child.stdout.take().map(|s| {
            let fd = s.as_raw_fd();
            set_nonblocking(fd);
            let handle = tio::register(fd, tio::READABLE)
                .expect("failed to register stdout with reactor");
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            std::mem::forget(s);
            ChildStdout {
                fd: owned,
                reactor_handle: handle,
            }
        });

        let stderr = child.stderr.take().map(|s| {
            let fd = s.as_raw_fd();
            set_nonblocking(fd);
            let handle = tio::register(fd, tio::READABLE)
                .expect("failed to register stderr with reactor");
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            std::mem::forget(s);
            ChildStderr {
                fd: owned,
                reactor_handle: handle,
            }
        });

        Ok(Child {
            inner: Some(child),
            stdin,
            stdout,
            stderr,
            kill_on_drop: self.kill_on_drop,
        })
    }

    /// Spawns the child and waits for it to exit, returning the exit status.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        self.spawn()?.wait().await
    }

    /// Spawns the child, reads all stdout/stderr, waits for exit.
    pub async fn output(&mut self) -> io::Result<Output> {
        // Ensure pipes are set up
        self.inner.stdout(Stdio::piped());
        self.inner.stderr(Stdio::piped());
        self.spawn()?.wait_with_output().await
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

// =============================================================================
// Child
// =============================================================================

/// A handle to a child process.
pub struct Child {
    inner: Option<std::process::Child>,
    /// The child's stdin handle, if piped.
    pub stdin: Option<ChildStdin>,
    /// The child's stdout handle, if piped.
    pub stdout: Option<ChildStdout>,
    /// The child's stderr handle, if piped.
    pub stderr: Option<ChildStderr>,
    kill_on_drop: bool,
}

impl Child {
    /// Returns the OS-assigned process ID, or `None` if the child has exited.
    pub fn id(&self) -> Option<u32> {
        self.inner.as_ref().map(|c| c.id())
    }

    /// Sends SIGKILL to the child. Does **not** wait for it to exit.
    pub fn start_kill(&mut self) -> io::Result<()> {
        if let Some(ref mut child) = self.inner {
            child.kill()
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has already been waited on",
            ))
        }
    }

    /// Sends SIGKILL then waits for the child to exit.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await?;
        Ok(())
    }

    /// Non-blocking check whether the child has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(ref mut child) = self.inner {
            child.try_wait()
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has already been waited on",
            ))
        }
    }

    /// Waits for the child to exit.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        // First, do a non-blocking check
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }

        // Poll using SIGCHLD notifications
        WaitFuture { child: self }.await
    }

    /// Reads all stdout and stderr, then waits for exit.
    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        // Take the pipe handles
        // Drop stdin so the child doesn't block waiting for input
        drop(self.stdin.take());
        let stdout_handle = self.stdout.take();
        let stderr_handle = self.stderr.take();

        // Read stdout and stderr concurrently with waiting
        let mut stdout_data = Vec::new();
        let mut stderr_data = Vec::new();

        if let Some(mut out) = stdout_handle {
            read_to_end(&mut out, &mut stdout_data).await?;
        }
        if let Some(mut err) = stderr_handle {
            read_to_end(&mut err, &mut stderr_data).await?;
        }

        let status = self.wait().await?;

        Ok(Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            if let Some(ref mut child) = self.inner {
                if debug() {
                    eprintln!("[process] kill_on_drop: killing pid={}", child.id());
                }
                let _ = child.kill();
            }
        }
    }
}

/// Read all bytes from an AsyncRead into a Vec.
async fn read_to_end<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<()> {
    use crate::io::AsyncReadExt;
    let mut tmp = [0u8; 4096];
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(())
}

/// Future that waits for a child process to exit using SIGCHLD.
struct WaitFuture<'a> {
    child: &'a mut Child,
}

impl<'a> std::future::Future for WaitFuture<'a> {
    type Output = io::Result<ExitStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        let this = unsafe { self.get_unchecked_mut() };

        // Try non-blocking wait first
        match this.child.try_wait() {
            Ok(Some(status)) => return Poll::Ready(Ok(status)),
            Ok(None) => {}
            Err(e) => return Poll::Ready(Err(e)),
        }

        // Not exited yet — register with centralized SIGCHLD handler
        let ffi_waker = tio::make_ffi_waker(cx);
        process::tau_sys_sigchld_subscribe(ffi_waker);

        Poll::Pending
    }
}

// =============================================================================
// ChildStdin
// =============================================================================

/// A handle to the child process's stdin, implementing `AsyncWrite`.
pub struct ChildStdin {
    fd: OwnedFd,
    reactor_handle: u64,
}

impl Drop for ChildStdin {
    fn drop(&mut self) {
        if debug() {
            eprintln!(
                "[process] drop ChildStdin fd={} handle={}",
                self.fd.as_raw_fd(),
                self.reactor_handle
            );
        }
        tio::deregister(self.reactor_handle);
        // OwnedFd closes the fd on drop
    }
}

impl AsyncWrite for ChildStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ffi_waker = tio::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }

        let n = unsafe { libc::write(this.fd.as_raw_fd(), buf.as_ptr() as *const _, buf.len()) };
        if n >= 0 {
            Poll::Ready(Ok(n as usize))
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                tio::clear_ready(this.reactor_handle, tio::DIR_WRITE);
                let ffi_waker = tio::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            } else {
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Close the fd by dropping
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// ChildStdout
// =============================================================================

/// A handle to the child process's stdout, implementing `AsyncRead`.
pub struct ChildStdout {
    fd: OwnedFd,
    reactor_handle: u64,
}

impl Drop for ChildStdout {
    fn drop(&mut self) {
        if debug() {
            eprintln!(
                "[process] drop ChildStdout fd={} handle={}",
                self.fd.as_raw_fd(),
                self.reactor_handle
            );
        }
        tio::deregister(self.reactor_handle);
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ffi_waker = tio::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }

        let dst = buf.initialize_unfilled();
        let n = unsafe { libc::read(this.fd.as_raw_fd(), dst.as_mut_ptr() as *mut _, dst.len()) };
        if n > 0 {
            buf.advance(n as usize);
            Poll::Ready(Ok(()))
        } else if n == 0 {
            // EOF
            Poll::Ready(Ok(()))
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                tio::clear_ready(this.reactor_handle, tio::DIR_READ);
                let ffi_waker = tio::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            } else {
                Poll::Ready(Err(err))
            }
        }
    }
}

// =============================================================================
// ChildStderr
// =============================================================================

/// A handle to the child process's stderr, implementing `AsyncRead`.
pub struct ChildStderr {
    fd: OwnedFd,
    reactor_handle: u64,
}

impl Drop for ChildStderr {
    fn drop(&mut self) {
        if debug() {
            eprintln!(
                "[process] drop ChildStderr fd={} handle={}",
                self.fd.as_raw_fd(),
                self.reactor_handle
            );
        }
        tio::deregister(self.reactor_handle);
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        let ffi_waker = tio::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }

        let dst = buf.initialize_unfilled();
        let n = unsafe { libc::read(this.fd.as_raw_fd(), dst.as_mut_ptr() as *mut _, dst.len()) };
        if n > 0 {
            buf.advance(n as usize);
            Poll::Ready(Ok(()))
        } else if n == 0 {
            Poll::Ready(Ok(()))
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                tio::clear_ready(this.reactor_handle, tio::DIR_READ);
                let ffi_waker = tio::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            } else {
                Poll::Ready(Err(err))
            }
        }
    }
}
