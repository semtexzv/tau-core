//! Plugin definition macro and helpers.
//!
//! Generates the `#[no_mangle] extern "C"` boilerplate so plugin authors
//! just write normal Rust functions.

/// Define a tau plugin.
///
/// Generates `plugin_init`, `plugin_destroy`, and the process hook.
/// Stores the plugin_id in a local static so `tau::plugin_id()` works.
///
/// # Example
///
/// ```rust
/// use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
///
/// tau::define_plugin! {
///     fn init() {
///         tau::resource::put("counter", Arc::new(AtomicU64::new(0)));
///     }
///
///     fn destroy() {
///         println!("goodbye!");
///     }
///
///     fn request(data: &[u8]) -> u64 {
///         let task = tau::spawn(async {
///             tau::sleep(std::time::Duration::from_millis(10)).await;
///         });
///         task.task_id()
///     }
/// }
/// ```
#[macro_export]
macro_rules! define_plugin {
    (
        fn init() $init_body:block

        fn destroy() $destroy_body:block

        fn request($data:ident : &[u8]) -> u64 $request_body:block
    ) => {
        // Plugin-local storage for the plugin ID
        static __TAU_PLUGIN_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        /// Get this plugin's ID (assigned by the host at load time).
        #[allow(dead_code)]
        pub fn plugin_id() -> u64 {
            __TAU_PLUGIN_ID.load(std::sync::atomic::Ordering::Relaxed)
        }

        // The hooks struct must match the host's layout
        #[repr(C)]
        struct __TauPluginHooks {
            process: unsafe extern "C" fn(*const u8, usize) -> u64,
        }

        // Trampoline: extern "C" → safe Rust
        unsafe extern "C" fn __tau_process_trampoline(ptr: *const u8, len: usize) -> u64 {
            let $data: &[u8] = unsafe { std::slice::from_raw_parts(ptr, len) };
            $request_body
        }

        #[no_mangle]
        pub unsafe extern "C" fn plugin_init(hooks: *mut __TauPluginHooks, pid: u64) -> i32 {
            // Store our plugin ID
            __TAU_PLUGIN_ID.store(pid, std::sync::atomic::Ordering::Relaxed);
            // Wire up hooks
            (*hooks).process = __tau_process_trampoline;
            // User init
            (|| $init_body)();
            0
        }

        #[no_mangle]
        pub extern "C" fn plugin_destroy() {
            (|| $destroy_body)();
        }
    };

    // Shorthand: no init/destroy
    (
        fn request($data:ident : &[u8]) -> u64 $request_body:block
    ) => {
        $crate::define_plugin! {
            fn init() {}
            fn destroy() {}
            fn request($data : &[u8]) -> u64 $request_body
        }
    };
}
