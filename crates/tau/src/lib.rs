//! Tau Plugin SDK
//!
//! This crate is the main entry point for plugin authors. It is compiled as an
//! **rlib** (static library) — each plugin's cdylib gets its own copy. This
//! enables per-plugin statics like [`PLUGIN_GUARD`].
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────┐    ┌─────────────────────┐
//! │  Plugin A (cdylib)  │    │  Plugin B (cdylib)  │
//! │  ┌───────────────┐  │    │  ┌───────────────┐  │
//! │  │ tau (rlib)     │  │    │  │ tau (rlib)     │  │
//! │  │ own GUARD static│ │    │  │ own GUARD static│ │
//! │  └───────┬───────┘  │    │  └───────┬───────┘  │
//! └──────────┼──────────┘    └──────────┼──────────┘
//!            │  links to dylib          │
//!            ▼                          ▼
//!    ┌──────────────────────────────────────┐
//!    │       tau-rt (dylib, shared)          │
//!    │  spawn, sleep, resource, event, io    │
//!    └──────────────────────────────────────┘
//! ```
//!
//! # What lives where
//!
//! | Crate | Link type | Contains |
//! |-------|-----------|----------|
//! | `tau-rt` | dylib (shared) | Runtime FFI: spawn, sleep, JoinHandle, resource, event, io |
//! | `tau` | rlib (per-plugin) | PluginGuard, PluginBox, PluginArc, PluginFn, define_plugin!, per-plugin statics |
//!
//! # Usage
//!
//! ```rust,ignore
//! use tau::define_plugin;
//! use tau::rt::spawn;
//! use tau::guard::PluginBox;
//! ```

// =============================================================================
// Re-export tau-rt as `tau::rt`
// =============================================================================

/// Runtime API — re-exported from the shared `tau-rt` dylib.
///
/// Contains `spawn`, `sleep`, `JoinHandle`, `block_on`, `drive`, `init`.
pub mod rt {
    pub use tau_rt::*;
}

// =============================================================================
// Per-plugin modules (compiled into each plugin's binary)
// =============================================================================

/// Plugin binary lifetime guards and safe wrappers for dynamic types.
///
/// `PluginBox`, `PluginArc`, `PluginFn` keep a plugin's shared library loaded
/// as long as any trait object / function pointer from that plugin exists.
pub mod guard;

/// Async stream primitives — re-exports `futures_core::Stream`.
pub mod stream;

/// Plugin definition macro.
mod plugin;

// =============================================================================
// Convenience re-exports at crate root
// =============================================================================

// Runtime essentials
pub use rt::{spawn, sleep, block_on, drive, init, JoinHandle, SleepFuture};

// Guard types
pub use guard::{PluginGuard, PluginBox, PluginArc, PluginFn};

// Sub-modules from tau-rt, re-exported for convenience
pub use rt::types;
pub use rt::io;
pub use rt::resource;
pub use rt::event;
pub use rt::process;
