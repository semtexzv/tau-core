//! Host-side PluginGuard constructor.
//!
//! Creates a [`PluginGuard`](tau::PluginGuard) from a raw `dlopen` handle.

/// Create a [`PluginGuard`](tau::PluginGuard) from a raw `libc::dlopen` handle.
///
/// The guard prevents `dlclose` while any `PluginBox`/`PluginArc`/`PluginFn`
/// values from this plugin exist. When the last clone is dropped, `dlclose` runs.
///
/// # Safety
///
/// `dl_handle` must be a valid, non-null handle returned by `libc::dlopen`.
pub unsafe fn guard_from_dlopen(dl_handle: *mut libc::c_void, plugin_id: u64) -> tau::PluginGuard {
    unsafe fn dlclose_fn(handle: *mut ()) {
        libc::dlclose(handle as *mut libc::c_void);
    }

    tau::PluginGuard::new(dl_handle as *mut (), dlclose_fn, plugin_id)
}
