//! Loom tests for tau executor concurrency primitives.
//!
//! These tests verify the correctness of our synchronization patterns
//! under all possible thread interleavings using the loom model checker.
//!
//! Run with:
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test -p tau-loom-tests --release
//! ```
//!
//! For faster iteration with bounded preemptions:
//! ```sh
//! LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" cargo test -p tau-loom-tests --release
//! ```

#[cfg(loom)]
mod tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use loom::sync::Mutex;
    use loom::thread;
    use std::collections::VecDeque;

    // =========================================================================
    // RemoteQueue - mirrors tau-host's sync::RemoteQueue under loom
    // =========================================================================

    /// Lock-based queue for cross-thread task wakes.
    struct RemoteQueue<T>(Mutex<VecDeque<T>>);

    impl<T> RemoteQueue<T> {
        fn new() -> Self {
            Self(Mutex::new(VecDeque::new()))
        }
        
        fn push(&self, value: T) {
            self.0.lock().unwrap().push_back(value);
        }
        
        fn pop(&self) -> Option<T> {
            self.0.lock().unwrap().pop_front()
        }
        
        #[allow(dead_code)]
        fn is_empty(&self) -> bool {
            self.0.lock().unwrap().is_empty()
        }
    }

    // =========================================================================
    // Basic Queue Tests
    // =========================================================================

    #[test]
    fn remote_queue_sequential() {
        loom::model(|| {
            let queue = RemoteQueue::new();
            queue.push(1u32);
            queue.push(2);
            assert_eq!(queue.pop(), Some(1));
            assert_eq!(queue.pop(), Some(2));
            assert_eq!(queue.pop(), None);
        });
    }

    #[test]
    fn remote_queue_push_then_pop() {
        loom::model(|| {
            let queue = Arc::new(RemoteQueue::new());
            let queue2 = queue.clone();
            
            let h = thread::spawn(move || {
                queue2.push(42u32);
                queue2.push(43u32);
            });
            
            h.join().unwrap();
            
            let mut items = vec![];
            while let Some(v) = queue.pop() {
                items.push(v);
            }
            items.sort();
            assert_eq!(items, vec![42, 43]);
        });
    }

    #[test]
    fn remote_queue_concurrent_pushes() {
        loom::model(|| {
            let queue = Arc::new(RemoteQueue::new());
            
            let q1 = queue.clone();
            let q2 = queue.clone();
            
            let h1 = thread::spawn(move || {
                q1.push(1u32);
            });
            
            let h2 = thread::spawn(move || {
                q2.push(2u32);
            });
            
            h1.join().unwrap();
            h2.join().unwrap();
            
            let mut items = vec![];
            while let Some(v) = queue.pop() {
                items.push(v);
            }
            items.sort();
            assert_eq!(items, vec![1, 2]);
        });
    }

    #[test]
    fn remote_queue_push_pop_interleaved() {
        loom::model(|| {
            let queue = Arc::new(RemoteQueue::new());
            let queue2 = queue.clone();
            
            let h = thread::spawn(move || {
                queue2.push(1u32);
            });
            
            // Main thread pops (may or may not see the item)
            let first_pop = queue.pop();
            
            h.join().unwrap();
            
            // Drain remaining
            let mut remaining = vec![];
            while let Some(v) = queue.pop() {
                remaining.push(v);
            }
            
            // Either we got it early or it's in remaining
            match first_pop {
                Some(1) => assert!(remaining.is_empty()),
                None => assert_eq!(remaining, vec![1]),
                _ => panic!("unexpected value"),
            }
        });
    }

    // =========================================================================
    // Atomic Counter Tests (NEXT_PLUGIN_ID pattern)
    // =========================================================================

    #[test]
    fn atomic_counter_unique_ids() {
        loom::model(|| {
            let counter = Arc::new(AtomicU64::new(1));
            
            let c1 = counter.clone();
            let c2 = counter.clone();
            
            let h1 = thread::spawn(move || {
                c1.fetch_add(1, Ordering::Relaxed)
            });
            
            let h2 = thread::spawn(move || {
                c2.fetch_add(1, Ordering::Relaxed)
            });
            
            let id1 = h1.join().unwrap();
            let id2 = h2.join().unwrap();
            
            // IDs must be unique
            assert_ne!(id1, id2);
            // IDs are 1 and 2 (in some order)
            let mut ids = vec![id1, id2];
            ids.sort();
            assert_eq!(ids, vec![1, 2]);
            
            // Counter should be at 3
            assert_eq!(counter.load(Ordering::Relaxed), 3);
        });
    }

    // =========================================================================
    // Store/Load Tests (CURRENT_PLUGIN pattern)
    // =========================================================================

    #[test]
    fn atomic_store_load() {
        loom::model(|| {
            let current = Arc::new(AtomicU64::new(0));
            let current2 = current.clone();
            
            let h = thread::spawn(move || {
                current2.store(42, Ordering::Relaxed);
            });
            
            let read = current.load(Ordering::Relaxed);
            
            h.join().unwrap();
            
            // Read is either 0 (before store) or 42 (after store)
            assert!(read == 0 || read == 42);
        });
    }

    #[test]
    fn atomic_store_restore_pattern() {
        // This tests the pattern: save current, set new, do work, restore
        loom::model(|| {
            let current = Arc::new(AtomicU64::new(0));
            let current2 = current.clone();
            
            let h = thread::spawn(move || {
                let prev = current2.load(Ordering::Relaxed);
                current2.store(1, Ordering::Relaxed);
                // "do work"
                loom::thread::yield_now();
                current2.store(prev, Ordering::Relaxed);
            });
            
            // Main thread also does store/restore
            let prev = current.load(Ordering::Relaxed);
            current.store(2, Ordering::Relaxed);
            loom::thread::yield_now();
            current.store(prev, Ordering::Relaxed);
            
            h.join().unwrap();
            
            // Final value depends on interleaving - could be 0, 1, or 2
            let final_val = current.load(Ordering::Relaxed);
            assert!(final_val == 0 || final_val == 1 || final_val == 2);
        });
    }

    // =========================================================================
    // Wake Pattern Tests
    // =========================================================================

    #[test]
    fn wake_flag_pattern() {
        loom::model(|| {
            let ready = Arc::new(AtomicBool::new(false));
            let ready2 = ready.clone();
            
            let h = thread::spawn(move || {
                ready2.store(true, Ordering::Release);
            });
            
            // Spin until ready (with yield for loom)
            while !ready.load(Ordering::Acquire) {
                loom::thread::yield_now();
            }
            
            h.join().unwrap();
            assert!(ready.load(Ordering::Relaxed));
        });
    }

    #[test]
    fn wake_with_queue() {
        // Simulates: wake sets flag AND pushes to queue
        loom::model(|| {
            let ready = Arc::new(AtomicBool::new(false));
            let queue = Arc::new(RemoteQueue::new());
            
            let ready2 = ready.clone();
            let queue2 = queue.clone();
            
            let h = thread::spawn(move || {
                queue2.push(42u32);
                ready2.store(true, Ordering::Release);
            });
            
            // Wait for ready
            while !ready.load(Ordering::Acquire) {
                loom::thread::yield_now();
            }
            
            h.join().unwrap();
            
            // Queue must have the item
            assert_eq!(queue.pop(), Some(42));
        });
    }

    // =========================================================================
    // MPSC Pattern (Multiple Producers, Single Consumer)
    // =========================================================================

    #[test]
    fn mpsc_queue() {
        loom::model(|| {
            let queue = Arc::new(RemoteQueue::new());
            let done = Arc::new(AtomicUsize::new(0));
            
            let q1 = queue.clone();
            let d1 = done.clone();
            let h1 = thread::spawn(move || {
                q1.push(1u32);
                d1.fetch_add(1, Ordering::Release);
            });
            
            let q2 = queue.clone();
            let d2 = done.clone();
            let h2 = thread::spawn(move || {
                q2.push(2u32);
                d2.fetch_add(1, Ordering::Release);
            });
            
            // Wait for both producers
            while done.load(Ordering::Acquire) < 2 {
                loom::thread::yield_now();
            }
            
            h1.join().unwrap();
            h2.join().unwrap();
            
            // Consume all
            let mut items = vec![];
            while let Some(v) = queue.pop() {
                items.push(v);
            }
            items.sort();
            assert_eq!(items, vec![1, 2]);
        });
    }

    // =========================================================================
    // State Machine Pattern (task state transitions)
    // =========================================================================

    const STATE_PENDING: u8 = 1;
    const STATE_READY: u8 = 2;
    const STATE_COMPLETE: u8 = 3;

    #[test]
    fn task_state_transitions() {
        use loom::sync::atomic::AtomicU8;
        
        loom::model(|| {
            let state = Arc::new(AtomicU8::new(STATE_PENDING));
            let state2 = state.clone();
            
            // Thread 1: wake (PENDING -> READY)
            let h = thread::spawn(move || {
                let _ = state2.compare_exchange(
                    STATE_PENDING,
                    STATE_READY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            });
            
            // Main thread: complete (READY -> COMPLETE or PENDING -> COMPLETE)
            loop {
                let current = state.load(Ordering::Acquire);
                if current == STATE_COMPLETE {
                    break;
                }
                if current == STATE_READY || current == STATE_PENDING {
                    if state.compare_exchange(
                        current,
                        STATE_COMPLETE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ).is_ok() {
                        break;
                    }
                }
                loom::thread::yield_now();
            }
            
            h.join().unwrap();
            
            assert_eq!(state.load(Ordering::Relaxed), STATE_COMPLETE);
        });
    }
}

// Placeholder for non-loom builds
#[cfg(not(loom))]
pub fn placeholder() {}
