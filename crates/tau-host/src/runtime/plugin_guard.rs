//! Re-exported from `tau::guard`.
//!
//! Host-side convenience: create a [`PluginGuard`] from a raw `dlopen` handle.

pub use tau::guard::*;

/// Create a [`PluginGuard`] from a raw `libc::dlopen` handle.
///
/// This is the host-side constructor. The guard takes ownership of the handle
/// and will call `libc::dlclose` when the last clone is dropped.
///
/// # Safety
///
/// `dl_handle` must be a valid, non-null handle returned by `libc::dlopen`.
pub unsafe fn guard_from_dlopen(dl_handle: *mut libc::c_void, plugin_id: u64) -> PluginGuard {
    unsafe fn dlclose_fn(handle: *mut ()) {
        libc::dlclose(handle as *mut libc::c_void);
    }

    PluginGuard::new(dl_handle as *mut (), dlclose_fn, plugin_id)
}
