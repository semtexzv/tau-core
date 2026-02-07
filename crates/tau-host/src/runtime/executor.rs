//! Slab-based single-threaded async executor.
//!
//! # Why This Lives in tau-host (NOT tau-rt)
//!
//! **This is intentional and load-bearing.** The executor implementation lives
//! in the host binary rather than the tau-rt dylib for one critical reason:
//!
//! **Dependency Resolution During Plugin Compilation**
//!
//! When plugins compile against tau-rt (provided as a prebuilt dylib via
//! `--extern=tau_rt=/path/to/libtau_rt.dylib`), Rust needs to resolve the
//! rmeta (metadata) for ALL of tau-rt's dependencies. If tau-rt depends on
//! `crossbeam-queue`, plugins would fail to compile with:
//!
//! ```text
//! error[E0463]: can't find crate for `crossbeam_queue` which `tau_rt` depends on
//! ```
//!
//! By keeping the executor (and its crossbeam-queue dependency) in the host:
//! - tau-rt becomes dependency-free (only std)
//! - Plugins compile against tau-rt's thin extern declarations
//! - The host provides the actual implementation at runtime
//! - crossbeam-queue is statically linked into the host, invisible to plugins
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      tau-host (binary)                       │
//! │  ┌─────────────────────────────────────────────────────────┐│
//! │  │  executor.rs (THIS FILE)                                 ││
//! │  │  - Full implementation with crossbeam-queue              ││
//! │  │  - Exports #[no_mangle] functions                        ││
//! │  └─────────────────────────────────────────────────────────┘│
//! └─────────────────────────────────────────────────────────────┘
//!                              ▲
//!                              │ extern "Rust" calls
//!                              │
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      tau-rt (dylib)                          │
//! │  ┌─────────────────────────────────────────────────────────┐│
//! │  │  executor.rs (thin wrapper)                              ││
//! │  │  - extern "Rust" { fn tau_exec_*(...); }                 ││
//! │  │  - Thin wrappers that call extern functions              ││
//! │  │  - NO external dependencies (only std)                   ││
//! │  └─────────────────────────────────────────────────────────┘│
//! │  ┌─────────────────────────────────────────────────────────┐│
//! │  │  runtime.rs                                              ││
//! │  │  - Generic spawn<F>, JoinHandle<T>, block_on<F>          ││
//! │  │  - Monomorphizes into plugins via dylib rmeta            ││
//! │  └─────────────────────────────────────────────────────────┘│
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design
//!
//! Tasks are stored in fixed-size segments (`[TaskSlot; 256]`). Small futures
//! (≤ INLINE_SIZE bytes) are stored inline in the slot; larger ones are boxed.
//! Each slot carries type-erased function pointers for poll/drop/read.
//!
//! Wakers pack `(executor_idx, task_id)` into the pointer field — zero heap
//! allocation. Cross-thread wakes go through a lock-free `SegQueue`.
//!
//! # Safety invariants
//!
//! - `with_executor` borrows are NEVER nested. All waker wakes, drop callbacks,
//!   and poll calls happen OUTSIDE `with_executor` closures.
//! - The C TLS (`tls_get_executor`) is set once per thread via `init()`.
//! - `waker_wake` on the fast path (same-thread) directly accesses EXECUTORS
//!   via UnsafeCell. This is safe because it's only called outside with_executor.

use std::collections::{HashMap, VecDeque};
use std::mem::MaybeUninit;
use std::task::{Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

use super::sync::{AtomicU16, AtomicU64, Ordering, UnsafeCell, RemoteQueue, thread};

// =============================================================================
// Constants
// =============================================================================

pub const SEGMENT_SIZE: usize = 256;
pub const MAX_SEGMENTS: usize = 64;
pub const INLINE_SIZE: usize = 80;
pub const MAX_EXECUTORS: usize = 256;

pub const STATE_EMPTY: u8 = 0;
pub const STATE_PENDING: u8 = 1;
pub const STATE_READY: u8 = 2;
pub const STATE_COMPLETE: u8 = 3;
pub const STATE_CANCELLED: u8 = 4;

// =============================================================================
// Function pointer types (type-erased task operations)
// =============================================================================

/// Poll the future. Returns Poll::Ready(()) on completion.
pub type PollFn = unsafe fn(*mut u8, &Waker) -> Poll<()>;

/// Drop the result (after completion, when nobody takes it).
/// Also frees storage for boxed futures.
pub type DropResultFn = unsafe fn(*mut u8);

/// Drop the future (for cancellation before completion).
/// Also frees storage for boxed futures.
pub type DropFutureFn = unsafe fn(*mut u8);

/// Get a pointer to the result T within the storage.
pub type ReadFn = unsafe fn(*mut u8) -> *mut u8;

/// Free the storage allocation without dropping T (for boxed futures,
/// after the result has been moved out via ptr::read).
pub type DeallocFn = unsafe fn(*mut u8);

// =============================================================================
// Task slot
// =============================================================================

#[repr(C, align(128))]
struct AlignedStorage([MaybeUninit<u8>; INLINE_SIZE]);

pub struct TaskSlot {
    storage: AlignedStorage,
    pub poll_fn: Option<PollFn>,
    pub drop_result_fn: Option<DropResultFn>,
    pub drop_future_fn: Option<DropFutureFn>,
    pub read_fn: Option<ReadFn>,
    pub dealloc_fn: Option<DeallocFn>,
    pub generation: u32,
    pub state: u8,
    pub detached: bool,
    pub plugin_id: u64,
    pub join_waker: Option<Waker>,
}

impl TaskSlot {
    const fn new() -> Self {
        Self {
            storage: AlignedStorage([MaybeUninit::uninit(); INLINE_SIZE]),
            poll_fn: None,
            drop_result_fn: None,
            drop_future_fn: None,
            read_fn: None,
            dealloc_fn: None,
            generation: 0,
            state: STATE_EMPTY,
            detached: false,
            plugin_id: 0,
            join_waker: None,
        }
    }

    pub fn storage_ptr(&mut self) -> *mut u8 {
        self.storage.0.as_mut_ptr() as *mut u8
    }
}

// =============================================================================
// Segment = fixed array of slots
// =============================================================================

type Segment = Box<[TaskSlot; SEGMENT_SIZE]>;

// =============================================================================
// Timer
// =============================================================================

pub struct TimerEntry {
    deadline: Instant,
    waker: Waker,
}

// =============================================================================
// Executor (one per thread)
// =============================================================================

pub struct Executor {
    pub segments: Vec<Segment>,
    pub ready: VecDeque<u32>,
    pub remote: RemoteQueue<u32>,
    pub free: Vec<u32>,
    pub hot_slot: Option<u32>,
    pub owner: Option<thread::Thread>,
    pub timers: Option<HashMap<u64, TimerEntry>>,
    pub next_timer_handle: u64,
}

impl Executor {
    const fn new() -> Self {
        Self {
            segments: Vec::new(),
            ready: VecDeque::new(),
            free: Vec::new(),
            remote: RemoteQueue::new(),
            hot_slot: None,
            owner: None,
            timers: None,
            next_timer_handle: 1,
        }
    }

    fn init(&mut self) {
        self.ready = VecDeque::with_capacity(SEGMENT_SIZE);
        self.free = Vec::with_capacity(SEGMENT_SIZE);
        self.owner = Some(thread::current());
        self.timers = Some(HashMap::new());
        if self.segments.is_empty() {
            self.segments.push(Box::new([const { TaskSlot::new() }; SEGMENT_SIZE]));
            for i in (0..SEGMENT_SIZE as u32).rev() {
                self.free.push(i);
            }
        }
    }

    #[inline(always)]
    pub fn slot(&self, id: u32) -> &TaskSlot {
        let seg = (id as usize) >> 8;
        let idx = (id as usize) & 0xFF;
        unsafe { self.segments.get_unchecked(seg).get_unchecked(idx) }
    }

    #[inline(always)]
    pub fn slot_mut(&mut self, id: u32) -> &mut TaskSlot {
        let seg = (id as usize) >> 8;
        let idx = (id as usize) & 0xFF;
        unsafe { self.segments.get_unchecked_mut(seg).get_unchecked_mut(idx) }
    }

    #[inline]
    pub fn timers(&self) -> &HashMap<u64, TimerEntry> {
        self.timers.as_ref().unwrap()
    }

    #[inline]
    pub fn timers_mut(&mut self) -> &mut HashMap<u64, TimerEntry> {
        self.timers.as_mut().unwrap()
    }

    pub fn alloc_slot(&mut self) -> u32 {
        if let Some(id) = self.free.pop() {
            return id;
        }
        if self.segments.len() >= MAX_SEGMENTS {
            panic!("executor: out of task slots ({} segments × {} slots)",
                   MAX_SEGMENTS, SEGMENT_SIZE);
        }
        let seg_idx = self.segments.len();
        self.segments.push(Box::new([const { TaskSlot::new() }; SEGMENT_SIZE]));
        let base = (seg_idx * SEGMENT_SIZE) as u32;
        for i in (1..SEGMENT_SIZE as u32).rev() {
            self.free.push(base + i);
        }
        base
    }
}

// =============================================================================
// Global executor array
// =============================================================================

struct ExecutorCell(UnsafeCell<Executor>);
unsafe impl Sync for ExecutorCell {}

static EXECUTORS: [ExecutorCell; MAX_EXECUTORS] = {
    const INIT: ExecutorCell = ExecutorCell(UnsafeCell::new(Executor::new()));
    [INIT; MAX_EXECUTORS]
};
static NEXT_EXECUTOR: AtomicU16 = AtomicU16::new(0);

// =============================================================================
// C TLS for fast thread-local executor access
// =============================================================================

extern "C" {
    fn tls_get_executor() -> u16;
    fn tls_set_executor(idx: u16);
    fn tls_is_current_executor(executor_idx: u16) -> bool;
}

#[inline(always)]
pub fn current_executor() -> u16 {
    unsafe { tls_get_executor() }
}

#[inline(always)]
pub fn is_current_executor(executor_idx: u16) -> bool {
    unsafe { tls_is_current_executor(executor_idx) }
}

fn set_current_executor(idx: u16) {
    unsafe { tls_set_executor(idx) }
}

// =============================================================================
// Executor access (borrow discipline: NEVER nest these)
// =============================================================================

/// Access the current thread's executor. MUST NOT be nested.
#[inline(always)]
pub fn with_executor<R>(f: impl FnOnce(&mut Executor) -> R) -> R {
    let idx = current_executor() as usize;
    unsafe {
        let ex = &mut *EXECUTORS[idx].0.get();
        f(ex)
    }
}

/// Access a specific executor by index. MUST NOT be nested.
#[inline(always)]
pub fn with_executor_idx<R>(idx: u16, f: impl FnOnce(&mut Executor) -> R) -> R {
    unsafe {
        let ex = &mut *EXECUTORS[idx as usize].0.get();
        f(ex)
    }
}

// =============================================================================
// Waker implementation — packs (executor_idx, task_id) into pointer
// =============================================================================

#[inline(always)]
fn pack_waker_data(executor_idx: u16, task_id: u32) -> usize {
    ((executor_idx as usize) << 48) | (task_id as usize)
}

#[inline(always)]
fn unpack_waker_data(data: usize) -> (u16, u32) {
    let executor_idx = (data >> 48) as u16;
    let task_id = (data & 0xFFFF_FFFF_FFFF) as u32;
    (executor_idx, task_id)
}

fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &WAKER_VTABLE)
}

fn waker_wake(data: *const ()) {
    let (executor_idx, task_id) = unpack_waker_data(data as usize);

    if is_current_executor(executor_idx) {
        // Fast path: same executor thread. Safe because this is called
        // OUTSIDE of with_executor closures (during poll, waker fire, etc.)
        let ex = unsafe { &mut *EXECUTORS[executor_idx as usize].0.get() };
        if ex.slot(task_id).state == STATE_PENDING {
            ex.slot_mut(task_id).state = STATE_READY;
            // LIFO: put in hot slot for cache locality
            if let Some(old_hot) = ex.hot_slot.replace(task_id) {
                ex.ready.push_back(old_hot);
            }
        }
    } else {
        // Slow path: cross-thread wake via lock-free queue
        let ex = unsafe { &*EXECUTORS[executor_idx as usize].0.get() };
        ex.remote.push(task_id);
        if let Some(ref owner) = ex.owner {
            owner.unpark();
        }
    }
}

fn waker_drop(_: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake, waker_drop);

pub fn make_waker(executor_idx: u16, task_id: u32) -> Waker {
    let data = pack_waker_data(executor_idx, task_id);
    unsafe { Waker::from_raw(RawWaker::new(data as *const (), &WAKER_VTABLE)) }
}

// =============================================================================
// Plugin tracking
// =============================================================================

pub static CURRENT_PLUGIN: AtomicU64 = AtomicU64::new(0);
static NEXT_PLUGIN_ID: AtomicU64 = AtomicU64::new(1);

pub fn allocate_plugin_id() -> u64 {
    NEXT_PLUGIN_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn set_current_plugin(id: u64) {
    CURRENT_PLUGIN.store(id, Ordering::Relaxed);
}

pub fn clear_current_plugin() {
    CURRENT_PLUGIN.store(0, Ordering::Relaxed);
}

pub fn current_plugin_id() -> u64 {
    CURRENT_PLUGIN.load(Ordering::Relaxed)
}

// =============================================================================
// Debug logging
// =============================================================================

pub fn debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TAU_DEBUG").map_or(false, |v| v == "1"))
}

macro_rules! rt_debug {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[rt] {}", format!($($arg)*));
        }
    };
}

// =============================================================================
// Packed task ID for FFI (gen << 32 | slot_id)
// =============================================================================

pub fn pack_task_id(gen: u32, slot_id: u32) -> u64 {
    ((gen as u64) << 32) | (slot_id as u64)
}

pub fn unpack_task_id(packed: u64) -> (u32, u32) {
    let slot_id = (packed & 0xFFFF_FFFF) as u32;
    let gen = (packed >> 32) as u32;
    (gen, slot_id)
}

// =============================================================================
// Initialization
// =============================================================================

/// Initialize a new executor on the current thread. Returns the executor index.
pub fn init() -> u16 {
    let idx = NEXT_EXECUTOR.fetch_add(1, Ordering::Relaxed);
    assert!((idx as usize) < MAX_EXECUTORS, "too many executors");
    unsafe {
        let ex = &mut *EXECUTORS[idx as usize].0.get();
        ex.init();
    }
    set_current_executor(idx);
    idx
}

// =============================================================================
// Spawn (non-generic: sets up slot metadata, marks ready)
// =============================================================================

/// Allocate a slot and return (slot_id, generation, storage_ptr, executor_idx).
/// The caller writes the future data to storage_ptr, then calls commit_spawn.
pub fn alloc_raw() -> (u32, u32, *mut u8, u16) {
    let executor_idx = current_executor();
    with_executor(|ex| {
        let id = ex.alloc_slot();
        let slot = ex.slot_mut(id);
        slot.generation = slot.generation.wrapping_add(1);
        let gen = slot.generation;
        let storage_ptr = slot.storage_ptr();
        (id, gen, storage_ptr, executor_idx)
    })
}

/// Finalize a spawned task: set function pointers and mark ready.
/// Call after writing future data to the storage pointer from alloc_raw.
pub fn commit_spawn(
    id: u32,
    poll_fn: PollFn,
    drop_result_fn: Option<DropResultFn>,
    drop_future_fn: Option<DropFutureFn>,
    read_fn: Option<ReadFn>,
    dealloc_fn: Option<DeallocFn>,
    plugin_id: u64,
) {
    with_executor(|ex| {
        let slot = ex.slot_mut(id);
        slot.poll_fn = Some(poll_fn);
        slot.drop_result_fn = drop_result_fn;
        slot.drop_future_fn = drop_future_fn;
        slot.read_fn = read_fn;
        slot.dealloc_fn = dealloc_fn;
        slot.plugin_id = plugin_id;
        slot.state = STATE_READY;
        slot.join_waker = None;
        slot.detached = false;
        let gen = slot.generation;
        ex.ready.push_back(id);
        rt_debug!("spawn slot={} gen={} plugin={}", id, gen, plugin_id);
    })
}

// =============================================================================
// Task snapshot (extracted for polling outside borrow)
// =============================================================================

struct TaskSnapshot {
    id: u32,
    plugin_id: u64,
    poll_fn: PollFn,
    storage_ptr: *mut u8,
}

// =============================================================================
// Drive cycle — the core executor loop
// =============================================================================

/// Execute one drive cycle: drain remote wakes, fire timers, poll ready tasks.
/// Returns true if any work was done.
pub fn drive_cycle() -> bool {
    let executor_idx = current_executor();
    let mut did_work = false;

    // Phase 1: drain remote wakes
    with_executor(|ex| {
        while let Some(id) = ex.remote.pop() {
            let total_slots = ex.segments.len() * SEGMENT_SIZE;
            if (id as usize) < total_slots && ex.slot(id).state == STATE_PENDING {
                ex.slot_mut(id).state = STATE_READY;
                ex.ready.push_back(id);
            }
            did_work = true;
        }
    });

    // Phase 2: fire expired timers (extract wakers, wake OUTSIDE borrow)
    let expired_wakers: Vec<Waker> = with_executor(|ex| {
        let now = Instant::now();
        let expired_handles: Vec<u64> = ex.timers()
            .iter()
            .filter(|(_, e)| e.deadline <= now)
            .map(|(h, _)| *h)
            .collect();
        expired_handles
            .iter()
            .filter_map(|h| ex.timers_mut().remove(h).map(|e| e.waker))
            .collect()
    });
    for w in expired_wakers {
        w.wake();
        did_work = true;
    }

    // Phase 3: poll tasks one at a time (borrow released between polls)
    loop {
        let task = with_executor(|ex| {
            // Hot slot first (LIFO for cache locality)
            if let Some(id) = ex.hot_slot.take() {
                let slot = ex.slot_mut(id);
                if slot.state == STATE_READY {
                    slot.state = STATE_PENDING;
                    return Some(TaskSnapshot {
                        id,
                        plugin_id: slot.plugin_id,
                        poll_fn: slot.poll_fn.unwrap(),
                        storage_ptr: slot.storage_ptr(),
                    });
                }
            }
            // Ready queue
            while let Some(id) = ex.ready.pop_front() {
                let slot = ex.slot_mut(id);
                if slot.state != STATE_READY { continue; }
                slot.state = STATE_PENDING;
                return Some(TaskSnapshot {
                    id,
                    plugin_id: slot.plugin_id,
                    poll_fn: slot.poll_fn.unwrap(),
                    storage_ptr: slot.storage_ptr(),
                });
            }
            None
        });

        let Some(snap) = task else { break; };

        // Set current plugin for any tau_rt calls during poll
        let prev_plugin = CURRENT_PLUGIN.load(Ordering::Relaxed);
        CURRENT_PLUGIN.store(snap.plugin_id, Ordering::Relaxed);

        // Poll (NO borrow held — future can call spawn, timer_register, etc.)
        let waker = make_waker(executor_idx, snap.id);
        let result = unsafe { (snap.poll_fn)(snap.storage_ptr, &waker) };

        CURRENT_PLUGIN.store(prev_plugin, Ordering::Relaxed);

        if result.is_ready() {
            // Extract completion info inside borrow, act outside
            let (join_waker, cleanup) = with_executor(|ex| {
                let slot = ex.slot_mut(snap.id);
                slot.state = STATE_COMPLETE;
                rt_debug!("complete slot={} detached={}", snap.id, slot.detached);
                if slot.detached {
                    // Auto-cleanup: mark cancelled (prevent reuse), extract drop info
                    let drop_fn = slot.drop_result_fn;
                    let storage = slot.storage_ptr();
                    slot.state = STATE_CANCELLED; // temporary — freed after drop
                    slot.join_waker = None;
                    (None, Some((drop_fn, storage, snap.id)))
                } else {
                    (slot.join_waker.take(), None)
                }
            });

            // Drop result and free slot OUTSIDE borrow
            if let Some((drop_fn, storage, id)) = cleanup {
                if let Some(f) = drop_fn {
                    // Set plugin so drop glue runs with correct plugin context
                    let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
                    CURRENT_PLUGIN.store(snap.plugin_id, Ordering::Relaxed);
                    unsafe { f(storage) };
                    CURRENT_PLUGIN.store(prev, Ordering::Relaxed);
                }
                with_executor(|ex| {
                    let slot = ex.slot_mut(id);
                    slot.state = STATE_EMPTY;
                    ex.free.push(id);
                });
            }
            // Wake join handle OUTSIDE borrow
            if let Some(w) = join_waker {
                w.wake();
            }
        }

        did_work = true;
    }

    did_work
}

// =============================================================================
// Timer API
// =============================================================================

/// Register or update a timer. Returns the timer handle.
/// If old_handle != 0, updates the existing timer's waker.
pub fn timer_register(deadline: Instant, waker: Waker, old_handle: u64) -> u64 {
    with_executor(|ex| {
        if old_handle != 0 {
            if let Some(entry) = ex.timers_mut().get_mut(&old_handle) {
                entry.waker = waker;
                return old_handle;
            }
        }
        let handle = ex.next_timer_handle;
        ex.next_timer_handle += 1;
        ex.timers_mut().insert(handle, TimerEntry { deadline, waker });
        handle
    })
}

/// Cancel a timer by handle.
pub fn timer_cancel(handle: u64) {
    with_executor(|ex| {
        ex.timers_mut().remove(&handle);
    })
}

/// Get the duration until the next timer fires.
pub fn next_timer_deadline() -> Option<Duration> {
    with_executor(|ex| {
        let now = Instant::now();
        ex.timers()
            .values()
            .map(|e| e.deadline)
            .filter(|d| *d > now)
            .min()
            .map(|d| d - now)
    })
}

// =============================================================================
// Task lifecycle helpers (called by JoinHandle / host)
// =============================================================================

/// Cancel a task. Drops the future (outside borrow). Returns true if cancelled.
pub fn cancel_task(id: u32, gen: u32, executor_idx: u16) {
    // Extract what we need inside borrow
    let cleanup = with_executor_idx(executor_idx, |ex| {
        let slot = ex.slot_mut(id);
        if slot.generation != gen { return None; }
        match slot.state {
            STATE_EMPTY | STATE_CANCELLED => None,
            STATE_COMPLETE => {
                // Already complete — drop result
                let drop_fn = slot.drop_result_fn;
                let storage = slot.storage_ptr();
                let plugin_id = slot.plugin_id;
                slot.state = STATE_EMPTY;
                slot.join_waker = None;
                ex.free.push(id);
                ex.ready.retain(|rid| *rid != id);
                Some((drop_fn, None::<DropFutureFn>, storage, plugin_id))
            }
            _ => {
                // Running — drop future
                let drop_fn = slot.drop_future_fn;
                let storage = slot.storage_ptr();
                let plugin_id = slot.plugin_id;
                slot.state = STATE_CANCELLED;
                slot.join_waker = None;
                ex.ready.retain(|rid| *rid != id);
                // Remove associated timers
                ex.timers_mut().retain(|_, _| true); // timers don't track task_id here
                Some((None::<DropResultFn>, drop_fn, storage, plugin_id))
            }
        }
    });

    // Drop OUTSIDE borrow
    if let Some((drop_result, drop_future, storage, plugin_id)) = cleanup {
        let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
        CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);
        if let Some(f) = drop_result {
            unsafe { f(storage) };
        }
        if let Some(f) = drop_future {
            unsafe { f(storage) };
        }
        CURRENT_PLUGIN.store(prev, Ordering::Relaxed);

        // Free slot if it was a running task (state was set to CANCELLED)
        with_executor_idx(executor_idx, |ex| {
            let slot = ex.slot_mut(id);
            if slot.state == STATE_CANCELLED && slot.generation == gen {
                slot.state = STATE_EMPTY;
                ex.free.push(id);
            }
        });
    }
}

/// Check if a task is finished (completed, cancelled, or slot reused).
pub fn is_task_finished(id: u32, gen: u32, executor_idx: u16) -> bool {
    with_executor_idx(executor_idx, |ex| {
        let total_slots = ex.segments.len() * SEGMENT_SIZE;
        if (id as usize) >= total_slots { return true; }
        let slot = ex.slot(id);
        if slot.generation != gen { return true; }
        matches!(slot.state, STATE_COMPLETE | STATE_CANCELLED | STATE_EMPTY)
    })
}

/// Called when JoinHandle is dropped without taking the result.
/// If the task is still running, mark it detached (auto-cleanup on completion).
/// If already complete, drop the result and free the slot.
pub fn detach_or_cleanup(id: u32, gen: u32, executor_idx: u16) {
    let cleanup = with_executor_idx(executor_idx, |ex| {
        let total_slots = ex.segments.len() * SEGMENT_SIZE;
        if (id as usize) >= total_slots { return None; }
        let slot = ex.slot_mut(id);
        if slot.generation != gen { return None; }
        match slot.state {
            STATE_COMPLETE => {
                // Task done, result not taken — drop result and free
                let drop_fn = slot.drop_result_fn;
                let storage = slot.storage_ptr();
                let plugin_id = slot.plugin_id;
                slot.state = STATE_EMPTY;
                slot.join_waker = None;
                ex.free.push(id);
                Some((drop_fn, storage, plugin_id))
            }
            STATE_PENDING | STATE_READY => {
                // Still running — mark detached for auto-cleanup
                slot.detached = true;
                slot.join_waker = None;
                None
            }
            _ => None,
        }
    });

    if let Some((drop_fn, storage, plugin_id)) = cleanup {
        if let Some(f) = drop_fn {
            let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
            CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);
            unsafe { f(storage) };
            CURRENT_PLUGIN.store(prev, Ordering::Relaxed);
        }
    }
}

// =============================================================================
// Plugin task cleanup (for unload)
// =============================================================================

/// Drop all tasks belonging to a plugin. Returns the count dropped.
/// SAFETY: Must be called before dlclose — function pointers in slots
/// point into the plugin's .text section.
pub fn drop_plugin_tasks(plugin_id: u64) -> usize {
    // Phase 1: collect tasks to drop
    let to_drop: Vec<(u32, Option<DropFutureFn>, Option<DropResultFn>, *mut u8, u8)> =
        with_executor(|ex| {
            let total_slots = ex.segments.len() * SEGMENT_SIZE;
            let mut result = Vec::new();
            for id in 0..total_slots as u32 {
                let slot = ex.slot(id);
                if slot.plugin_id != plugin_id { continue; }
                if slot.state == STATE_EMPTY || slot.state == STATE_CANCELLED { continue; }
                let slot = ex.slot_mut(id);
                let state = slot.state;
                let drop_future = slot.drop_future_fn;
                let drop_result = slot.drop_result_fn;
                let storage = slot.storage_ptr();
                slot.state = STATE_CANCELLED;
                slot.join_waker = None;
                ex.ready.retain(|rid| *rid != id);
                result.push((id, drop_future, drop_result, storage, state));
            }
            // Clear hot slot if it belongs to this plugin
            if let Some(hot) = ex.hot_slot {
                if ex.slot(hot).plugin_id == plugin_id {
                    ex.hot_slot = None;
                }
            }
            result
        });

    let count = to_drop.len();

    // Phase 2: drop futures/results OUTSIDE borrow
    let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
    CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);
    for (id, drop_future, drop_result, storage, state) in &to_drop {
        match *state {
            STATE_COMPLETE => {
                if let Some(f) = drop_result {
                    unsafe { f(*storage) };
                }
            }
            _ => {
                if let Some(f) = drop_future {
                    unsafe { f(*storage) };
                }
            }
        }
        rt_debug!("drop_plugin_tasks: dropped slot={} state={}", id, state);
    }
    CURRENT_PLUGIN.store(prev, Ordering::Relaxed);

    // Phase 3: free slots
    with_executor(|ex| {
        for (id, _, _, _, _) in &to_drop {
            let slot = ex.slot_mut(*id);
            slot.state = STATE_EMPTY;
            ex.free.push(*id);
        }
        // Remove timers that have wakers pointing into the unloaded plugin
        // (We can't easily identify which timers belong to which plugin,
        // so we drain all timers — conservative but safe. Surviving tasks
        // will re-register their timers on next poll.)
        // TODO: track plugin_id on timers for precise cleanup
        if count > 0 {
            let old_timers = std::mem::take(ex.timers.as_mut().unwrap());
            drop(old_timers); // wakers dropped safely (plugin still loaded)
        }
    });

    count
}

// =============================================================================
// Wait for task (host's block_on equivalent)
// =============================================================================

extern "C" {
    fn tau_react(millis: u64);
}

/// Drive the executor until a specific task (packed_id) completes.
/// Also polls the IO reactor when idle.
pub fn wait_for_task(packed_id: u64) {
    if packed_id == 0 { return; }
    let (gen, slot_id) = unpack_task_id(packed_id);
    let _executor_idx = current_executor();

    loop {
        // Check if done
        let done = with_executor(|ex| {
            let total_slots = ex.segments.len() * SEGMENT_SIZE;
            if (slot_id as usize) >= total_slots { return true; }
            let slot = ex.slot(slot_id);
            if slot.generation != gen { return true; }
            matches!(slot.state, STATE_COMPLETE | STATE_CANCELLED | STATE_EMPTY)
        });
        if done {
            // Clean up slot if result is still there
            let cleanup = with_executor(|ex| {
                let total_slots = ex.segments.len() * SEGMENT_SIZE;
                if (slot_id as usize) >= total_slots { return None; }
                let slot = ex.slot_mut(slot_id);
                if slot.generation != gen { return None; }
                if slot.state == STATE_COMPLETE {
                    let drop_fn = slot.drop_result_fn;
                    let storage = slot.storage_ptr();
                    let plugin_id = slot.plugin_id;
                    slot.state = STATE_EMPTY;
                    slot.join_waker = None;
                    ex.free.push(slot_id);
                    Some((drop_fn, storage, plugin_id))
                } else {
                    None
                }
            });
            if let Some((drop_fn, storage, plugin_id)) = cleanup {
                if let Some(f) = drop_fn {
                    let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
                    CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);
                    unsafe { f(storage) };
                    CURRENT_PLUGIN.store(prev, Ordering::Relaxed);
                }
            }
            return;
        }

        // Drive executor
        let mut drove = false;
        while drive_cycle() {
            drove = true;
            // Re-check after each cycle
            let done = with_executor(|ex| {
                let slot = ex.slot(slot_id);
                if slot.generation != gen { return true; }
                matches!(slot.state, STATE_COMPLETE | STATE_CANCELLED | STATE_EMPTY)
            });
            if done { break; }
        }
        if drove { continue; }

        // No work — block on IO reactor
        let timeout = next_timer_deadline()
            .map(|d| d.min(Duration::from_millis(10)))
            .unwrap_or(Duration::from_millis(10));
        unsafe { tau_react(timeout.as_millis() as u64) };
    }
}

/// Reset the current executor to a clean state. Cancels all live tasks
/// (running their drop glue), drains all timers, and resets the slab.
///
/// # Safety
/// Must only be called when no JoinHandles or other references to this
/// executor's tasks are live. Intended for test isolation.
#[cfg(test)]
#[allow(dead_code)]
pub fn reset() {
    let executor_idx = current_executor();

    // Phase 1: collect live tasks to drop (inside borrow)
    let to_drop: Vec<(u32, Option<DropFutureFn>, Option<DropResultFn>, *mut u8, u8, u64)> =
        with_executor_idx(executor_idx, |ex| {
            let total_slots = ex.segments.len() * SEGMENT_SIZE;
            let mut result = Vec::new();
            for id in 0..total_slots as u32 {
                let slot = ex.slot(id);
                if slot.state == STATE_EMPTY || slot.state == STATE_CANCELLED {
                    continue;
                }
                let slot = ex.slot_mut(id);
                let state = slot.state;
                let drop_future = slot.drop_future_fn;
                let drop_result = slot.drop_result_fn;
                let storage = slot.storage_ptr();
                let plugin_id = slot.plugin_id;
                slot.state = STATE_CANCELLED;
                result.push((id, drop_future, drop_result, storage, state, plugin_id));
            }
            result
        });

    // Phase 2: drop futures/results OUTSIDE borrow
    for (_id, drop_future, drop_result, storage, state, plugin_id) in &to_drop {
        let prev = CURRENT_PLUGIN.load(Ordering::Relaxed);
        CURRENT_PLUGIN.store(*plugin_id, Ordering::Relaxed);
        match *state {
            STATE_COMPLETE => {
                if let Some(f) = drop_result {
                    unsafe { f(*storage) };
                }
            }
            _ => {
                if let Some(f) = drop_future {
                    unsafe { f(*storage) };
                }
            }
        }
        CURRENT_PLUGIN.store(prev, Ordering::Relaxed);
    }

    // Phase 3: reinitialize the executor
    with_executor_idx(executor_idx, |ex| {
        // Reset all slot states
        let total_slots = ex.segments.len() * SEGMENT_SIZE;
        for id in 0..total_slots as u32 {
            let slot = ex.slot_mut(id);
            slot.state = STATE_EMPTY;
            slot.poll_fn = None;
            slot.drop_result_fn = None;
            slot.drop_future_fn = None;
            slot.read_fn = None;
            slot.dealloc_fn = None;
            slot.detached = false;
            slot.plugin_id = 0;
            slot.join_waker = None;
            // Preserve generation so stale IDs are detected
        }
        // Rebuild free list
        ex.free.clear();
        for i in (0..total_slots as u32).rev() {
            ex.free.push(i);
        }
        // Clear queues
        ex.ready.clear();
        ex.hot_slot = None;
        // Drain remote queue
        while ex.remote.pop().is_some() {}
        // Clear timers
        *ex.timers_mut() = HashMap::new();
        ex.next_timer_handle = 1;
    });
    // Reset plugin state
    CURRENT_PLUGIN.store(0, Ordering::Relaxed);
}

/// Return (live_tasks, pending_timers, ready_queue_len) for debug.
pub fn debug_stats() -> (usize, usize, usize) {
    with_executor(|ex| {
        let total_slots = ex.segments.len() * SEGMENT_SIZE;
        let live = (0..total_slots as u32)
            .filter(|id| {
                let slot = ex.slot(*id);
                slot.state != STATE_EMPTY && slot.state != STATE_CANCELLED
            })
            .count();
        (live, ex.timers().len(), ex.ready.len())
    })
}

// =============================================================================
// FFI exports — backward-compatible symbols for plugins using extern "C"
// =============================================================================

/// Abort a task by packed ID. Returns 1 (accepted).
#[no_mangle]
pub extern "C" fn tau_task_abort(packed_id: u64) -> u8 {
    if packed_id == 0 { return 0; }
    let (gen, slot_id) = unpack_task_id(packed_id);
    let _executor_idx = current_executor();
    cancel_task(slot_id, gen, _executor_idx);
    1
}

/// Check if a task is finished by packed ID. Returns 1 if finished, 0 if running.
#[no_mangle]
pub extern "C" fn tau_task_is_finished(packed_id: u64) -> u8 {
    if packed_id == 0 { return 1; }
    let (gen, slot_id) = unpack_task_id(packed_id);
    let executor_idx = current_executor();
    if is_task_finished(slot_id, gen, executor_idx) { 1 } else { 0 }
}

/// Get the current plugin ID. Exported for plugins that call via extern "C".
#[no_mangle]
pub extern "C" fn tau_current_plugin_id() -> u64 {
    current_plugin_id()
}

// =============================================================================
// Executor exports for tau-rt
//
// These functions are called by tau-rt's thin executor wrapper via extern "Rust".
// Using extern "Rust" (not "C") preserves Rust calling conventions and is safe
// because of the same-compiler invariant.
//
// IMPORTANT: These must be kept alive by the EXECUTOR_EXPORTS static below,
// otherwise the linker may strip them as unused.
// =============================================================================

#[no_mangle]
pub extern "Rust" fn tau_exec_init() -> u16 {
    init()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_current_executor() -> u16 {
    current_executor()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_alloc_raw() -> (u32, u32, *mut u8, u16) {
    alloc_raw()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_commit_spawn(
    id: u32,
    poll_fn: PollFn,
    drop_result_fn: Option<DropResultFn>,
    drop_future_fn: Option<DropFutureFn>,
    read_fn: Option<ReadFn>,
    dealloc_fn: Option<DeallocFn>,
    plugin_id: u64,
) {
    commit_spawn(id, poll_fn, drop_result_fn, drop_future_fn, read_fn, dealloc_fn, plugin_id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_drive_cycle() -> bool {
    drive_cycle()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_cancel_task(id: u32, gen: u32, executor_idx: u16) {
    cancel_task(id, gen, executor_idx)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_is_task_finished(id: u32, gen: u32, executor_idx: u16) -> bool {
    is_task_finished(id, gen, executor_idx)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_detach_or_cleanup(id: u32, gen: u32, executor_idx: u16) {
    detach_or_cleanup(id, gen, executor_idx)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_timer_register(deadline_secs: u64, deadline_nanos: u32, waker: Waker, old_handle: u64) -> u64 {
    let deadline = Instant::now() + Duration::new(deadline_secs, deadline_nanos);
    timer_register(deadline, waker, old_handle)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_timer_cancel(handle: u64) {
    timer_cancel(handle)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_next_timer_deadline() -> Option<Duration> {
    next_timer_deadline()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_current_plugin_id() -> u64 {
    current_plugin_id()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_allocate_plugin_id() -> u64 {
    allocate_plugin_id()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_set_current_plugin(id: u64) {
    set_current_plugin(id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_clear_current_plugin() {
    clear_current_plugin()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_drop_plugin_tasks(plugin_id: u64) -> usize {
    drop_plugin_tasks(plugin_id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_wait_for_task(packed_id: u64) {
    wait_for_task(packed_id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_make_waker(executor_idx: u16, task_id: u32) -> Waker {
    make_waker(executor_idx, task_id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_pack_task_id(gen: u32, slot_id: u32) -> u64 {
    pack_task_id(gen, slot_id)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_unpack_task_id(packed: u64) -> (u32, u32) {
    unpack_task_id(packed)
}

#[no_mangle]
pub extern "Rust" fn tau_exec_debug_stats() -> (usize, usize, usize) {
    debug_stats()
}

#[no_mangle]
pub extern "Rust" fn tau_exec_debug_enabled() -> bool {
    debug_enabled()
}

/// Access executor slot state for JoinHandle polling.
/// Returns (state, generation) for the slot.
#[no_mangle]
pub extern "Rust" fn tau_exec_slot_state(id: u32, executor_idx: u16) -> (u8, u32) {
    with_executor_idx(executor_idx, |ex| {
        let total_slots = ex.segments.len() * SEGMENT_SIZE;
        if (id as usize) >= total_slots {
            return (STATE_EMPTY, 0);
        }
        let slot = ex.slot(id);
        (slot.state, slot.generation)
    })
}

/// Set join waker on a slot.
#[no_mangle]
pub extern "Rust" fn tau_exec_set_join_waker(id: u32, executor_idx: u16, waker: Waker) {
    with_executor_idx(executor_idx, |ex| {
        let slot = ex.slot_mut(id);
        slot.join_waker = Some(waker);
    })
}

/// Read result from a completed task and free the slot.
/// Returns the result pointer (caller must read via ptr::read).
#[no_mangle]
pub extern "Rust" fn tau_exec_take_result(id: u32, executor_idx: u16) -> (*mut u8, Option<DeallocFn>) {
    with_executor_idx(executor_idx, |ex| {
        let slot = ex.slot_mut(id);
        let result_ptr = unsafe { (slot.read_fn.unwrap())(slot.storage_ptr()) };
        let dealloc = slot.dealloc_fn;
        slot.state = STATE_EMPTY;
        slot.join_waker = None;
        ex.free.push(id);
        (result_ptr, dealloc)
    })
}

/// Free slot storage without dropping the result (for boxed futures after ptr::read).
#[no_mangle]
pub extern "Rust" fn tau_exec_dealloc_storage(id: u32, executor_idx: u16) {
    with_executor_idx(executor_idx, |ex| {
        let slot = ex.slot_mut(id);
        if let Some(dealloc) = slot.dealloc_fn {
            unsafe { dealloc(slot.storage_ptr()) };
        }
    })
}

// =============================================================================
// Force-keep executor exports
//
// The linker would strip the tau_exec_* functions as unused because they're
// only called from plugins at runtime (via extern "Rust" declarations).
// This static array references them, forcing the linker to keep them.
// =============================================================================

/// Call this function once from main() to ensure executor exports are linked.
/// The function itself does nothing, but referencing the symbols prevents
/// the linker from stripping them.
#[inline(never)]
#[allow(function_casts_as_integer)]
pub fn ensure_executor_exports_linked() {
    // Reference all tau_exec_* functions to prevent dead code elimination.
    // The black_box ensures the compiler can't optimize these away.
    std::hint::black_box((
        tau_exec_init as usize,
        tau_exec_current_executor as usize,
        tau_exec_alloc_raw as usize,
        tau_exec_commit_spawn as usize,
        tau_exec_drive_cycle as usize,
        tau_exec_cancel_task as usize,
        tau_exec_is_task_finished as usize,
        tau_exec_detach_or_cleanup as usize,
        tau_exec_timer_register as usize,
        tau_exec_timer_cancel as usize,
        tau_exec_next_timer_deadline as usize,
        tau_exec_current_plugin_id as usize,
        tau_exec_allocate_plugin_id as usize,
        tau_exec_set_current_plugin as usize,
        tau_exec_clear_current_plugin as usize,
        tau_exec_drop_plugin_tasks as usize,
        tau_exec_wait_for_task as usize,
        tau_exec_make_waker as usize,
        tau_exec_pack_task_id as usize,
        tau_exec_unpack_task_id as usize,
        tau_exec_debug_stats as usize,
        tau_exec_debug_enabled as usize,
        tau_exec_slot_state as usize,
        tau_exec_set_join_waker as usize,
        tau_exec_take_result as usize,
        tau_exec_dealloc_storage as usize,
    ));
}
