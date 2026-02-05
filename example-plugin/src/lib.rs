//! Example async plugin using µTokio
//!
//! This plugin:
//! 1. Registers hooks with the runtime during init
//! 2. Returns task IDs (futures) instead of blocking
//! 3. All task tracking lives in the runtime, not the plugin

use tau::serde::{Deserialize, Serialize};
use tau::{spawn, TaskId};

#[derive(Serialize, Deserialize)]
#[serde(crate = "tau::serde")]
pub struct Message {
    pub id: u64,
    pub payload: String,
}

/// Plugin hooks that the host can call
#[repr(C)]
pub struct PluginHooks {
    /// Process a message asynchronously, returns TaskId
    pub process: unsafe extern "C" fn(*const u8, usize) -> TaskId,
}

/// Initialize plugin and return hooks
#[no_mangle]
pub extern "C" fn plugin_init(hooks_out: *mut PluginHooks) -> i32 {
    if hooks_out.is_null() {
        return -1;
    }

    unsafe {
        (*hooks_out).process = plugin_process_async;
    }

    0
}

/// Submit a message for async processing, returns TaskId immediately
/// The task lives in the runtime - plugin just spawns it
unsafe extern "C" fn plugin_process_async(data: *const u8, len: usize) -> TaskId {
    if data.is_null() {
        return TaskId(0);
    }

    let input = std::slice::from_raw_parts(data, len).to_vec();

    // Spawn an async task via utokio interface
    // The task is tracked by the runtime, not the plugin
    let handle = spawn(async move {
        // Parse and echo (could do any async work here)
        match tau::json::from_slice::<Message>(&input) {
            Ok(msg) => tau::json::to_vec(&msg).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    });

    handle.task_id()
}

#[no_mangle]
pub extern "C" fn plugin_destroy() {
    // Nothing to clean up - tasks are in the runtime
}
