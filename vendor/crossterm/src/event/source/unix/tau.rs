use std::{collections::VecDeque, io, os::unix::io::AsRawFd, time::Duration};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    source::EventSource, sys::unix::parse::parse_event, timeout::PollTimeout, Event, InternalEvent,
};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: FileDesc<'static>,
    sigwinch_read: OwnedFd,
    #[allow(dead_code)]
    sigwinch_write: OwnedFd, // kept alive so the pipe doesn't close
    #[cfg(feature = "event-stream")]
    wake_read: OwnedFd,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

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

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        // Make tty non-blocking
        rustix::io::ioctl_fionbio(&input_fd, true).map_err(io_err)?;

        // SIGWINCH self-pipe: signal-hook writes to sender, we poll receiver.
        let (sigwinch_read, sigwinch_write) = nonblocking_pipe()?;
        // Register SIGWINCH → writes a byte to sigwinch_write.
        // Uses signal-hook's safe self-pipe pattern.
        signal_hook::low_level::pipe::register_raw(
            signal_hook::consts::SIGWINCH,
            sigwinch_write.as_fd().as_raw_fd(),
        )?;

        // Wake pipe for event-stream
        #[cfg(feature = "event-stream")]
        let (wake_read, wake_write) = nonblocking_pipe()?;
        #[cfg(feature = "event-stream")]
        let waker = Waker::new(wake_write);

        Ok(UnixInternalEventSource {
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty: input_fd,
            sigwinch_read,
            sigwinch_write,
            #[cfg(feature = "event-stream")]
            wake_read,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        let timeout = PollTimeout::new(timeout);

        while timeout.leftover().map_or(true, |t| !t.is_zero()) {
            // Check parser buffer first
            if let Some(event) = self.parser.next() {
                return Ok(Some(event));
            }

            // Build poll fds
            #[cfg(not(feature = "event-stream"))]
            let mut poll_fds = [
                rustix::event::PollFd::new(&self.tty, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&self.sigwinch_read, rustix::event::PollFlags::IN),
            ];

            #[cfg(feature = "event-stream")]
            let mut poll_fds = [
                rustix::event::PollFd::new(&self.tty, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&self.sigwinch_read, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&self.wake_read, rustix::event::PollFlags::IN),
            ];

            // Convert timeout to milliseconds for poll
            let timeout_ms = match timeout.leftover() {
                Some(d) => {
                    let ms = d.as_millis();
                    if ms > i32::MAX as u128 {
                        i32::MAX
                    } else {
                        ms as i32
                    }
                }
                None => -1, // block indefinitely
            };

            match rustix::event::poll(&mut poll_fds, timeout_ms) {
                Ok(_) => {}
                Err(e) if e == rustix::io::Errno::INTR => continue,
                Err(e) => return Err(io_err(e)),
            }

            // Check tty readability
            if poll_fds[0]
                .revents()
                .contains(rustix::event::PollFlags::IN)
            {
                loop {
                    let read_count = read_complete(&self.tty, &mut self.tty_buffer)?;
                    if read_count > 0 {
                        self.parser.advance(
                            &self.tty_buffer[..read_count],
                            read_count == TTY_BUFFER_SIZE,
                        );
                    }

                    if let Some(event) = self.parser.next() {
                        return Ok(Some(event));
                    }

                    if read_count == 0 {
                        break;
                    }
                }
            }

            // Check SIGWINCH
            if poll_fds[1]
                .revents()
                .contains(rustix::event::PollFlags::IN)
            {
                drain_pipe(self.sigwinch_read.as_fd());
                let new_size = crate::terminal::size()?;
                return Ok(Some(InternalEvent::Event(Event::Resize(
                    new_size.0, new_size.1,
                ))));
            }

            // Check wake pipe (event-stream)
            #[cfg(feature = "event-stream")]
            if poll_fds[2]
                .revents()
                .contains(rustix::event::PollFlags::IN)
            {
                drain_pipe(self.wake_read.as_fd());
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }
        }

        Ok(None)
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

//
// Parser — same structure as in mio.rs / tty.rs.
// Buffers raw bytes, parses ANSI escape sequences into InternalEvents.
//
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
