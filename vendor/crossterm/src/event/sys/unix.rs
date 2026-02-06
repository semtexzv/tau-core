#[cfg(all(feature = "event-stream", feature = "events"))]
pub(crate) mod waker;

#[cfg(any(feature = "events", feature = "event-stream"))]
pub(crate) mod parse;
