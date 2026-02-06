//! Terminal stream consumers — futures that consume a stream to completion.
//!
//! Unlike combinators (which return new streams), these return **futures**
//! that the caller `.await`s.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use pin_project_lite::pin_project;

// ---------------------------------------------------------------------------
// Next
// ---------------------------------------------------------------------------

/// Future that yields the next item from a stream.
///
/// Created by [`StreamExt::next`](super::StreamExt::next).
///
/// Borrows the stream mutably; the stream can be reused after awaiting.
/// Requires `St: Unpin` because it takes `&mut St`.
#[must_use = "futures do nothing unless awaited"]
pub struct Next<'a, St: ?Sized> {
    stream: &'a mut St,
}

impl<'a, St: ?Sized> Next<'a, St> {
    pub(super) fn new(stream: &'a mut St) -> Self {
        Self { stream }
    }
}

impl<'a, St> Future for Next<'a, St>
where
    St: Stream + Unpin + ?Sized,
{
    type Output = Option<St::Item>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut *self.stream).poll_next(cx)
    }
}

// ---------------------------------------------------------------------------
// Collect
// ---------------------------------------------------------------------------

pin_project! {
    /// Future that collects all stream items into a container.
    ///
    /// Created by [`StreamExt::collect`](super::StreamExt::collect).
    #[must_use = "futures do nothing unless awaited"]
    pub struct Collect<St, C> {
        #[pin]
        stream: St,
        collection: C,
    }
}

impl<St, C> Collect<St, C>
where
    C: Default,
{
    pub(super) fn new(stream: St) -> Self {
        Self {
            stream,
            collection: C::default(),
        }
    }
}

impl<St, C> Future for Collect<St, C>
where
    St: Stream,
    C: Default + Extend<St::Item>,
{
    type Output = C;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<C> {
        let mut this = self.project();
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    this.collection.extend(core::iter::once(item));
                }
                Poll::Ready(None) => {
                    return Poll::Ready(core::mem::take(this.collection));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ForEach
// ---------------------------------------------------------------------------

pin_project! {
    /// Future that calls a closure on every stream item, then completes.
    ///
    /// Created by [`StreamExt::for_each`](super::StreamExt::for_each).
    #[must_use = "futures do nothing unless awaited"]
    pub struct ForEach<St, F> {
        #[pin]
        stream: St,
        f: F,
    }
}

impl<St, F> ForEach<St, F> {
    pub(super) fn new(stream: St, f: F) -> Self {
        Self { stream, f }
    }
}

impl<St, F> Future for ForEach<St, F>
where
    St: Stream,
    F: FnMut(St::Item),
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    (this.f)(item);
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fold
// ---------------------------------------------------------------------------

pin_project! {
    /// Future that folds all stream items into a single value.
    ///
    /// Created by [`StreamExt::fold`](super::StreamExt::fold).
    #[must_use = "futures do nothing unless awaited"]
    pub struct Fold<St, B, F> {
        #[pin]
        stream: St,
        acc: Option<B>,
        f: F,
    }
}

impl<St, B, F> Fold<St, B, F> {
    pub(super) fn new(stream: St, init: B, f: F) -> Self {
        Self {
            stream,
            acc: Some(init),
            f,
        }
    }
}

impl<St, B, F> Future for Fold<St, B, F>
where
    St: Stream,
    F: FnMut(B, St::Item) -> B,
{
    type Output = B;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<B> {
        let mut this = self.project();
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    let acc = this.acc.take().unwrap();
                    *this.acc = Some((this.f)(acc, item));
                }
                Poll::Ready(None) => {
                    return Poll::Ready(this.acc.take().unwrap());
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
