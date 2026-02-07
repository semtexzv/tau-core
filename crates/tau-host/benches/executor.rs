//! Executor benchmarks — comparing tau-host executor with smol.
//!
//! Run with: cargo bench -p tau-host

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// Import executor internals
use tau_host::runtime::executor;

// =============================================================================
// Minimal spawn/block_on for benchmarking (avoids tau-rt indirection)
// =============================================================================

const INLINE_SIZE: usize = 80;

type PollFn = unsafe fn(*mut u8, &Waker) -> Poll<()>;
type DropResultFn = unsafe fn(*mut u8);
type DropFutureFn = unsafe fn(*mut u8);
type ReadFn = unsafe fn(*mut u8) -> *mut u8;
type DeallocFn = unsafe fn(*mut u8);

union Payload<F, T> {
    future: ManuallyDrop<F>,
    result: ManuallyDrop<T>,
}

unsafe fn poll_inline<F: Future<Output = T>, T>(ptr: *mut u8, waker: &Waker) -> Poll<()> {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    let future = Pin::new_unchecked(&mut *payload.future);
    let mut cx = Context::from_waker(waker);
    match future.poll(&mut cx) {
        Poll::Ready(v) => {
            ManuallyDrop::drop(&mut payload.future);
            std::ptr::write(&mut payload.result, ManuallyDrop::new(v));
            Poll::Ready(())
        }
        Poll::Pending => Poll::Pending,
    }
}

unsafe fn drop_inline_result<F: Future<Output = T>, T>(ptr: *mut u8) {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    ManuallyDrop::drop(&mut payload.result);
}

unsafe fn cancel_inline<F: Future<Output = T>, T>(ptr: *mut u8) {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    ManuallyDrop::drop(&mut payload.future);
}

unsafe fn read_inline<F: Future<Output = T>, T>(ptr: *mut u8) -> *mut u8 {
    let payload = &mut *(ptr as *mut Payload<F, T>);
    &mut *payload.result as *mut T as *mut u8
}

struct BenchHandle<T> {
    id: u32,
    gen: u32,
    executor_idx: u16,
    _marker: std::marker::PhantomData<T>,
}

impl<T> Unpin for BenchHandle<T> {}

fn bench_spawn<F, T>(f: F) -> BenchHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    let (id, gen, storage, executor_idx) = executor::alloc_raw();
    
    let payload_size = std::mem::size_of::<Payload<F, T>>();
    let payload_align = std::mem::align_of::<Payload<F, T>>();
    
    if payload_size <= INLINE_SIZE && payload_align <= 128 {
        unsafe {
            let ptr = storage as *mut Payload<F, T>;
            std::ptr::write(ptr, Payload { future: ManuallyDrop::new(f) });
        }
        executor::commit_spawn(
            id,
            poll_inline::<F, T>,
            Some(drop_inline_result::<F, T>),
            Some(cancel_inline::<F, T>),
            Some(read_inline::<F, T>),
            None,
            0,
        );
    } else {
        panic!("boxed spawn not implemented in bench");
    }
    
    BenchHandle { id, gen, executor_idx, _marker: std::marker::PhantomData }
}

impl<T> BenchHandle<T> {
    fn poll_take(&mut self) -> Option<T> {
        let (state, _gen) = executor::tau_exec_slot_state(self.id, self.executor_idx);
        if state == 3 { // STATE_COMPLETE
            let (ptr, _dealloc) = executor::tau_exec_take_result(self.id, self.executor_idx);
            let result = unsafe { std::ptr::read(ptr as *mut T) };
            executor::tau_exec_dealloc_storage(self.id, self.executor_idx);
            Some(result)
        } else {
            None
        }
    }
    
    fn cancel(&mut self) {
        executor::cancel_task(self.id, self.gen, self.executor_idx);
    }
}

impl<T> Future for BenchHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        let (state, _gen) = executor::tau_exec_slot_state(this.id, this.executor_idx);
        if state == 3 { // STATE_COMPLETE
            let (ptr, _dealloc) = executor::tau_exec_take_result(this.id, this.executor_idx);
            let result = unsafe { std::ptr::read(ptr as *mut T) };
            executor::tau_exec_dealloc_storage(this.id, this.executor_idx);
            Poll::Ready(result)
        } else {
            executor::tau_exec_set_join_waker(this.id, this.executor_idx, cx.waker().clone());
            Poll::Pending
        }
    }
}

fn bench_block_on<F: Future>(f: F) -> F::Output {
    fn noop_clone(d: *const ()) -> RawWaker { RawWaker::new(d, &MAIN_VTABLE) }
    fn noop(_: *const ()) {}
    static MAIN_VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);
    
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &MAIN_VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = std::pin::pin!(f);
    
    loop {
        if let Poll::Ready(v) = pinned.as_mut().poll(&mut cx) {
            return v;
        }
        executor::drive_cycle();
    }
}

// =============================================================================
// SMOL BENCHMARKS
// =============================================================================

fn smol_spawn_1k(c: &mut Criterion) {
    c.bench_function("smol_spawn_1k", |b| {
        b.iter(|| {
            smol::block_on(async {
                let mut handles = Vec::with_capacity(1_000);
                for i in 0..1_000 {
                    handles.push(smol::spawn(async move { black_box(i) }));
                }
                for h in handles {
                    h.await;
                }
            });
        });
    });
}

fn smol_pingpong_1k(c: &mut Criterion) {
    use futures::channel::oneshot;
    c.bench_function("smol_pingpong_1k", |b| {
        b.iter(|| {
            smol::block_on(async {
                for _ in 0..1_000u32 {
                    let (tx1, rx1) = oneshot::channel::<u32>();
                    let (tx2, rx2) = oneshot::channel::<u32>();

                    let pinger = smol::spawn(async move {
                        tx1.send(42).ok();
                        rx2.await.ok()
                    });

                    let ponger = smol::spawn(async move {
                        let v = rx1.await.unwrap();
                        tx2.send(v).ok();
                    });

                    pinger.await;
                    ponger.await;
                }
            });
        });
    });
}

// =============================================================================
// TAU BENCHMARKS
// =============================================================================

fn tau_spawn_1k(c: &mut Criterion) {
    executor::init();
    c.bench_function("tau_spawn_1k", |b| {
        b.iter(|| {
            bench_block_on(async {
                let mut handles = Vec::with_capacity(1_000);
                for i in 0..1_000 {
                    handles.push(bench_spawn(async move { black_box(i) }));
                }
                for h in handles {
                    h.await;
                }
            });
        });
    });
}

fn tau_spawn_cancel_50pct(c: &mut Criterion) {
    executor::init();
    c.bench_function("tau_spawn_cancel_50pct", |b| {
        b.iter(|| {
            let mut handles: Vec<BenchHandle<i32>> = Vec::with_capacity(1000);
            for i in 0..1000i32 {
                handles.push(bench_spawn(async move { black_box(i) }));
            }
            
            // Cancel 50% of tasks
            for h in handles.iter_mut().step_by(2) {
                h.cancel();
            }
            
            // Complete remaining tasks
            bench_block_on(async {
                for h in handles.into_iter().skip(1).step_by(2) {
                    h.await;
                }
            });
        });
    });
}

fn tau_parallel_1k(c: &mut Criterion) {
    executor::init();
    c.bench_function("tau_parallel_1k", |b| {
        b.iter(|| {
            let mut handles = Vec::with_capacity(1000);
            for i in 0..1000i32 {
                handles.push(bench_spawn(async move { black_box(i) }));
            }
            
            bench_block_on(async {
                for h in handles {
                    h.await;
                }
            });
        });
    });
}

fn smol_parallel_1k(c: &mut Criterion) {
    c.bench_function("smol_parallel_1k", |b| {
        b.iter(|| {
            smol::block_on(async {
                let handles: Vec<_> = (0..1000i32)
                    .map(|i| smol::spawn(async move { black_box(i) }))
                    .collect();
                
                for h in handles {
                    h.await;
                }
            });
        });
    });
}

// =============================================================================
// WAKER MICROBENCHMARKS
// =============================================================================

fn waker_create_1m(c: &mut Criterion) {
    fn noop_clone(d: *const ()) -> RawWaker { RawWaker::new(d, &VTABLE) }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);
    
    c.bench_function("waker_create_1m", |b| {
        b.iter(|| {
            for i in 0..1_000_000u64 {
                let data = black_box(i) as *const ();
                let waker = unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) };
                black_box(&waker);
            }
        });
    });
}

fn waker_clone_1m(c: &mut Criterion) {
    fn noop_clone(d: *const ()) -> RawWaker { RawWaker::new(d, &VTABLE) }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);
    
    let base_waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    
    c.bench_function("waker_clone_1m", |b| {
        b.iter(|| {
            for _ in 0..1_000_000u32 {
                let cloned = base_waker.clone();
                black_box(&cloned);
            }
        });
    });
}

fn tau_make_waker_1m(c: &mut Criterion) {
    executor::init();
    let executor_idx = executor::current_executor();
    
    c.bench_function("tau_make_waker_1m", |b| {
        b.iter(|| {
            for i in 0..1_000_000u32 {
                let waker = executor::make_waker(executor_idx, black_box(i));
                black_box(&waker);
            }
        });
    });
}

criterion_group!(
    benches,
    smol_spawn_1k,
    tau_spawn_1k,
    smol_pingpong_1k,
    tau_spawn_cancel_50pct,
    tau_parallel_1k,
    smol_parallel_1k,
    waker_create_1m,
    waker_clone_1m,
    tau_make_waker_1m,
);
criterion_main!(benches);
