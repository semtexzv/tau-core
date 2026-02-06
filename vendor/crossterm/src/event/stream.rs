use std::{
    collections::VecDeque,
    ffi::c_int,
    io,
    os::unix::io::AsRawFd,
    pin::Pin,
    sync::atomic::{AtomicI32, Ordering},
    task::{Context, Poll},
};

use async_ffi::ContextExt;
use futures_core::stream::Stream;
use tau_io::AsyncFd;

use crate::event::{sys::unix::parse::parse_event, Event, InternalEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

const TTY_BUFFER_SIZE: usize = 1_024;

// ── Self-pipe for SIGWINCH notification ──────────────────────────────────

/// Write-end of the self-pipe. Signal handler writes 1 byte here.
static SIGWINCH_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// Raw FFI for the handful of libc functions we need. Avoids pulling
/// in the entire `libc` crate (crossterm defaults to `rustix`).
mod raw {
    use std::ffi::c_int;

    pub const SIGWINCH: c_int = 28; // same on Linux and macOS
    pub const O_NONBLOCK: c_int = {
        #[cfg(target_os = "linux")]
        {
            0o4000
        }
        #[cfg(not(target_os = "linux"))]
        {
            4
        } // macOS / BSD
    };
    pub const F_GETFL: c_int = 3;
    pub const F_SETFL: c_int = 4;
    pub const SIG_DFL: usize = 0;

    extern "C" {
        pub fn pipe(pipefd: *mut c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
        pub fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
        pub fn close(fd: c_int) -> c_int;
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        pub fn signal(signum: c_int, handler: usize) -> usize;
    }
}

/// SIGWINCH handler — writes a single byte to the self-pipe.
/// Must be async-signal-safe (only uses `write` on a pre-set fd).
unsafe extern "C" fn sigwinch_handler(_signum: c_int) {
    let fd = SIGWINCH_PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { raw::write(fd, &1u8 as *const u8, 1) };
    }
}

// ── Parser (inline copy from source/unix/mio.rs) ────────────────────────

/// Parses raw terminal bytes into `InternalEvent`s.
///
/// Mirrors the `Parser` from `source/unix/mio.rs` — a small buffer that
/// accumulates bytes and feeds them to `parse_event()`.
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            buffer: Vec::with_capacity(256),
            internal_events: VecDeque::with_capacity(128),
        }
    }
}

impl Parser {
    fn advance(&mut self, buffer: &[u8], more: bool) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;
            self.buffer.push(*byte);

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Not enough bytes yet — keep buffer.
                }
                Err(_) => {
                    // Unparseable sequence — discard and continue.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;
    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

// ── EventStream ─────────────────────────────────────────────────────────

/// A stream of `Result<Event>`.
///
/// **This type is not available by default. You have to use the
/// `event-stream` feature flag to make it available.**
///
/// This implementation uses `tau_io::AsyncFd` to poll stdin for
/// readability through the tau-rt reactor — no background threads needed.
/// SIGWINCH is detected via a self-pipe whose read-end is also registered
/// with the reactor.
pub struct EventStream {
    /// AsyncFd wrapping the tty file descriptor (stdin or /dev/tty).
    stdin_fd: AsyncFd,
    /// AsyncFd wrapping the read-end of the SIGWINCH self-pipe.
    sigwinch_fd: AsyncFd,
    /// Raw fd of the SIGWINCH pipe read-end (for drain reads in poll_next).
    sigwinch_read_raw: c_int,
    /// The underlying tty file descriptor (for raw byte reads).
    tty: FileDesc<'static>,
    /// Parser: raw bytes → InternalEvent.
    parser: Parser,
    /// Read buffer for tty bytes.
    tty_buffer: [u8; TTY_BUFFER_SIZE],
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish()
    }
}

impl Default for EventStream {
    fn default() -> Self {
        let tty = tty_fd().expect("failed to open tty");
        let tty_raw = tty.as_raw_fd();

        // Set tty to non-blocking so reads don't hang after a spurious wake.
        unsafe {
            let flags = raw::fcntl(tty_raw, raw::F_GETFL);
            raw::fcntl(tty_raw, raw::F_SETFL, flags | raw::O_NONBLOCK);
        }

        // Register tty with the tau-rt reactor.
        let stdin_fd =
            AsyncFd::new(tty_raw).expect("failed to register tty fd with reactor");

        // Create self-pipe for SIGWINCH.
        let mut pipe_fds = [0i32; 2];
        let rc = unsafe { raw::pipe(pipe_fds.as_mut_ptr()) };
        assert!(rc == 0, "pipe() failed");
        unsafe {
            let fl0 = raw::fcntl(pipe_fds[0], raw::F_GETFL);
            raw::fcntl(pipe_fds[0], raw::F_SETFL, fl0 | raw::O_NONBLOCK);
            let fl1 = raw::fcntl(pipe_fds[1], raw::F_GETFL);
            raw::fcntl(pipe_fds[1], raw::F_SETFL, fl1 | raw::O_NONBLOCK);
        }

        // Store write-end globally for signal handler.
        SIGWINCH_PIPE_WRITE.store(pipe_fds[1], Ordering::SeqCst);

        // Install SIGWINCH handler.
        unsafe {
            raw::signal(raw::SIGWINCH, sigwinch_handler as *const () as usize);
        }

        // Register pipe read-end with the reactor.
        let sigwinch_fd =
            AsyncFd::new(pipe_fds[0]).expect("failed to register sigwinch pipe with reactor");

        EventStream {
            stdin_fd,
            sigwinch_fd,
            sigwinch_read_raw: pipe_fds[0],
            tty,
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
        }
    }
}

impl EventStream {
    /// Constructs a new instance of `EventStream`.
    pub fn new() -> EventStream {
        EventStream::default()
    }

    /// Returns the next event from the stream.
    ///
    /// Convenience method that avoids requiring `futures::StreamExt` in
    /// downstream crates. Equivalent to `StreamExt::next(self)`.
    pub async fn next(&mut self) -> Option<io::Result<Event>> {
        std::future::poll_fn(|cx| {
            Stream::poll_next(Pin::new(&mut *self), cx)
        })
        .await
    }
}

impl Stream for EventStream {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // 1. Return any already-buffered event.
        if let Some(event) = drain_next_event(&mut this.parser) {
            return Poll::Ready(Some(event));
        }

        // 2. Poll the SIGWINCH self-pipe for readability.
        //    Register waker with the reactor so we're woken on signal.
        let sigwinch_ready = poll_readable(&this.sigwinch_fd, cx);
        if sigwinch_ready {
            // Drain all bytes from the pipe (multiple signals may have queued).
            let mut buf = [0u8; 64];
            loop {
                let n = unsafe {
                    raw::read(
                        this.sigwinch_read_raw,
                        buf.as_mut_ptr(),
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
            }
            let size = crate::terminal::size().unwrap_or((80, 24));
            return Poll::Ready(Some(Ok(Event::Resize(size.0, size.1))));
        }

        // 3. Poll stdin for readability.
        let stdin_ready = poll_readable(&this.stdin_fd, cx);
        if stdin_ready {
            // Read all available bytes from tty.
            loop {
                match this.tty.read(&mut this.tty_buffer) {
                    Ok(n) if n > 0 => {
                        this.parser
                            .advance(&this.tty_buffer[..n], n == TTY_BUFFER_SIZE);
                    }
                    Ok(_) => break, // EOF or 0 bytes
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                }
            }

            if let Some(event) = drain_next_event(&mut this.parser) {
                return Poll::Ready(Some(event));
            }
        }

        // 4. Neither fd is ready — return Pending.
        //    Wakers have been registered with the reactor for both fds.
        Poll::Pending
    }
}

/// Poll an `AsyncFd` for readability within a sync `poll_next` context.
///
/// Uses `ContextExt::with_ffi_context` to bridge between `std::task::Context`
/// and `async_ffi::FfiContext` required by the tau-rt FFI.
fn poll_readable(fd: &AsyncFd, cx: &mut Context<'_>) -> bool {
    cx.with_ffi_context(|ffi_cx| {
        let result = unsafe {
            tau_io::ffi::tau_rt_io_poll_readable(fd.handle(), ffi_cx as *mut _)
        };
        result == 1
    })
}

/// Pull the next `Event` from the parser, skipping non-public `InternalEvent`s.
fn drain_next_event(parser: &mut Parser) -> Option<io::Result<Event>> {
    while let Some(internal) = parser.next() {
        match internal {
            InternalEvent::Event(event) => return Some(Ok(event)),
            // Skip internal-only events (e.g. CursorPosition on unix).
            #[allow(unreachable_patterns)]
            _ => continue,
        }
    }
    None
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // Close the write-end of the self-pipe (stops signal handler writes).
        let write_fd = SIGWINCH_PIPE_WRITE.swap(-1, Ordering::SeqCst);
        if write_fd >= 0 {
            unsafe { raw::close(write_fd) };
        }

        // Restore default SIGWINCH handler.
        unsafe {
            raw::signal(raw::SIGWINCH, raw::SIG_DFL);
        }

        // After drop() body returns, struct fields drop in declaration order:
        //   stdin_fd (AsyncFd) → deregisters tty from reactor
        //   sigwinch_fd (AsyncFd) → deregisters pipe read-end from reactor
        //   sigwinch_read_raw (c_int) → no-op (primitives don't drop)
        //   tty (FileDesc) → closes tty fd if owned
        //
        // The pipe read-end raw fd is NOT auto-closed by either AsyncFd
        // (which doesn't own fds) or c_int (a primitive). We close it here.
        // The reactor's io_deregister already ignores EBADF from closed fds,
        // so it's safe that sigwinch_fd's destructor runs after this close.
        unsafe { raw::close(self.sigwinch_read_raw) };
    }
}
