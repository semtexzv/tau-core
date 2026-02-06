//! Stream combinator structs.
//!
//! Each struct wraps a source stream and a closure, implementing
//! [`Stream`](futures_core::Stream) with the appropriate transformation.
//!
//! **Synchronous** combinators (`Map`, `Filter`, `FilterMap`, `TakeWhile`)
//! use plain `FnMut` closures. **Async** combinators (`Then`) take closures
//! that return futures.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use pin_project_lite::pin_project;

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that applies a function to every item.
    ///
    /// Created by [`StreamExt::map`](super::StreamExt::map).
    #[must_use = "streams do nothing unless polled"]
    pub struct Map<St, F> {
        #[pin]
        stream: St,
        f: F,
    }
}

impl<St, F> Map<St, F> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f }
    }
}

impl<St, F, B> Stream for Map<St, F>
where
    St: Stream,
    F: FnMut(St::Item) -> B,
{
    type Item = B;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<B>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some((this.f)(item))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that yields only items for which a predicate returns `true`.
    ///
    /// Created by [`StreamExt::filter`](super::StreamExt::filter).
    #[must_use = "streams do nothing unless polled"]
    pub struct Filter<St, F> {
        #[pin]
        stream: St,
        f: F,
    }
}

impl<St, F> Filter<St, F> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f }
    }
}

impl<St, F> Stream for Filter<St, F>
where
    St: Stream,
    F: FnMut(&St::Item) -> bool,
{
    type Item = St::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<St::Item>> {
        let mut this = self.project();
        // Loop to skip items that don't match the predicate.
        // `ready!`-style: return Pending if the source isn't ready.
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if (this.f)(&item) {
                        return Poll::Ready(Some(item));
                    }
                    // predicate rejected — try next item
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Lower bound is 0 because every item could be filtered out.
        let (_, upper) = self.stream.size_hint();
        (0, upper)
    }
}

// ---------------------------------------------------------------------------
// FilterMap
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that both filters and maps via `FnMut(Item) -> Option<B>`.
    ///
    /// Created by [`StreamExt::filter_map`](super::StreamExt::filter_map).
    #[must_use = "streams do nothing unless polled"]
    pub struct FilterMap<St, F> {
        #[pin]
        stream: St,
        f: F,
    }
}

impl<St, F> FilterMap<St, F> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f }
    }
}

impl<St, F, B> Stream for FilterMap<St, F>
where
    St: Stream,
    F: FnMut(St::Item) -> Option<B>,
{
    type Item = B;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<B>> {
        let mut this = self.project();
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if let Some(mapped) = (this.f)(item) {
                        return Poll::Ready(Some(mapped));
                    }
                    // closure returned None — skip, try next
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (_, upper) = self.stream.size_hint();
        (0, upper)
    }
}

// ---------------------------------------------------------------------------
// TakeWhile
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that yields items while a predicate returns `true`,
    /// then permanently terminates.
    ///
    /// Created by [`StreamExt::take_while`](super::StreamExt::take_while).
    #[must_use = "streams do nothing unless polled"]
    pub struct TakeWhile<St, F> {
        #[pin]
        stream: St,
        f: F,
        done: bool,
    }
}

impl<St, F> TakeWhile<St, F> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f, done: false }
    }
}

impl<St, F> Stream for TakeWhile<St, F>
where
    St: Stream,
    F: FnMut(&St::Item) -> bool,
{
    type Item = St::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<St::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                if (this.f)(&item) {
                    Poll::Ready(Some(item))
                } else {
                    *this.done = true;
                    Poll::Ready(None)
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.done {
            (0, Some(0))
        } else {
            let (_, upper) = self.stream.size_hint();
            (0, upper)
        }
    }
}

// ---------------------------------------------------------------------------
// Then (async map)
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that applies an async function to every item.
    ///
    /// Only one future is in-flight at a time (sequential async processing).
    ///
    /// Created by [`StreamExt::then`](super::StreamExt::then).
    #[must_use = "streams do nothing unless polled"]
    pub struct Then<St, F, Fut> {
        #[pin]
        stream: St,
        f: F,
        #[pin]
        pending: Option<Fut>,
    }
}

impl<St, F, Fut> Then<St, F, Fut> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f, pending: None }
    }
}

impl<St, F, Fut> Stream for Then<St, F, Fut>
where
    St: Stream,
    F: FnMut(St::Item) -> Fut,
    Fut: Future,
{
    type Item = Fut::Output;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Fut::Output>> {
        let mut this = self.project();

        // If a future is in-flight, poll it first.
        if let Some(fut) = this.pending.as_mut().as_pin_mut() {
            match fut.poll(cx) {
                Poll::Ready(output) => {
                    this.pending.set(None);
                    return Poll::Ready(Some(output));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // No future in-flight — poll the source stream.
        match this.stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                let fut = (this.f)(item);
                this.pending.set(Some(fut));
                // Poll the newly created future immediately.
                // SAFETY: we just set pending to Some, so as_pin_mut() is Some.
                match this.pending.as_mut().as_pin_mut().unwrap().poll(cx) {
                    Poll::Ready(output) => {
                        this.pending.set(None);
                        Poll::Ready(Some(output))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let hint = self.stream.size_hint();
        if self.pending.is_some() {
            // One extra item is in-flight.
            let lo = hint.0.saturating_add(1);
            let hi = hint.1.map(|h| h.saturating_add(1));
            (lo, hi)
        } else {
            hint
        }
    }
}
