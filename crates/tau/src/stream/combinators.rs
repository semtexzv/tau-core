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
// Merge
// ---------------------------------------------------------------------------

pin_project! {
    /// Stream adapter that interleaves items from two streams.
    ///
    /// Polls both inner streams fairly by alternating which is polled first.
    /// Completes only when **both** streams are exhausted.
    ///
    /// Created by [`StreamExt::merge`](super::StreamExt::merge).
    #[must_use = "streams do nothing unless polled"]
    pub struct Merge<A, B> {
        #[pin]
        a: A,
        a_done: bool,
        #[pin]
        b: B,
        b_done: bool,
        // Toggled each poll for fairness: true means poll a first.
        poll_a_first: bool,
    }
}

impl<A, B> Merge<A, B> {
    pub(super) fn new(a: A, b: B) -> Self {
        Self {
            a,
            a_done: false,
            b,
            b_done: false,
            poll_a_first: true,
        }
    }
}

impl<A, B> Stream for Merge<A, B>
where
    A: Stream,
    B: Stream<Item = A::Item>,
{
    type Item = A::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<A::Item>> {
        let mut this = self.project();

        // Alternate which stream gets polled first for fairness.
        let poll_a_first = *this.poll_a_first;
        *this.poll_a_first = !poll_a_first;

        if poll_a_first {
            // Try A first, then B.
            if let Some(item) = poll_stream(this.a.as_mut(), this.a_done, cx) {
                return Poll::Ready(Some(item));
            }
            if let Some(item) = poll_stream(this.b.as_mut(), this.b_done, cx) {
                return Poll::Ready(Some(item));
            }
        } else {
            // Try B first, then A.
            if let Some(item) = poll_stream(this.b.as_mut(), this.b_done, cx) {
                return Poll::Ready(Some(item));
            }
            if let Some(item) = poll_stream(this.a.as_mut(), this.a_done, cx) {
                return Poll::Ready(Some(item));
            }
        }

        // Neither stream produced an item.
        if *this.a_done && *this.b_done {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (a_lo, a_hi) = self.a.size_hint();
        let (b_lo, b_hi) = self.b.size_hint();
        let lo = a_lo.saturating_add(b_lo);
        let hi = match (a_hi, b_hi) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
        (lo, hi)
    }
}

/// Helper: poll a single stream, updating its `done` flag.
/// Returns `Some(item)` if an item was produced, `None` otherwise.
fn poll_stream<St: Stream>(
    stream: Pin<&mut St>,
    done: &mut bool,
    cx: &mut Context<'_>,
) -> Option<St::Item> {
    if *done {
        return None;
    }
    match stream.poll_next(cx) {
        Poll::Ready(Some(item)) => Some(item),
        Poll::Ready(None) => {
            *done = true;
            None
        }
        Poll::Pending => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamExt;
    use core::task::{RawWaker, RawWakerVTable, Waker};

    // A simple stream that yields items from a vec.
    struct VecStream<T> {
        items: Vec<T>,
        index: usize,
    }

    impl<T> VecStream<T> {
        fn new(items: Vec<T>) -> Self {
            Self { items, index: 0 }
        }
    }

    impl<T: Unpin> Stream for VecStream<T> {
        type Item = T;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
            let this = self.get_mut();
            if this.index < this.items.len() {
                // Move the item out by swapping with a default... or use unsafe.
                // Simpler: store Option<T> internally.
                let item = unsafe {
                    core::ptr::read(&this.items[this.index] as *const T)
                };
                this.index += 1;
                Poll::Ready(Some(item))
            } else {
                Poll::Ready(None)
            }
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.items.len() - self.index;
            (remaining, Some(remaining))
        }
    }

    // Don't drop the vec items normally since we moved them out via ptr::read
    impl<T> Drop for VecStream<T> {
        fn drop(&mut self) {
            // Only drop items we haven't consumed
            unsafe {
                let ptr = self.items.as_mut_ptr().add(self.index);
                let remaining = self.items.len() - self.index;
                core::ptr::drop_in_place(core::slice::from_raw_parts_mut(ptr, remaining));
                self.items.set_len(0); // prevent Vec from dropping already-moved items
            }
        }
    }

    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker { RawWaker::new(p, &VTABLE) }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// Poll a stream to completion synchronously (only works with ready streams).
    fn collect_sync<S: Stream + Unpin>(mut stream: S) -> Vec<S::Item> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut items = Vec::new();
        loop {
            match Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(item)) => items.push(item),
                Poll::Ready(None) => break,
                Poll::Pending => panic!("unexpected Pending in sync collect"),
            }
        }
        items
    }

    #[test]
    fn merge_both_streams() {
        let a = VecStream::new(vec![1, 3, 5]);
        let b = VecStream::new(vec![2, 4, 6]);
        let merged = a.merge(b);
        let items = collect_sync(merged);
        // All 6 items must be present
        assert_eq!(items.len(), 6);
        let mut sorted = items.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn merge_one_empty() {
        let a = VecStream::new(vec![10, 20]);
        let b = VecStream::<i32>::new(vec![]);
        let items = collect_sync(a.merge(b));
        assert_eq!(items.len(), 2);
        assert!(items.contains(&10));
        assert!(items.contains(&20));
    }

    #[test]
    fn merge_both_empty() {
        let a = VecStream::<i32>::new(vec![]);
        let b = VecStream::<i32>::new(vec![]);
        let items = collect_sync(a.merge(b));
        assert!(items.is_empty());
    }

    #[test]
    fn merge_fairness_alternates() {
        // Both streams always ready, check that items alternate
        let a = VecStream::new(vec![1, 1, 1]);
        let b = VecStream::new(vec![2, 2, 2]);
        let items = collect_sync(a.merge(b));
        // First poll: a first (poll_a_first starts true) → yields 1
        // Second poll: b first → yields 2
        // Third poll: a first → yields 1
        // ... etc
        assert_eq!(items, vec![1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn merge_size_hint() {
        let a = VecStream::new(vec![1, 2, 3]);
        let b = VecStream::new(vec![4, 5]);
        let merged = a.merge(b);
        assert_eq!(merged.size_hint(), (5, Some(5)));
    }
}
