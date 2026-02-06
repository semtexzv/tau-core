//! Async stream primitives.
//!
//! Re-exports the standard [`Stream`] trait from `futures-core` so that plugins
//! use the exact same trait the wider async ecosystem depends on (hyper, tower,
//! tonic, kube, reqwest, etc.).
//!
//! Also provides [`StreamExt`] — an extension trait with synchronous
//! combinators (`map`, `filter`, `filter_map`, `take_while`), async
//! combinators (`then`), and terminal consumers (`next`, `collect`,
//! `for_each`, `fold`).

pub mod combinators;
pub mod consumers;

pub use futures_core::Stream;
pub use combinators::{Map, Filter, FilterMap, TakeWhile, Then, Merge};
pub use consumers::{Next, Collect, ForEach, Fold};

/// Extension trait for [`Stream`] providing synchronous combinators.
///
/// All methods consume the source stream and return a new combinator struct
/// that also implements `Stream`.
pub trait StreamExt: Stream + Sized {
    /// Transforms each item using a closure.
    fn map<F, B>(self, f: F) -> Map<Self, F>
    where
        F: FnMut(Self::Item) -> B,
    {
        Map::new(self, f)
    }

    /// Yields only items for which the predicate returns `true`.
    fn filter<F>(self, f: F) -> Filter<Self, F>
    where
        F: FnMut(&Self::Item) -> bool,
    {
        Filter::new(self, f)
    }

    /// Filters and maps in one step. Items for which the closure returns
    /// `None` are skipped; `Some(value)` items are yielded.
    fn filter_map<F, B>(self, f: F) -> FilterMap<Self, F>
    where
        F: FnMut(Self::Item) -> Option<B>,
    {
        FilterMap::new(self, f)
    }

    /// Yields items while the predicate returns `true`. Once the predicate
    /// returns `false`, the stream permanently terminates.
    fn take_while<F>(self, f: F) -> TakeWhile<Self, F>
    where
        F: FnMut(&Self::Item) -> bool,
    {
        TakeWhile::new(self, f)
    }

    /// Applies an async function to each item, yielding the future's output.
    ///
    /// Only one future is in-flight at a time — items are processed
    /// sequentially, not concurrently. This is the async equivalent of
    /// [`map`](StreamExt::map).
    fn then<F, Fut>(self, f: F) -> Then<Self, F, Fut>
    where
        F: FnMut(Self::Item) -> Fut,
        Fut: core::future::Future,
    {
        Then::new(self, f)
    }

    /// Merges two streams into one, interleaving items in arrival order.
    ///
    /// Polls both streams fairly by alternating which is polled first.
    /// The merged stream completes only when **both** inner streams are
    /// exhausted.
    fn merge<S2>(self, other: S2) -> Merge<Self, S2>
    where
        S2: Stream<Item = Self::Item>,
    {
        Merge::new(self, other)
    }

    /// Returns a future that yields the next item from the stream.
    ///
    /// The stream is borrowed, not consumed — it can be used again after
    /// the returned future completes. Requires `Self: Unpin`.
    fn next(&mut self) -> Next<'_, Self>
    where
        Self: Unpin,
    {
        Next::new(self)
    }

    /// Collects all stream items into a container (e.g., `Vec<T>`).
    ///
    /// Consumes the stream. The returned future resolves when the stream
    /// is exhausted.
    fn collect<C: Default + Extend<Self::Item>>(self) -> Collect<Self, C> {
        Collect::new(self)
    }

    /// Calls a closure on every item, consuming the stream.
    ///
    /// The returned future resolves to `()` when the stream is exhausted.
    fn for_each<F>(self, f: F) -> ForEach<Self, F>
    where
        F: FnMut(Self::Item),
    {
        ForEach::new(self, f)
    }

    /// Folds all stream items into a single accumulator value.
    ///
    /// Consumes the stream. The returned future resolves to the final
    /// accumulator when the stream is exhausted.
    fn fold<B, F>(self, init: B, f: F) -> Fold<Self, B, F>
    where
        F: FnMut(B, Self::Item) -> B,
    {
        Fold::new(self, init, f)
    }
}

/// Blanket implementation: every `Stream + Sized` automatically gets `StreamExt`.
impl<S: Stream + Sized> StreamExt for S {}
