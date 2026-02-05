//! Event Bus — named broadcast channels with plugin-scoped cleanup.
//!
//! Subscribers register extern "C" callbacks + context pointer.
//! On emit, all subscribers for that channel are called synchronously.
//! On plugin unload, all subscriptions for that plugin are removed (drop_ctx called).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SUB_ID: AtomicU64 = AtomicU64::new(1);

struct Subscription {
    id: u64,
    plugin_id: u64,
    /// Callback function — lives in plugin code
    callback: unsafe extern "C" fn(ctx: *mut (), data_ptr: *const u8, data_len: usize),
    /// Opaque context pointer (typically a boxed closure)
    ctx: *mut (),
    /// Drop function for ctx — lives in plugin code
    drop_ctx: unsafe extern "C" fn(*mut ()),
}

// Safety: ctx is plugin-managed, accessed under mutex
unsafe impl Send for Subscription {}

static EVENT_BUS: std::sync::OnceLock<Mutex<HashMap<String, Vec<Subscription>>>> = std::sync::OnceLock::new();

fn bus() -> &'static Mutex<HashMap<String, Vec<Subscription>>> {
    EVENT_BUS.get_or_init(|| Mutex::new(HashMap::new()))
}

// =============================================================================
// FFI exports
// =============================================================================

/// Subscribe to a named event channel. Returns a subscription handle (for unsubscribe).
#[no_mangle]
pub extern "C" fn tau_event_subscribe(
    channel_ptr: *const u8,
    channel_len: usize,
    callback: unsafe extern "C" fn(*mut (), *const u8, usize),
    ctx: *mut (),
    drop_ctx: unsafe extern "C" fn(*mut ()),
    plugin_id: u64,
) -> u64 {
    let channel = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(channel_ptr, channel_len)) };
    let id = NEXT_SUB_ID.fetch_add(1, Ordering::Relaxed);

    let sub = Subscription {
        id,
        plugin_id,
        callback,
        ctx,
        drop_ctx,
    };

    let mut map = bus().lock().unwrap();
    map.entry(channel.to_string()).or_default().push(sub);

    id
}

/// Unsubscribe by handle. Returns 1 if found and removed, 0 if not found.
#[no_mangle]
pub extern "C" fn tau_event_unsubscribe(handle: u64) -> i32 {
    let mut map = bus().lock().unwrap();

    for subs in map.values_mut() {
        if let Some(pos) = subs.iter().position(|s| s.id == handle) {
            let sub = subs.remove(pos);
            // Drop outside the lock
            drop(map);
            unsafe { (sub.drop_ctx)(sub.ctx) };
            return 1;
        }
    }

    0
}

/// Emit an event to all subscribers on a channel.
/// Callbacks are called synchronously. Returns the number of subscribers notified.
#[no_mangle]
pub extern "C" fn tau_event_emit(
    channel_ptr: *const u8,
    channel_len: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> u32 {
    let channel = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(channel_ptr, channel_len)) };

    // Collect callbacks under lock, then call outside lock (re-entrancy safe)
    let callbacks: Vec<(unsafe extern "C" fn(*mut (), *const u8, usize), *mut (), u64)> = {
        let map = bus().lock().unwrap();
        match map.get(channel) {
            Some(subs) => subs.iter().map(|s| (s.callback, s.ctx, s.plugin_id)).collect(),
            None => return 0,
        }
    };

    let count = callbacks.len() as u32;

    // Set CURRENT_PLUGIN to the SUBSCRIBER's plugin for each callback,
    // so any tau::resource/event calls inside are tagged to the right plugin.
    use std::sync::atomic::Ordering;
    let prev_plugin = super::executor::CURRENT_PLUGIN.load(Ordering::Relaxed);
    for (callback, ctx, plugin_id) in callbacks {
        super::executor::CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);
        unsafe { callback(ctx, data_ptr, data_len) };
    }
    super::executor::CURRENT_PLUGIN.store(prev_plugin, Ordering::Relaxed);

    count
}

/// Drop all event subscriptions for a plugin. Called during plugin unload, BEFORE dlclose.
/// Returns the number of subscriptions dropped.
pub fn drop_plugin_events(plugin_id: u64) -> usize {
    let mut map = bus().lock().unwrap();

    let mut to_drop = Vec::new();

    for subs in map.values_mut() {
        let mut i = 0;
        while i < subs.len() {
            if subs[i].plugin_id == plugin_id {
                to_drop.push(subs.remove(i));
            } else {
                i += 1;
            }
        }
    }

    let count = to_drop.len();

    // Drop outside the lock
    drop(map);

    for sub in to_drop {
        #[cfg(debug_assertions)]
        if std::env::var("TAU_DEBUG").is_ok() {
            eprintln!("[rt] Dropping event subscription {} (plugin {})", sub.id, plugin_id);
        }
        unsafe { (sub.drop_ctx)(sub.ctx) };
    }

    count
}
