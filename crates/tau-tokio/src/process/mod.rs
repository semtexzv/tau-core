//! Asynchronous process management.
//!
//! Provides [`Command`], [`Child`], [`ChildStdin`], [`ChildStdout`], and
//! [`ChildStderr`] backed by the tau IO reactor.
//!
//! Child process waiting uses a SIGCHLD self-pipe: a `pipe()` registered with
//! the reactor. The SIGCHLD signal handler writes a byte to the pipe, waking
//! all `Child::wait()` futures which then call `try_wait()`.

use crate::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::pin::Pin;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Once;
use std::task::{Context, Poll};
use tau::io as tio;

fn debug() -> bool {
    crate::debug_enabled()
}

// =============================================================================
// SIGCHLD self-pipe (global singleton)
// =============================================================================

/// Global SIGCHLD pipe read-end fd, registered with the tau reactor.
static mut SIGCHLD_READ_FD: RawFd = -1;
static mut SIGCHLD_WRITE_FD: RawFd = -1;
static mut SIGCHLD_REACTOR_HANDLE: u64 = 0;
static SIGCHLD_INIT: Once = Once::new();

/// Initialize the SIGCHLD self-pipe and signal handler.
/// Safe to call multiple times — only runs once.
fn ensure_sigchld_handler() {
    SIGCHLD_INIT.call_once(|| {
        // Create non-blocking pipe
        let mut fds = [0 as libc::c_int; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "pipe() failed");

        let read_fd = fds[0];
        let write_fd = fds[1];

        // Set both ends non-blocking
        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL);
            libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let flags = libc::fcntl(write_fd, libc::F_GETFL);
            libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Register read end with reactor
        let handle = tio::register(read_fd, tio::READABLE)
            .expect("failed to register SIGCHLD pipe with reactor");

        unsafe {
            SIGCHLD_READ_FD = read_fd;
            SIGCHLD_WRITE_FD = write_fd;
            SIGCHLD_REACTOR_HANDLE = handle;
        }

        // Install SIGCHLD handler
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sigchld_handler as usize;
            sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
        }

        if debug() {
            eprintln!(
                "[process] SIGCHLD handler installed: read_fd={}, write_fd={}, handle={}",
                read_fd, write_fd, handle
            );
        }
    });
}

/// Signal handler — writes a byte to the self-pipe. Must be async-signal-safe.
extern "C" fn sigchld_handler(_sig: libc::c_int) {
    let fd = unsafe { SIGCHLD_WRITE_FD };
    if fd >= 0 {
        let buf: [u8; 1] = [1];
        // write() is async-signal-safe per POSIX
        unsafe {
            libc::write(fd, buf.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Drain the SIGCHLD pipe (read all pending bytes).
fn drain_sigchld_pipe() {
    let fd = unsafe { SIGCHLD_READ_FD };
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

/// Poll the SIGCHLD pipe for readability via the reactor.
fn poll_sigchld(cx: &mut Context<'_>) -> Poll<()> {
    let handle = unsafe { SIGCHLD_REACTOR_HANDLE };
    let ffi_waker = tio::make_ffi_waker(cx);
    if tio::poll_ready(handle, tio::DIR_READ, ffi_waker) {
        // Drain the pipe and clear readiness so we get notified again
        drain_sigchld_pipe();
        tio::clear_ready(handle, tio::DIR_READ);
        Poll::Ready(())
    } else {
        Poll::Pending
    }
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
        ensure_sigchld_handler();

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

        // Not exited yet — register with SIGCHLD pipe via reactor
        match poll_sigchld(cx) {
            Poll::Ready(()) => {
                // SIGCHLD received — try again
                match this.child.try_wait() {
                    Ok(Some(status)) => Poll::Ready(Ok(status)),
                    Ok(None) => {
                        // Not our child — re-register waker
                        // We need to poll again on next SIGCHLD
                        Poll::Pending
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Poll::Pending => Poll::Pending,
        }
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
