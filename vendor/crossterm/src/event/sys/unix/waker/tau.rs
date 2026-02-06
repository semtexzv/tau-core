use std::{
    io,
    sync::{Arc, Mutex},
};

use rustix::fd::{AsFd, OwnedFd};

/// Allows waking up the EventSource::try_read() method.
/// Writes a byte to a pipe; the event source polls the read end.
#[derive(Clone, Debug)]
pub(crate) struct Waker {
    inner: Arc<Mutex<OwnedFd>>,
}

impl Waker {
    /// Create a new `Waker` from the write end of a pipe.
    pub(crate) fn new(writer: OwnedFd) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    /// Wake up the poll loop.
    pub(crate) fn wake(&self) -> io::Result<()> {
        let fd = self.inner.lock().unwrap();
        rustix::io::write(fd.as_fd(), &[0u8])
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        Ok(())
    }

    /// Reset is a no-op for pipe-based wakers (pipe is drained by the reader).
    #[allow(dead_code, clippy::unnecessary_wraps)]
    pub(crate) fn reset(&self) -> io::Result<()> {
        Ok(())
    }
}
