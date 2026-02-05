//! µTokio Interface - Plugin API
//!
//! This crate provides the public API for plugins. All calls are resolved
//! at runtime to symbols provided by the host's utokio-runtime dylib.

mod runtime;
pub mod types;

pub use runtime::*;
pub use types::*;

/// Attribute macro for async main (placeholder - would be a proc macro)
/// For now, plugins use block_on() directly
#[macro_export]
macro_rules! main {
    ($body:block) => {
        fn main() {
            $crate::block_on(async $body);
        }
    };
}
pub use serde;
pub use serde_json as json;
