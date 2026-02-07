//! Tau Runtime — thin dylib wrapper over host-provided executor.
//!
//! # Architecture
//!
//! tau-rt is a **dependency-free dylib** that plugins compile against.
//! The actual executor implementation lives in `tau-host`. This split is
//! intentional — see `executor.rs` for the full explanation.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      Plugin (cdylib)                         │
//! │  ┌─────────────────────────────────────────────────────────┐│
//! │  │  spawn(), JoinHandle, sleep() — monomorphized from tau  ││
//! │  └──────────────────────────┬──────────────────────────────┘│
//! └─────────────────────────────┼───────────────────────────────┘
//!                               │ calls
//!                               ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    tau-rt (dylib, THIS CRATE)                │
//! │  - Thin wrappers around extern "Rust" declarations          │
//! │  - NO external dependencies (only std)                      │
//! │  - Generic spawn/JoinHandle/block_on monomorphize here      │
//! └──────────────────────────────┬──────────────────────────────┘
//!                                │ extern "Rust" calls
//!                                ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      tau-host (binary)                       │
//! │  - Full executor implementation with crossbeam-queue        │
//! │  - IO reactor, resource registry, plugin management         │
//! │  - Exports #[no_mangle] tau_exec_* functions                │
//! └─────────────────────────────────────────────────────────────┘
//! ```

pub mod ffi;
pub mod executor;
mod runtime;
pub mod types;
pub mod io;
pub mod resource;
pub mod process;

pub use runtime::*;
pub use types::*;

// Re-export executor items for convenience
pub use executor::{
    allocate_plugin_id,
    set_current_plugin,
    clear_current_plugin,
    current_plugin_id,
    drop_plugin_tasks,
    wait_for_task,
    debug_enabled,
    debug_stats,
    pack_task_id,
    unpack_task_id,
    CURRENT_PLUGIN,
};
