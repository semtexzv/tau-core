//! Typed event bus.
//!
//! Plugins subscribe to named channels and receive byte payloads.
//! Emit broadcasts to all subscribers synchronously.
//!
//! Subscriptions are automatically cleaned up when a plugin is unloaded
//! (the host calls `drop_ctx` while the plugin's code is still loaded).

// FFI — resolved at load time from host
extern "C" {
    fn tau_event_subscribe(
        channel_ptr: *const u8,
        channel_len: usize,
        callback: unsafe extern "C" fn(*mut (), *const u8, usize),
        ctx: *mut (),
        drop_ctx: unsafe extern "C" fn(*mut ()),
        plugin_id: u64,
    ) -> u64;

    fn tau_event_unsubscribe(handle: u64) -> i32;

    fn tau_event_emit(
        channel_ptr: *const u8,
        channel_len: usize,
        data_ptr: *const u8,
        data_len: usize,
    ) -> u32;

    fn tau_current_plugin_id() -> u64;
}

/// Handle to an event subscription. Unsubscribes on drop.
pub struct Subscription {
    handle: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { tau_event_unsubscribe(self.handle) };
        }
    }
}

/// Subscribe to a named event channel.
///
/// The callback receives raw bytes. Use `subscribe_json` for typed deserialization.
/// Returns a `Subscription` handle — dropping it unsubscribes.
pub fn subscribe_raw<F>(channel: &str, callback: F) -> Subscription
where
    F: Fn(&[u8]) + Send + 'static,
{
    let ctx = Box::into_raw(Box::new(callback)) as *mut ();
    let plugin_id = unsafe { tau_current_plugin_id() };

    unsafe extern "C" fn trampoline<F: Fn(&[u8])>(
        ctx: *mut (),
        data: *const u8,
        len: usize,
    ) {
        let f = &*(ctx as *const F);
        let slice = std::slice::from_raw_parts(data, len);
        f(slice);
    }

    unsafe extern "C" fn drop_ctx<F>(ctx: *mut ()) {
        drop(Box::from_raw(ctx as *mut F));
    }

    let handle = unsafe {
        tau_event_subscribe(
            channel.as_ptr(),
            channel.len(),
            trampoline::<F>,
            ctx,
            drop_ctx::<F>,
            plugin_id,
        )
    };

    Subscription { handle }
}

/// Emit raw bytes to all subscribers on a channel.
/// Returns the number of subscribers notified.
pub fn emit_raw(channel: &str, data: &[u8]) -> u32 {
    unsafe {
        tau_event_emit(
            channel.as_ptr(),
            channel.len(),
            data.as_ptr(),
            data.len(),
        )
    }
}

// =============================================================================
// Convenience: JSON-based typed events (requires serde)
// =============================================================================

/// Emit a serializable value as JSON bytes.
#[cfg(feature = "json")]
pub fn emit<T: serde::Serialize>(channel: &str, value: &T) -> u32 {
    let data = serde_json::to_vec(value).expect("Failed to serialize event");
    emit_raw(channel, &data)
}

/// Subscribe with automatic JSON deserialization.
#[cfg(feature = "json")]
pub fn subscribe<T, F>(channel: &str, callback: F) -> Subscription
where
    T: serde::de::DeserializeOwned + 'static,
    F: Fn(T) + Send + 'static,
{
    subscribe_raw(channel, move |data| {
        match serde_json::from_slice::<T>(data) {
            Ok(value) => callback(value),
            Err(e) => eprintln!("[tau::event] Failed to deserialize on '{}': {}", "<channel>", e),
        }
    })
}

// =============================================================================
// Convenience: Simple string events (no serde dependency)
// =============================================================================

/// Emit a string event.
pub fn emit_str(channel: &str, msg: &str) -> u32 {
    emit_raw(channel, msg.as_bytes())
}

/// Subscribe to string events.
pub fn subscribe_str<F>(channel: &str, callback: F) -> Subscription
where
    F: Fn(&str) + Send + 'static,
{
    subscribe_raw(channel, move |data| {
        let s = std::str::from_utf8(data).unwrap_or("<invalid utf8>");
        callback(s);
    })
}
