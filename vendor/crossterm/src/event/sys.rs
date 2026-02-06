#[cfg(all(unix, feature = "event-stream", feature = "events"))]
pub(crate) use unix::waker::Waker;
#[cfg(all(windows, feature = "event-stream", feature = "events"))]
pub(crate) use windows::waker::Waker;

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;
