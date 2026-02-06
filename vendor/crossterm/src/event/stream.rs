use std::{
    collections::VecDeque,
    io,
    os::unix::io::AsRawFd,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::stream::Stream;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use tau::io::AsyncFd;

use crate::event::{sys::unix::parse::parse_event, Event, InternalEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

const TTY_BUFFER_SIZE: usize = 1_024;

// ── Pipe helpers (shared with sync source) ──────────────────────────────

/// Create a non-blocking pipe pair.
fn nonblocking_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = rustix::pipe::pipe().map_err(io_err)?;
    rustix::io::ioctl_fionbio(&read, true).map_err(io_err)?;
    rustix::io::ioctl_fionbio(&write, true).map_err(io_err)?;
    Ok((read, write))
}

fn io_err(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

/// Read all available data from a non-blocking fd, discarding it.
fn drain_pipe(fd: BorrowedFd<'_>) {
    let mut buf = [0u8; 64];
    loop {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break, // EAGAIN/EWOULDBLOCK
        }
    }
}

/// Read from a non-blocking FileDesc, handling WouldBlock and Interrupted.
fn read_complete(fd: &FileDesc, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match fd.read(buf) {
            Ok(x) => return Ok(x),
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(0),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            },
        }
    }
}

// ── Parser (same as sync source) ────────────────────────────────────────

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
                Ok(None) => {}
                Err(_) => {
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
/// **Requires the `event-stream` feature.**
///
/// Uses `tau::io::AsyncFd` to poll the tty and a SIGWINCH self-pipe
/// through the tau reactor. No background threads needed.
pub struct EventStream {
    /// AsyncFd wrapping the tty file descriptor.
    tty_async: AsyncFd,
    /// AsyncFd wrapping the read-end of the SIGWINCH self-pipe.
    sigwinch_async: AsyncFd,
    /// The tty fd for byte reads.
    tty: FileDesc<'static>,
    /// Read-end of sigwinch pipe (kept alive; AsyncFd doesn't own the fd).
    sigwinch_read: OwnedFd,
    /// Write-end of sigwinch pipe (kept alive so signal handler can write).
    #[allow(dead_code)]
    sigwinch_write: OwnedFd,
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

impl EventStream {
    /// Constructs a new `EventStream`.
    pub fn new() -> EventStream {
        EventStream::default()
    }

    /// Returns the next event from the stream.
    ///
    /// Convenience async method — equivalent to `StreamExt::next(self)`.
    pub async fn next(&mut self) -> Option<io::Result<Event>> {
        std::future::poll_fn(|cx| Stream::poll_next(Pin::new(&mut *self), cx)).await
    }
}

impl Default for EventStream {
    fn default() -> Self {
        let tty = tty_fd().expect("failed to open tty");
        let tty_raw = tty.as_raw_fd();

        // Set tty non-blocking.
        rustix::io::ioctl_fionbio(&tty, true).expect("failed to set tty non-blocking");

        // Register tty with reactor (read interest only).
        let tty_async =
            AsyncFd::with_interest(tty_raw, tau::io::READABLE).expect("failed to register tty");

        // SIGWINCH self-pipe.
        let (sigwinch_read, sigwinch_write) = nonblocking_pipe().expect("failed to create pipe");

        // Register SIGWINCH → writes a byte to sigwinch_write.
        signal_hook::low_level::pipe::register_raw(
            signal_hook::consts::SIGWINCH,
            sigwinch_write.as_fd().as_raw_fd(),
        )
        .expect("failed to register SIGWINCH handler");

        // Register pipe read-end with reactor.
        let sigwinch_async =
            AsyncFd::with_interest(sigwinch_read.as_raw_fd(), tau::io::READABLE)
                .expect("failed to register sigwinch pipe");

        EventStream {
            tty_async,
            sigwinch_async,
            tty,
            sigwinch_read,
            sigwinch_write,
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
        }
    }
}

impl Stream for EventStream {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // 1. Return any already-buffered event from the parser.
        if let Some(event) = drain_next_event(&mut this.parser) {
            return Poll::Ready(Some(event));
        }

        // 2. Poll SIGWINCH self-pipe for readability.
        if this.sigwinch_async.poll_read_ready(cx).is_ready() {
            // Drain all bytes from the pipe (multiple signals may have queued).
            drain_pipe(this.sigwinch_read.as_fd());
            // Clear readiness so next poll re-checks the reactor.
            this.sigwinch_async.clear_read_ready();
            let size = crate::terminal::size().unwrap_or((80, 24));
            return Poll::Ready(Some(Ok(Event::Resize(size.0, size.1))));
        }

        // 3. Poll tty for readability.
        if this.tty_async.poll_read_ready(cx).is_ready() {
            // Read all available bytes from tty.
            loop {
                let n = read_complete(&this.tty, &mut this.tty_buffer)?;
                if n > 0 {
                    this.parser
                        .advance(&this.tty_buffer[..n], n == TTY_BUFFER_SIZE);
                }
                if n < TTY_BUFFER_SIZE {
                    break;
                }
            }

            // Clear readiness — we've consumed all available data.
            this.tty_async.clear_read_ready();

            if let Some(event) = drain_next_event(&mut this.parser) {
                return Poll::Ready(Some(event));
            }
        }

        // 4. Neither fd is ready — wakers have been registered by poll_read_ready.
        Poll::Pending
    }
}

/// Pull the next `Event` from the parser, skipping non-public `InternalEvent`s.
fn drain_next_event(parser: &mut Parser) -> Option<io::Result<Event>> {
    while let Some(internal) = parser.next() {
        match internal {
            InternalEvent::Event(event) => return Some(Ok(event)),
            #[allow(unreachable_patterns)]
            _ => continue,
        }
    }
    None
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // Restore default SIGWINCH handler.
        // signal-hook doesn't provide unregister_raw, but SIG_DFL via libc works.
        unsafe {
            libc_signal(signal_hook::consts::SIGWINCH, SIG_DFL);
        }

        // After this body: tty_async drops (deregisters tty from reactor),
        // sigwinch_async drops (deregisters pipe read from reactor),
        // sigwinch_read/sigwinch_write close (OwnedFd Drop), tty closes.
    }
}

// Minimal libc signal() binding — just enough to restore SIG_DFL.
const SIG_DFL: usize = 0;

extern "C" {
    #[link_name = "signal"]
    fn libc_signal(signum: i32, handler: usize) -> usize;
}
