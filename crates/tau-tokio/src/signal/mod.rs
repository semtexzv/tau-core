//! Signal handling (stubs).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Returns a future that completes on Ctrl+C.
pub fn ctrl_c() -> CtrlC {
    CtrlC
}

/// Future returned by [`ctrl_c`].
pub struct CtrlC;

impl Future for CtrlC {
    type Output = std::io::Result<()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // In a real implementation, this would register with the OS signal handler.
        // For now, just pend forever (signals won't be caught).
        Poll::Pending
    }
}

/// Unix-specific signal handling.
#[cfg(unix)]
pub mod unix {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Types of Unix signals.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SignalKind(i32);

    impl SignalKind {
        /// Create a `SignalKind` from a raw signal number.
        pub const fn from_raw(signum: i32) -> Self {
            SignalKind(signum)
        }

        /// SIGTERM
        pub const fn terminate() -> Self {
            SignalKind(15) // SIGTERM
        }

        /// SIGHUP
        pub const fn hangup() -> Self {
            SignalKind(1) // SIGHUP
        }

        /// SIGINT
        pub const fn interrupt() -> Self {
            SignalKind(2) // SIGINT
        }

        /// SIGQUIT
        pub const fn quit() -> Self {
            SignalKind(3) // SIGQUIT
        }

        /// SIGUSR1
        pub const fn user_defined1() -> Self {
            SignalKind(10) // SIGUSR1
        }

        /// SIGUSR2
        pub const fn user_defined2() -> Self {
            SignalKind(12) // SIGUSR2
        }
    }

    /// Create a listener for a Unix signal.
    pub fn signal(_kind: SignalKind) -> std::io::Result<Signal> {
        Ok(Signal)
    }

    /// A stream of signals.
    pub struct Signal;

    impl Signal {
        /// Receive the next signal.
        pub async fn recv(&mut self) -> Option<()> {
            // Never returns in this stub implementation
            std::future::pending().await
        }
    }

    impl Future for Signal {
        type Output = Option<()>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }
}
