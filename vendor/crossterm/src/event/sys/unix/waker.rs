#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod tau;

// Keep mio.rs around but unused — original upstream reference.
#[cfg(any())]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::Waker;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::tau::Waker;
