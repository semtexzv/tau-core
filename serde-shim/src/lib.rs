//! Serde shim - re-exports real serde
//!
//! This crate is named "serde" but wraps the real serde crate.
//! It allows us to provide a prebuilt serde to plugins.

pub use serde_real::*;
