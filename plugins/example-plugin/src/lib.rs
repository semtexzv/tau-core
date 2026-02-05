//! Example plugin demonstrating tau SDK usage.
//!
//! This plugin:
//! - Uses `tau::define_plugin!` macro (generates init/destroy with guard storage)
//! - Spawns async tasks via `tau::spawn` (from tau-rt dylib)
//! - Uses `tau::guard::plugin_guard()` to access the per-plugin guard
//! - Creates `PluginBox` values that keep this plugin's binary alive

use tau::guard::{PluginBox, PluginGuard};

tau::define_plugin! {
    fn init() {
        // Verify our per-plugin guard was stored during init
        let guard = PluginGuard::current();
        println!(
            "[example-plugin] Initialized! plugin_id={}, guard refs={}",
            plugin_id(),
            guard.ref_count()
        );

        // Create a PluginBox using the per-plugin guard
        let boxed: PluginBox<dyn Fn() -> &'static str + Send> = PluginBox::new(
            Box::new(|| "hello from guarded closure"),
            guard,
        );
        println!("[example-plugin] Guarded closure says: {}", boxed.call());
    }

    fn destroy() {
        println!("[example-plugin] Destroyed!");
    }

    fn request(data: &[u8]) -> u64 {
        let msg = std::str::from_utf8(data).unwrap_or("<invalid>");
        println!("[example-plugin] Got request: {}", msg);

        // Spawn an async task using tau-rt (shared dylib)
        let handle = tau::spawn(async {
            println!("[example-plugin] Async task running!");
            tau::sleep(std::time::Duration::from_millis(10)).await;
            println!("[example-plugin] Async task done!");
        });

        // Demonstrate that PluginGuard::current() works in the request path too
        let guard = PluginGuard::current();
        println!(
            "[example-plugin] Request handler: guard refs={}",
            guard.ref_count()
        );

        handle.task_id()
    }
}
