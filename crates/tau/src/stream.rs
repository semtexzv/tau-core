//! Async stream primitives.
//!
//! Re-exports the standard [`Stream`] trait from `futures-core` so that plugins
//! use the exact same trait the wider async ecosystem depends on (hyper, tower,
//! tonic, kube, reqwest, etc.).

pub use futures_core::Stream;
