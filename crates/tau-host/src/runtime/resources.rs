//! Resource Registry — named typed storage with automatic plugin-scoped cleanup.
//!
//! Host stores raw pointers + metadata. Plugins provide drop functions.
//! On plugin unload, all resources owned by that plugin are dropped (while code still loaded).

use std::collections::HashMap;
use std::sync::Mutex;

struct ResourceEntry {
    /// Raw pointer to the stored value (Box::into_raw)
    ptr: *mut (),
    /// TypeId bits for safety check on retrieval
    type_id: u128,
    /// Which plugin created this resource
    plugin_id: u64,
    /// Drop function — monomorphized in the plugin, called before dlclose
    drop_fn: unsafe extern "C" fn(*mut ()),
}

// Safety: Resources are accessed under mutex, and ptr points to Send+Sync data
unsafe impl Send for ResourceEntry {}

static RESOURCES: std::sync::OnceLock<Mutex<HashMap<String, ResourceEntry>>> = std::sync::OnceLock::new();

fn resources() -> &'static Mutex<HashMap<String, ResourceEntry>> {
    RESOURCES.get_or_init(|| Mutex::new(HashMap::new()))
}

// =============================================================================
// Host-side helpers (not FFI)
// =============================================================================

/// Check if a resource with the given name exists (for testing).
pub fn resource_exists(name: &str) -> bool {
    let map = resources().lock().unwrap();
    map.contains_key(name)
}

// =============================================================================
// FFI exports
// =============================================================================

/// Store a resource. Overwrites any existing resource with the same name
/// (calling the old one's drop_fn first).
#[no_mangle]
pub extern "C" fn tau_resource_put(
    name_ptr: *const u8,
    name_len: usize,
    ptr: *mut (),
    type_id: u128,
    plugin_id: u64,
    drop_fn: unsafe extern "C" fn(*mut ()),
) {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    let mut map = resources().lock().unwrap();

    // If overwriting, drop the old value
    if let Some(old) = map.remove(name) {
        unsafe { (old.drop_fn)(old.ptr) };
    }

    map.insert(name.to_string(), ResourceEntry {
        ptr,
        type_id,
        plugin_id,
        drop_fn,
    });
}

/// Retrieve a pointer to a resource. Returns null if not found or type mismatch.
/// The pointer is valid as long as the resource is in the registry.
#[no_mangle]
pub extern "C" fn tau_resource_get(
    name_ptr: *const u8,
    name_len: usize,
    type_id: u128,
) -> *const () {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    let map = resources().lock().unwrap();

    match map.get(name) {
        Some(entry) if entry.type_id == type_id => entry.ptr as *const (),
        _ => std::ptr::null(),
    }
}

/// Remove a resource and return its pointer. Caller takes ownership (must drop).
/// Returns null if not found or type mismatch.
/// Does NOT call drop_fn — the caller is expected to reconstruct and drop.
#[no_mangle]
pub extern "C" fn tau_resource_take(
    name_ptr: *const u8,
    name_len: usize,
    type_id: u128,
) -> *mut () {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    let mut map = resources().lock().unwrap();

    // Check type_id before removing
    match map.get(name) {
        Some(entry) if entry.type_id == type_id => {
            let entry = map.remove(name).unwrap();
            entry.ptr
        }
        _ => std::ptr::null_mut(),
    }
}

/// Drop all resources owned by a plugin. Called during plugin unload, BEFORE dlclose.
/// Returns the number of resources dropped.
pub fn drop_plugin_resources(plugin_id: u64) -> usize {
    let mut map = resources().lock().unwrap();

    let to_drop: Vec<(String, ResourceEntry)> = map
        .iter()
        .filter(|(_, entry)| entry.plugin_id == plugin_id)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .map(|name| {
            let entry = map.remove(&name).unwrap();
            (name, entry)
        })
        .collect();

    let count = to_drop.len();

    // Drop outside the lock to avoid deadlock if drop_fn touches resources
    drop(map);

    // Set CURRENT_PLUGIN during drops so any tau calls in destructors are correct
    use std::sync::atomic::Ordering;
    let prev_plugin = super::executor::CURRENT_PLUGIN.load(Ordering::Relaxed);
    super::executor::CURRENT_PLUGIN.store(plugin_id, Ordering::Relaxed);

    for (name, entry) in to_drop {
        #[cfg(debug_assertions)]
        if std::env::var("TAU_DEBUG").is_ok() {
            eprintln!("[rt] Dropping resource '{}' (plugin {})", name, plugin_id);
        }
        unsafe { (entry.drop_fn)(entry.ptr) };
    }

    super::executor::CURRENT_PLUGIN.store(prev_plugin, Ordering::Relaxed);

    count
}
