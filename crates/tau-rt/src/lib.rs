//! Tau Runtime — shared dynamic library
//!
//! This crate is compiled as a **dylib**: one copy is loaded into the process,
//! shared by the host and all plugins. It provides the core runtime API:
//!
//! - **Task spawning**: [`spawn`], [`block_on`], [`JoinHandle`]
//! - **Timers**: [`sleep`], [`SleepFuture`]
//! - **Resources**: [`resource`] — typed key-value store across plugins
//! - **Events**: [`event`] — typed pub-sub event bus
//! - **IO**: [`io`] — reactor integration for async file descriptors
//! - **FFI types**: [`types`] — `FfiPoll`, `FfiWaker`, `RawPollFn`, `RawDropFn`
//!
//! All runtime calls resolve at load time to symbols exported by the host binary
//! via dynamic linking (`-Wl,-undefined,dynamic_lookup` + `-rdynamic`).
//!
//! # Not in this crate
//!
//! Per-plugin types (`PluginGuard`, `PluginBox`, `PluginArc`, `PluginFn`,
//! `define_plugin!`) live in the `tau` rlib crate, which is statically compiled
//! into each plugin's cdylib. This gives each plugin its own copy of the statics,
//! enabling per-plugin guard storage.

mod runtime;
pub mod types;
pub mod io;
pub mod resource;
pub mod event;

pub use runtime::*;
pub use types::*;
