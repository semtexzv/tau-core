//! Abort test plugin — exercises task abort end-to-end across FFI.
//!
//! Commands:
//!   "spawn"  — spawn a long-sleeping task, store its task_id
//!   "abort"  — abort the spawned task
//!   "check"  — print whether the task is finished and whether Drop ran
//!   "spawn-and-complete" — spawn a task that completes immediately
//!   "abort-completed" — abort the already-completed task (should be no-op)
//!   "cross-thread-abort" — spawn a task, abort it from a background thread, verify cancelled

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    id: u64,
    payload: String,
}

/// Tracks whether the spawned future's Drop ran.
static DROP_RAN: AtomicBool = AtomicBool::new(false);

/// Stored task_id for the long-running task.
static TASK_ID: AtomicU64 = AtomicU64::new(0);

/// Stored task_id for the completed task.
static COMPLETED_TASK_ID: AtomicU64 = AtomicU64::new(0);

/// A guard that sets DROP_RAN when dropped — proves cancellation drops the future.
struct DropDetector;

impl Drop for DropDetector {
    fn drop(&mut self) {
        DROP_RAN.store(true, Ordering::SeqCst);
        println!("[abort-test] DropDetector::drop() called — future was cancelled and dropped");
    }
}

extern "C" {
    fn tau_task_abort(task_id: u64) -> u8;
    fn tau_task_is_finished(task_id: u64) -> u8;
}

tau::define_plugin! {
    fn init() {
        println!("[abort-test] Initialized");
    }

    fn destroy() {
        println!("[abort-test] Destroyed");
    }

    fn request(data: &[u8]) -> u64 {
        let msg: Message = serde_json::from_slice(data).expect("invalid JSON from host");
        let cmd = msg.payload.as_str();
        println!("[abort-test] Command: {}", cmd);

        match cmd {
            "spawn" => {
                DROP_RAN.store(false, Ordering::SeqCst);
                let handle = tau::spawn(async {
                    let _detector = DropDetector;
                    println!("[abort-test] Long task started, sleeping...");
                    // Sleep for a long time — will be aborted before completing
                    tau::sleep(std::time::Duration::from_secs(60)).await;
                    println!("[abort-test] Long task completed (should NOT see this if aborted)");
                });
                let tid = handle.task_id();
                TASK_ID.store(tid, Ordering::SeqCst);
                println!("[abort-test] Spawned task_id={}", tid);
                // Return 0: don't make block_on wait for the child task
                0
            }

            "abort" => {
                let tid = TASK_ID.load(Ordering::SeqCst);
                if tid != 0 {
                    let result = unsafe { tau_task_abort(tid) };
                    println!("[abort-test] Aborted task_id={}, found={}", tid, result);
                } else {
                    println!("[abort-test] No task to abort");
                }
                0
            }

            "check" => {
                let tid = TASK_ID.load(Ordering::SeqCst);
                if tid != 0 {
                    let finished = unsafe { tau_task_is_finished(tid) };
                    let drop_ran = DROP_RAN.load(Ordering::SeqCst);
                    println!("[abort-test] task_id={} is_finished={} drop_ran={}", tid, finished, drop_ran);
                } else {
                    println!("[abort-test] No task stored");
                }
                0
            }

            "spawn-and-complete" => {
                let handle = tau::spawn(async {
                    println!("[abort-test] Quick task completed immediately");
                    42u32
                });
                let tid = handle.task_id();
                COMPLETED_TASK_ID.store(tid, Ordering::SeqCst);
                println!("[abort-test] Spawned quick task_id={}", tid);
                // Return task_id so block_on drives it to completion
                tid
            }

            "abort-completed" => {
                let tid = COMPLETED_TASK_ID.load(Ordering::SeqCst);
                if tid != 0 {
                    let result = unsafe { tau_task_abort(tid) };
                    println!("[abort-test] Abort completed task_id={}, found={}", tid, result);
                } else {
                    println!("[abort-test] No task to abort");
                }
                0
            }

            "cross-thread-spawn" => {
                // Spawn a long-running task (same as "spawn" but for cross-thread test)
                DROP_RAN.store(false, Ordering::SeqCst);
                let handle = tau::spawn(async {
                    let _detector = DropDetector;
                    println!("[abort-test] Cross-thread target task started, sleeping...");
                    tau::sleep(std::time::Duration::from_secs(60)).await;
                    println!("[abort-test] Cross-thread target completed (should NOT see this)");
                });
                let tid = handle.task_id();
                TASK_ID.store(tid, Ordering::SeqCst);
                println!("[abort-test] Spawned cross-thread target task_id={}", tid);
                0
            }

            "cross-thread-abort" => {
                // Abort from a background thread using the stored task_id
                let tid = TASK_ID.load(Ordering::SeqCst);
                if tid == 0 {
                    println!("[abort-test] No task to cross-thread abort");
                    return 0;
                }
                std::thread::spawn(move || {
                    println!("[abort-test] Background thread calling tau_task_abort({})", tid);
                    let result = unsafe { tau_task_abort(tid) };
                    println!("[abort-test] Background thread abort result={}", result);
                });

                // Give the background thread a moment to queue the abort
                std::thread::sleep(std::time::Duration::from_millis(50));
                0
            }

            _ => {
                println!("[abort-test] Unknown command: {}", cmd);
                0
            }
        }
    }
}
