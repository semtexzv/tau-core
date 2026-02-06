//! Time utilities: [`sleep`], [`Sleep`], [`timeout`], [`Elapsed`], [`interval`], [`Interval`].

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use std::time::Duration;

/// A measurement of a monotonically nondecreasing clock.
///
/// This is a thin wrapper around [`std::time::Instant`] that provides
/// the `into_std()` / `from_std()` methods expected by the tokio ecosystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(std::time::Instant);

impl Instant {
    /// Returns an instant corresponding to "now".
    pub fn now() -> Self {
        Self(std::time::Instant::now())
    }

    /// Convert from a `std::time::Instant`.
    pub fn from_std(std_instant: std::time::Instant) -> Self {
        Self(std_instant)
    }

    /// Convert into a `std::time::Instant`.
    pub fn into_std(self) -> std::time::Instant {
        self.0
    }

    /// Returns the amount of time elapsed from another instant to this one,
    /// or zero if that instant is later.
    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }

    /// Returns the amount of time elapsed from another instant to this one.
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0.duration_since(earlier.0)
    }

    /// Returns the amount of time elapsed since this instant was created.
    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    /// Returns `Some(t)` where `t` is `self + duration` if it can be represented,
    /// `None` otherwise.
    pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
        self.0.checked_add(duration).map(Instant)
    }

    /// Returns `Some(t)` where `t` is `self - duration` if it can be represented,
    /// `None` otherwise.
    pub fn checked_sub(&self, duration: Duration) -> Option<Instant> {
        self.0.checked_sub(duration).map(Instant)
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: Duration) -> Instant {
        Instant(self.0 + rhs)
    }
}

impl std::ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs;
    }
}

impl std::ops::Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, rhs: Duration) -> Instant {
        Instant(self.0 - rhs)
    }
}

impl std::ops::SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, rhs: Duration) {
        self.0 -= rhs;
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Duration {
        self.0 - rhs.0
    }
}

impl From<std::time::Instant> for Instant {
    fn from(std_instant: std::time::Instant) -> Self {
        Self(std_instant)
    }
}

impl From<Instant> for std::time::Instant {
    fn from(instant: Instant) -> Self {
        instant.0
    }
}

/// Creates a future that completes after `duration`.
pub fn sleep(duration: Duration) -> Sleep {
    let deadline = Instant::now() + duration;
    Sleep {
        inner: tau::SleepFuture::new(duration),
        deadline,
    }
}

/// Creates a future that completes at `deadline`.
pub fn sleep_until(deadline: Instant) -> Sleep {
    let now = Instant::now();
    let dur = deadline.saturating_duration_since(now);
    Sleep {
        inner: tau::SleepFuture::new(dur),
        deadline,
    }
}

/// A future returned by [`sleep`] and [`sleep_until`].
pub struct Sleep {
    inner: tau::SleepFuture,
    deadline: Instant,
}

impl std::fmt::Debug for Sleep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl Sleep {
    /// Returns the instant at which this sleep will complete.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Returns true if the sleep has elapsed.
    pub fn is_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Resets the sleep to a new deadline.
    pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
        let this = unsafe { self.get_unchecked_mut() };
        let dur = deadline.saturating_duration_since(Instant::now());
        this.inner = tau::SleepFuture::new(dur);
        this.deadline = deadline;
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = unsafe { &mut self.get_unchecked_mut().inner };
        unsafe { Pin::new_unchecked(inner) }.poll(cx)
    }
}

// =============================================================================
// Timeout
// =============================================================================

use pin_project_lite::pin_project;

pin_project! {
    /// Future returned by [`timeout`] and [`timeout_at`].
    #[derive(Debug)]
    pub struct Timeout<T> {
        #[pin]
        value: T,
        #[pin]
        delay: Sleep,
    }
}

impl<T> Timeout<T> {
    /// Returns the inner future.
    pub fn into_inner(self) -> T {
        self.value
    }
    
    /// Returns a reference to the inner future.
    pub fn get_ref(&self) -> &T {
        &self.value
    }
    
    /// Returns a mutable reference to the inner future.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: Future> Future for Timeout<T> {
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // First, check if the value is ready
        if let Poll::Ready(v) = this.value.poll(cx) {
            return Poll::Ready(Ok(v));
        }

        // Check if the delay has elapsed
        if this.delay.poll(cx).is_ready() {
            return Poll::Ready(Err(Elapsed));
        }

        Poll::Pending
    }
}

/// Wraps a future with a timeout duration. Returns `Err(Elapsed)` if the deadline
/// passes before the future completes.
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    let deadline = Instant::now() + duration;
    Timeout {
        value: future,
        delay: sleep_until(deadline),
    }
}

/// Wraps a future with a deadline instant. Returns `Err(Elapsed)` if the deadline
/// passes before the future completes.
pub fn timeout_at<F: Future>(deadline: Instant, future: F) -> Timeout<F> {
    Timeout {
        value: future,
        delay: sleep_until(deadline),
    }
}

/// Error returned by [`timeout`] when the deadline elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

// =============================================================================
// Interval
// =============================================================================

/// Creates a new interval that yields at fixed periods.
pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now() + period, period)
}

/// Creates a new interval starting at `start` and yielding every `period`.
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    Interval {
        delay: sleep_until(start),
        period,
    }
}

/// An interval that yields repeatedly at a fixed period.
/// 
/// Created by [`interval`] and [`interval_at`].
#[derive(Debug)]
pub struct Interval {
    delay: Sleep,
    period: Duration,
}

impl Interval {
    /// Waits until the next tick.
    pub async fn tick(&mut self) -> Instant {
        (&mut self.delay).await;
        let now = Instant::now();
        // Reset for the next tick
        self.delay = sleep(self.period);
        now
    }

    /// Returns the period of the interval.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Resets the interval to complete after `period` from now.
    pub fn reset(&mut self) {
        self.delay = sleep(self.period);
    }

    /// Polls for the next tick.
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        let delay = unsafe { Pin::new_unchecked(&mut self.delay) };
        match delay.poll(cx) {
            Poll::Ready(()) => {
                let now = Instant::now();
                self.delay = sleep(self.period);
                Poll::Ready(now)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Error indicating a missed tick.
#[derive(Debug)]
pub struct MissedTickBehavior;

impl Interval {
    /// Sets the behavior for missed ticks (currently a no-op stub).
    pub fn set_missed_tick_behavior(&mut self, _behavior: MissedTickBehavior) {
        // Simplified: ignore
    }
}
