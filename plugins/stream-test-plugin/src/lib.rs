//! Stream test plugin — verifies real `tokio-stream` compiles against our tokio shim.
//!
//! ## tokio-stream feature compatibility with tau-tokio shim
//!
//! | Feature        | Works? | Notes                                                    |
//! |----------------|--------|----------------------------------------------------------|
//! | (default/time) | ✅ YES | `IntervalStream`, `Timeout`, `ChunksTimeout` all compile |
//! | sync (base)    | ✅ YES | `ReceiverStream`, `UnboundedReceiverStream` work         |
//! | sync (feature) | ❌ NO  | Needs `broadcast` module + `tokio-util` ReusableBoxFuture|
//! | net            | ❌ NO  | Needs `TcpListener::poll_accept`, `UnixListener`         |
//! | signal         | ❌ NO  | Needs `Signal::poll_recv`                                |
//! | io-util        | ❌ NO  | Needs `Lines<R>`, `Split<R>` structs                     |
//! | fs             | ❌ NO  | Needs `ReadDir::poll_next_entry`                         |
//!
//! The default features (`time`) plus base sync (always enabled by tokio-stream)
//! cover the most common use cases: `ReceiverStream`, `StreamExt`, time-based
//! stream combinators.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

tau::define_plugin! {
    fn init() {
        println!("[stream-test] Initialized — tokio-stream compiles against tau-tokio shim!");

        // Demonstrate: create mpsc channel, wrap in ReceiverStream, use StreamExt combinators
        tau::spawn(async {
            let (tx, rx) = mpsc::channel::<i32>(16);

            // Wrap receiver in tokio-stream's ReceiverStream
            let stream = ReceiverStream::new(rx);

            // Use StreamExt combinators from the real tokio-stream crate
            let mut mapped = stream.map(|x| x * 2).filter(|x| *x > 4);

            // Send some values
            tx.send(1).await.unwrap();
            tx.send(2).await.unwrap();
            tx.send(3).await.unwrap();
            tx.send(5).await.unwrap();
            drop(tx); // close channel

            // Consume with .next()
            let mut results = Vec::new();
            while let Some(val) = mapped.next().await {
                results.push(val);
            }

            // Expected: [6, 10] (1*2=2 filtered, 2*2=4 filtered, 3*2=6 kept, 5*2=10 kept)
            println!("[stream-test] Results: {:?}", results);
            assert_eq!(results, vec![6, 10]);
            println!("[stream-test] All stream operations succeeded!");
        });
    }

    fn destroy() {
        println!("[stream-test] Destroyed!");
    }

    fn request(data: &[u8]) -> u64 {
        let _ = data;
        0
    }
}
