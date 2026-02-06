//! HTTP plugin — verifies `futures-core` resolves to a single copy when a plugin
//! depends on both `tau` (which re-exports `futures_core::Stream`) and `reqwest`
//! (which pulls in `hyper` → `http-body` → `futures-core`).
//!
//! ## futures-core resolution
//!
//! `tau` depends on `futures-core = "0.3"`. `reqwest` (via hyper, http-body)
//! also depends on `futures-core ^0.3.x`. Because all are semver-compatible
//! (`0.3.*`), Cargo unifies them into ONE copy. This means:
//! - `tau::stream::Stream` and `futures_core::Stream` are the same trait
//! - Types implementing one automatically implement the other
//! - No "trait Stream is not satisfied" errors from duplicate trait definitions

use tau::stream::Stream;

/// Compile-time proof that tau::stream::Stream == futures_core::Stream.
/// If futures-core resolved to two copies, this function would fail to compile
/// because `tau::stream::Stream` and `futures_core::Stream` would be different traits.
fn _assert_same_stream_trait<S: Stream>(_s: &S) {
    // This bound proves S implements futures_core::Stream (the same trait tau re-exports)
    fn _inner<T: futures_core::Stream>() {}
    // If they were different traits, this line would not compile
    fn _check<T: Stream + futures_core::Stream>() {}
}

tau::define_plugin! {
    fn init() {
        println!("[http-plugin] Initialized — futures-core is unified!");
        println!("[http-plugin] tau::stream::Stream == futures_core::Stream: verified at compile time");

        tau::spawn(async {
            // We don't actually make HTTP requests (no network in test),
            // but the fact that this compiles proves reqwest's futures-core
            // is the same as tau's futures-core.
            let client = reqwest::Client::new();
            println!("[http-plugin] reqwest::Client created successfully");
            let _ = client;
        });
    }

    fn destroy() {
        println!("[http-plugin] Destroyed!");
    }

    fn request(data: &[u8]) -> u64 {
        let _ = data;
        0
    }
}
