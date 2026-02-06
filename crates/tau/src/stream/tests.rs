//! Comprehensive tests for all stream combinators and consumers.
//!
//! Covers: map, filter, filter_map, take_while, then, next, collect,
//! for_each, fold, merge.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use futures_core::Stream;

use super::StreamExt;

// ===========================================================================
// Test helpers
// ===========================================================================

/// A simple stream that yields owned items from a vec.
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
            let item = unsafe { core::ptr::read(&this.items[this.index] as *const T) };
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

impl<T> Drop for VecStream<T> {
    fn drop(&mut self) {
        // Only drop items we haven't consumed via ptr::read
        unsafe {
            let ptr = self.items.as_mut_ptr().add(self.index);
            let remaining = self.items.len() - self.index;
            core::ptr::drop_in_place(core::slice::from_raw_parts_mut(ptr, remaining));
            self.items.set_len(0);
        }
    }
}

fn noop_waker() -> Waker {
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// Poll a stream to completion synchronously (only works with always-ready streams).
fn collect_sync<S: Stream + Unpin>(mut stream: S) -> Vec<S::Item> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut items = Vec::new();
    loop {
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(item)) => items.push(item),
            Poll::Ready(None) => break,
            Poll::Pending => panic!("unexpected Pending in collect_sync"),
        }
    }
    items
}

/// Poll a future to completion synchronously (only works with always-ready futures).
fn poll_once<F: Future + Unpin>(mut fut: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut fut).poll(&mut cx) {
        Poll::Ready(val) => val,
        Poll::Pending => panic!("unexpected Pending in poll_once"),
    }
}

// ===========================================================================
// map
// ===========================================================================

#[test]
fn map_transform_items() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5]);
    let mapped = stream.map(|x| x * 10);
    assert_eq!(collect_sync(mapped), vec![10, 20, 30, 40, 50]);
}

#[test]
fn map_type_change() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let mapped = stream.map(|x| format!("item_{}", x));
    assert_eq!(
        collect_sync(mapped),
        vec!["item_1", "item_2", "item_3"]
    );
}

#[test]
fn map_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let mapped = stream.map(|x| x * 2);
    assert_eq!(collect_sync(mapped), Vec::<i32>::new());
}

// ===========================================================================
// filter
// ===========================================================================

#[test]
fn filter_keeps_matching() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5, 6]);
    let filtered = stream.filter(|x| x % 2 == 0);
    assert_eq!(collect_sync(filtered), vec![2, 4, 6]);
}

#[test]
fn filter_drops_all() {
    let stream = VecStream::new(vec![1, 3, 5]);
    let filtered = stream.filter(|_| false);
    assert_eq!(collect_sync(filtered), Vec::<i32>::new());
}

#[test]
fn filter_keeps_all() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let filtered = stream.filter(|_| true);
    assert_eq!(collect_sync(filtered), vec![1, 2, 3]);
}

#[test]
fn filter_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let filtered = stream.filter(|_| true);
    assert_eq!(collect_sync(filtered), Vec::<i32>::new());
}

// ===========================================================================
// filter_map
// ===========================================================================

#[test]
fn filter_map_combined() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5]);
    let fm = stream.filter_map(|x| {
        if x % 2 == 0 {
            Some(x * 100)
        } else {
            None
        }
    });
    assert_eq!(collect_sync(fm), vec![200, 400]);
}

#[test]
fn filter_map_all_some() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let fm = stream.filter_map(|x| Some(x + 1));
    assert_eq!(collect_sync(fm), vec![2, 3, 4]);
}

#[test]
fn filter_map_all_none() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let fm = stream.filter_map(|_: i32| -> Option<i32> { None });
    assert_eq!(collect_sync(fm), Vec::<i32>::new());
}

#[test]
fn filter_map_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let fm = stream.filter_map(|x| Some(x));
    assert_eq!(collect_sync(fm), Vec::<i32>::new());
}

// ===========================================================================
// take_while
// ===========================================================================

#[test]
fn take_while_early_termination() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5]);
    let tw = stream.take_while(|x| *x < 4);
    assert_eq!(collect_sync(tw), vec![1, 2, 3]);
}

#[test]
fn take_while_all_pass() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let tw = stream.take_while(|_| true);
    assert_eq!(collect_sync(tw), vec![1, 2, 3]);
}

#[test]
fn take_while_none_pass() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let tw = stream.take_while(|_| false);
    assert_eq!(collect_sync(tw), Vec::<i32>::new());
}

#[test]
fn take_while_first_item_fails() {
    let stream = VecStream::new(vec![10, 1, 2]);
    let tw = stream.take_while(|x| *x < 5);
    assert_eq!(collect_sync(tw), Vec::<i32>::new());
}

#[test]
fn take_while_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let tw = stream.take_while(|_| true);
    assert_eq!(collect_sync(tw), Vec::<i32>::new());
}

// ===========================================================================
// then (async transform)
// ===========================================================================

#[test]
fn then_sync_future() {
    // Use immediately-ready futures (std::future::ready)
    let stream = VecStream::new(vec![1, 2, 3]);
    let then_stream = stream.then(|x| core::future::ready(x * 10));
    assert_eq!(collect_sync(then_stream), vec![10, 20, 30]);
}

#[test]
fn then_type_change() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let then_stream = stream.then(|x| core::future::ready(format!("v{}", x)));
    assert_eq!(
        collect_sync(then_stream),
        vec!["v1", "v2", "v3"]
    );
}

#[test]
fn then_ordering_preserved() {
    // Even with async, ordering is sequential (one future at a time)
    let stream = VecStream::new(vec![3, 1, 2]);
    let then_stream = stream.then(|x| core::future::ready(x));
    assert_eq!(collect_sync(then_stream), vec![3, 1, 2]);
}

#[test]
fn then_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let then_stream = stream.then(|x| core::future::ready(x));
    assert_eq!(collect_sync(then_stream), Vec::<i32>::new());
}

// ===========================================================================
// next
// ===========================================================================

#[test]
fn next_consume_one_item() {
    let mut stream = VecStream::new(vec![10, 20, 30]);
    let first = poll_once(stream.next());
    assert_eq!(first, Some(10));
    let second = poll_once(stream.next());
    assert_eq!(second, Some(20));
    let third = poll_once(stream.next());
    assert_eq!(third, Some(30));
    let done = poll_once(stream.next());
    assert_eq!(done, None);
}

#[test]
fn next_empty_stream() {
    let mut stream = VecStream::<i32>::new(vec![]);
    let result = poll_once(stream.next());
    assert_eq!(result, None);
}

// ===========================================================================
// collect
// ===========================================================================

#[test]
fn collect_into_vec() {
    let stream = VecStream::new(vec![1, 2, 3, 4]);
    let collected: Vec<i32> = poll_once(stream.collect());
    assert_eq!(collected, vec![1, 2, 3, 4]);
}

#[test]
fn collect_empty() {
    let stream = VecStream::<i32>::new(vec![]);
    let collected: Vec<i32> = poll_once(stream.collect());
    assert!(collected.is_empty());
}

// ===========================================================================
// for_each
// ===========================================================================

#[test]
fn for_each_visits_all() {
    let stream = VecStream::new(vec![1, 2, 3]);
    let mut seen = Vec::new();
    poll_once(stream.for_each(|x| seen.push(x)));
    assert_eq!(seen, vec![1, 2, 3]);
}

#[test]
fn for_each_empty_stream() {
    let stream = VecStream::<i32>::new(vec![]);
    let mut count = 0;
    poll_once(stream.for_each(|_| count += 1));
    assert_eq!(count, 0);
}

// ===========================================================================
// fold
// ===========================================================================

#[test]
fn fold_sum() {
    let stream = VecStream::new(vec![1, 2, 3, 4]);
    let sum: i32 = poll_once(stream.fold(0, |acc, x| acc + x));
    assert_eq!(sum, 10);
}

#[test]
fn fold_product() {
    let stream = VecStream::new(vec![1, 2, 3, 4]);
    let product: i32 = poll_once(stream.fold(1, |acc, x| acc * x));
    assert_eq!(product, 24);
}

#[test]
fn fold_empty_returns_init() {
    let stream = VecStream::<i32>::new(vec![]);
    let result: i32 = poll_once(stream.fold(42, |acc, x| acc + x));
    assert_eq!(result, 42);
}

#[test]
fn fold_build_string() {
    let stream = VecStream::new(vec!["a", "b", "c"]);
    let result: String = poll_once(stream.fold(String::new(), |mut acc, x| {
        acc.push_str(x);
        acc
    }));
    assert_eq!(result, "abc");
}

// ===========================================================================
// merge
// ===========================================================================

#[test]
fn merge_two_producers_all_items_received() {
    let a = VecStream::new(vec![1, 3, 5]);
    let b = VecStream::new(vec![2, 4, 6]);
    let items = collect_sync(a.merge(b));
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
    assert_eq!(items, vec![10, 20]);
}

#[test]
fn merge_both_empty() {
    let a = VecStream::<i32>::new(vec![]);
    let b = VecStream::<i32>::new(vec![]);
    let items = collect_sync(a.merge(b));
    assert!(items.is_empty());
}

#[test]
fn merge_fairness() {
    // With both streams always ready, items should alternate
    let a = VecStream::new(vec![1, 1, 1]);
    let b = VecStream::new(vec![2, 2, 2]);
    let items = collect_sync(a.merge(b));
    // poll_a_first starts true: a,b,a,b,a,b
    assert_eq!(items, vec![1, 2, 1, 2, 1, 2]);
}

// ===========================================================================
// Combinator chaining
// ===========================================================================

#[test]
fn chain_map_filter_collect() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let result: Vec<i32> = poll_once(
        stream
            .map(|x| x * 2)
            .filter(|x| *x > 10)
            .collect(),
    );
    assert_eq!(result, vec![12, 14, 16, 18, 20]);
}

#[test]
fn chain_filter_map_then_fold() {
    let stream = VecStream::new(vec![1, 2, 3, 4, 5]);
    let result: i32 = poll_once(
        stream
            .filter_map(|x| if x % 2 == 1 { Some(x) } else { None })
            .then(|x| core::future::ready(x * 10))
            .fold(0, |acc, x| acc + x),
    );
    // Odd numbers: 1,3,5 → *10 → 10,30,50 → sum = 90
    assert_eq!(result, 90);
}

#[test]
fn chain_take_while_map_collect() {
    let stream = VecStream::new(vec![1, 2, 3, 10, 4, 5]);
    let result: Vec<String> = poll_once(
        stream
            .take_while(|x| *x < 10)
            .map(|x| format!("#{}", x))
            .collect(),
    );
    assert_eq!(result, vec!["#1", "#2", "#3"]);
}

#[test]
fn chain_merge_filter_collect() {
    let a = VecStream::new(vec![1, 2, 3]);
    let b = VecStream::new(vec![4, 5, 6]);
    let result: Vec<i32> = poll_once(
        a.merge(b)
            .filter(|x| *x > 3)
            .collect(),
    );
    let mut sorted = result.clone();
    sorted.sort();
    assert_eq!(sorted, vec![4, 5, 6]);
}
