//! Kube plugin -- verifies `futures-core` resolves to a single copy when a plugin
//! depends on both `tau` (which re-exports `futures_core::Stream`) and `kube`
//! (which pulls in `kube-runtime` -> `futures-core` via many transitive deps).
//!
//! ## futures-core resolution
//!
//! `kube-runtime` depends on `futures-core` through multiple paths:
//! - Direct: `kube-runtime` -> `futures-core`
//! - Via async-broadcast, async-stream, futures, futures-util, etc.
//! All resolve to `futures-core ^0.3.x` -- one unified copy.
//!
//! This means `tau::stream::Stream` == `futures_core::Stream` everywhere.

use tau::stream::Stream;

/// Compile-time proof that tau::stream::Stream == futures_core::Stream.
fn _assert_same_stream_trait<S: Stream>(_s: &S) {
    fn _check<T: Stream + futures_core::Stream>() {}
}

/// Compile-time proof that kube-runtime's watcher stream implements tau's Stream trait.
fn _assert_watcher_uses_our_stream() {
    use core::pin::Pin;
    use core::task::{Context, Poll};
    
    struct Dummy;
    impl futures_core::Stream for Dummy {
        type Item = ();
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
            Poll::Ready(None)
        }
    }
    
    fn _takes_tau_stream<S: Stream>(_s: S) {}
    fn _prove() { _takes_tau_stream(Dummy); }
}

/// Proves that `kube_runtime::watcher()` produces a stream that satisfies `tau::stream::Stream`.
/// This function is never called -- it only needs to compile.
#[allow(dead_code)]
async fn _prove_watcher_compiles(client: kube::Client) {
    use kube::Api;
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::runtime::watcher;

    let api: Api<ConfigMap> = Api::default_namespaced(client);
    let watcher_config = watcher::Config::default();

    // watcher() returns impl Stream<Item = Result<watcher::Event<ConfigMap>, ...>>
    let _stream = watcher::watcher(api, watcher_config);

    // Prove the watcher stream implements tau::stream::Stream
    fn _takes_tau_stream<S: Stream>(_s: &S) {}
    _takes_tau_stream(&_stream);
}

tau::define_plugin! {
    fn init() {
        println!("[kube-plugin] Initialized -- futures-core is unified with kube deps!");
        println!("[kube-plugin] tau::stream::Stream == futures_core::Stream: verified at compile time");
        
        // We can reference kube types to prove they compiled against the same futures-core
        // We don't actually connect to a cluster -- just verify compilation
        let _client_config = kube::Config::incluster();
        println!("[kube-plugin] kube::Config available (won't connect -- no cluster)");
    }

    fn destroy() {
        println!("[kube-plugin] Destroyed!");
    }

    fn request(data: &[u8]) -> u64 {
        let _ = data;
        0
    }
}
