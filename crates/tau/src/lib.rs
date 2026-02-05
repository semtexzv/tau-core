//! Tau Interface — Plugin API
//!
//! This crate provides the public API that plugins link against.
//! All runtime calls are resolved at load time to symbols exported by the host binary.
//! Compiled as both dylib (shared) and rlib, with -C prefer-dynamic.
//!
//! # Same-Compiler Invariant
//!
//! **The host compiles all plugins using the exact same `rustc` version and flags.**
//! This is enforced by the host's compiler infrastructure (`tau-host::compiler`).
//! Plugins are never pre-compiled or distributed as binaries — they are always
//! built from source by the host process.
//!
//! This invariant guarantees that `repr(Rust)` type layouts and generic
//! monomorphizations are identical across the host and all loaded plugins.
//! Every plugin gets its own copy of generic code (drop glue, clone impls, etc.)
//! but the copies are **layout-compatible** — same field order, same size, same
//! alignment, same allocator.
//!
//! # Concrete data CAN cross plugin boundaries
//!
//! Because every plugin has its own duplicate of the type's code (constructors,
//! destructors, `Clone` impls, etc.) and all plugins share the same global
//! allocator, **owned concrete data can be freely sent between plugins**:
//!
//! - `String`, `Vec<T>`, `Box<T>`, `HashMap<K,V>` — any `repr(Rust)` struct
//! - `Arc<T>`, `Mutex<T>` — shared-ownership types (ref counting works, same allocator)
//! - Nested structs, enums, tuples — any concrete type with a known layout
//!
//! Plugin A can allocate a `Vec<String>`, send it to Plugin B, and Plugin B
//! can read, mutate, and drop it. Plugin B's drop glue for `Vec<String>` is
//! a separate copy of the same code, calling the same allocator. This is safe.
//!
//! # Dynamic types CANNOT cross plugin boundaries (without a guard)
//!
//! **Bare trait objects and function pointers are unsafe to send across plugins.**
//!
//! Dynamic dispatch types contain pointers into the creating plugin's `.text`
//! section (vtables, function pointers, closure code). If that plugin is
//! unloaded via `dlclose`, those pointers dangle — **use-after-free**.
//!
//! Unlike concrete data (where every plugin has its own copy of the code),
//! a vtable or function pointer refers to **one specific copy** in one specific
//! plugin's binary. There is no way to "re-resolve" it after unload.
//!
//! Unsafe to send **bare** across plugins:
//! - `Box<dyn Trait>`, `&dyn Trait`, `Arc<dyn Trait>` — vtable points into creator's `.text`
//! - `Box<dyn Future>`, `Box<dyn Stream>` — poll function is in creator's `.text`
//! - `Box<dyn Fn()>`, `Box<dyn FnOnce()>` — closure code is in creator's `.text`
//! - `fn()` pointers, `extern "C" fn()` — same problem
//!
//! # `PluginBox` / `PluginArc` — safe wrappers for dynamic types
//!
//! The host provides `PluginBox<dyn Trait>` and `PluginArc<dyn Trait>` which
//! pair a trait object with a ref-counted guard (`PluginGuard`) that keeps the
//! creating plugin's shared library loaded. The plugin binary cannot be
//! `dlclose`'d while any guarded references exist.
//!
//! On hot-reload, the old binary stays mapped alongside the new one. Old
//! `PluginBox`/`PluginArc` values continue to work (their vtables point into
//! the still-mapped old binary). When the last guarded reference drops, the
//! old binary is finally unmapped.
//!
//! See `tau_host::runtime::plugin_guard` for the implementation.
//!
//! # Safe patterns for cross-plugin work
//!
//! | Want to share | Do this |
//! |---|---|
//! | A computed value | Send the owned `T` directly (concrete data, safe) |
//! | A trait object | Wrap in `PluginBox<dyn Trait>` or `PluginArc<dyn Trait>` |
//! | A future's result | Spawn in Plugin A, send result `T` via channel to Plugin B |
//! | A stream | Wrap in `PluginBox<dyn Stream<Item = T>>` (keeps creator alive) |
//! | Shared state | Use [`resource::put`] / [`resource::get`] (host holds data, plugins get clones) |
//! | Events | Use [`event`] (host dispatches, callbacks stay in owning plugin) |
//!
//! # The executor's type-erasure pattern
//!
//! The executor must store futures from any plugin in a single `HashMap<u64, Task>`.
//! It does this via `extern "C"` function pointers (`poll_fn`, `drop_fn`) that are
//! created in the spawning plugin's code and stored alongside the opaque `*mut ()`
//! future pointer. This is safe because:
//!
//! 1. The `poll_fn`/`drop_fn` point into the plugin that spawned the task
//! 2. The host tracks `plugin_id` on every task
//! 3. On plugin unload, the host drops all tasks owned by that plugin **before**
//!    calling `dlclose`, so the function pointers are still valid during cleanup
//!
//! The result type `T` never crosses the FFI boundary — it stays in the
//! `TaskCell<T>` which is allocated and consumed entirely within the plugin.
//! When Plugin B needs the result, Plugin A sends the concrete `T` through a
//! channel — not the future or the `JoinHandle`.

mod runtime;
pub mod types;
pub mod io;
pub mod resource;
pub mod event;
pub mod guard;
mod plugin;

pub use runtime::*;
pub use types::*;
pub use guard::{PluginGuard, PluginBox, PluginArc, PluginFn};
