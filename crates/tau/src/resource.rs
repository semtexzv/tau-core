//! Typed resource registry.
//!
//! Plugins store and retrieve named, typed values. The host holds raw pointers;
//! the typed API lives here (generic, monomorphized in each plugin).
//!
//! Only `Clone + Send + Sync + 'static` types are allowed.
//! `get()` clones the value using the *calling* plugin's Clone impl (safe after reload).
//!
//! The host automatically drops leftover resources when a plugin is unloaded
//! (calling `drop_fn` while the plugin's code is still loaded).
//!
//! # Why Clone, not references
//!
//! `get()` returns an owned clone, not a reference. This is intentional:
//! the stored value may contain internal pointers or vtables from the plugin
//! that called `put()`. By cloning in the *calling* plugin's code, the new
//! value's drop glue and any derived trait impls resolve to the caller's
//! `.text` section — not the original creator's. This makes the clone safe
//! to use even if the creating plugin is later unloaded.
//!
//! For types that are pure concrete data (`String`, `Vec<u8>`, numeric
//! structs), cloning is always safe because both plugins have identical
//! layout and their own copy of the drop glue. For types that might contain
//! trait objects or function pointers, cloning produces a fresh value with
//! pointers into the *cloning* plugin's code — which is the correct behavior.

use std::any::TypeId;

// FFI — resolved at load time from host
extern "C" {
    fn tau_resource_put(
        name_ptr: *const u8,
        name_len: usize,
        ptr: *mut (),
        type_id: u128,
        plugin_id: u64,
        drop_fn: unsafe extern "C" fn(*mut ()),
    );

    fn tau_resource_get(
        name_ptr: *const u8,
        name_len: usize,
        type_id: u128,
    ) -> *const ();

    fn tau_resource_take(
        name_ptr: *const u8,
        name_len: usize,
        type_id: u128,
    ) -> *mut ();
}

fn type_id_bits<T: 'static>() -> u128 {
    unsafe { std::mem::transmute(TypeId::of::<T>()) }
}

extern "C" {
    fn tau_current_plugin_id() -> u64;
}

/// Get the active plugin ID from the host.
fn current_plugin() -> u64 {
    unsafe { tau_current_plugin_id() }
}

/// Store a resource in the global registry.
///
/// The value is boxed and stored as a raw pointer. The host holds it
/// until `take()` is called or the owning plugin is unloaded.
///
/// If a resource with the same name already exists, the old one is dropped.
pub fn put<T: Clone + Send + Sync + 'static>(name: &str, value: T) {
    let ptr = Box::into_raw(Box::new(value)) as *mut ();
    let type_id = type_id_bits::<T>();
    let plugin_id = current_plugin();

    unsafe extern "C" fn drop_fn<T>(ptr: *mut ()) {
        drop(Box::from_raw(ptr as *mut T));
    }

    unsafe {
        tau_resource_put(
            name.as_ptr(),
            name.len(),
            ptr,
            type_id,
            plugin_id,
            drop_fn::<T>,
        );
    }
}

/// Get a clone of a resource from the registry.
///
/// Returns `None` if the resource doesn't exist or the type doesn't match.
/// The clone is performed using the *calling* plugin's `Clone` impl,
/// so this is safe even if the owning plugin has been reloaded.
pub fn get<T: Clone + Send + Sync + 'static>(name: &str) -> Option<T> {
    let type_id = type_id_bits::<T>();
    let ptr = unsafe { tau_resource_get(name.as_ptr(), name.len(), type_id) };
    if ptr.is_null() {
        return None;
    }
    // Clone from the stored value — Clone impl is in THIS plugin's code
    let reference = unsafe { &*(ptr as *const T) };
    Some(reference.clone())
}

/// Remove a resource from the registry and return it.
///
/// Returns `None` if not found or type mismatch.
/// The caller takes ownership; the host no longer tracks it.
pub fn take<T: Clone + Send + Sync + 'static>(name: &str) -> Option<T> {
    let type_id = type_id_bits::<T>();
    let ptr = unsafe { tau_resource_take(name.as_ptr(), name.len(), type_id) };
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { *Box::from_raw(ptr as *mut T) })
}
