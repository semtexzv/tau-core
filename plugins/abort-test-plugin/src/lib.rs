//! Abort test plugin — exercises task abort end-to-end across FFI.
//!
//! Commands:
//!   "spawn"  — spawn a long-sleeping task, store its task_id in a resource
//!   "abort"  — abort the spawned task
//!   "check"  — print whether the task is finished
//!   "spawn-and-complete" — spawn a task that completes immediately, store task_id
//!   "abort-completed" — abort the already-completed task (should be a no-op)

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    id: u64,
    payload: String,
}

/// Tracks whether the spawned future's Drop ran.
static DROP_RAN: AtomicBool = AtomicBool::new(false);

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
                // Store task_id as a resource so we can retrieve it in later requests
                tau::resource::put("abort-test-task-id", tid);
                println!("[abort-test] Spawned task_id={}", tid);
                tid
            }

            "abort" => {
                if let Some(tid) = tau::resource::get::<u64>("abort-test-task-id") {
                    let result = unsafe { tau_task_abort(tid) };
                    println!("[abort-test] Aborted task_id={}, found={}", tid, result);
                } else {
                    println!("[abort-test] No task to abort");
                }
                0
            }

            "check" => {
                if let Some(tid) = tau::resource::get::<u64>("abort-test-task-id") {
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
                tau::resource::put("abort-test-task-id", tid);
                println!("[abort-test] Spawned quick task_id={}", tid);
                // Drop the handle — task is detached but still runs
                tid
            }

            "abort-completed" => {
                if let Some(tid) = tau::resource::get::<u64>("abort-test-task-id") {
                    let result = unsafe { tau_task_abort(tid) };
                    println!("[abort-test] Abort completed task_id={}, found={}", tid, result);
                } else {
                    println!("[abort-test] No task to abort");
                }
                0
            }

            _ => {
                println!("[abort-test] Unknown command: {}", cmd);
                0
            }
        }
    }
}
