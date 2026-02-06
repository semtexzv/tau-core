//! Crossterm integration test plugin — proves crossterm's sync and async event APIs
//! work through the tau reactor.
//!
//! ## Supported commands
//!
//! - `size`        — calls `crossterm::terminal::size()`, prints result
//! - `poll <ms>`   — calls `crossterm::event::poll(Duration::from_millis(ms))`, reports result
//! - `read`        — calls `crossterm::event::read()`, prints the event
//! - `stream`      — spawns a task that creates an `EventStream`, reads up to 5 events via `.next().await`
//!
//! ## Notes
//!
//! This plugin enables raw mode on init and disables it on destroy.
//! The `stream` command requires the `event-stream` feature on crossterm.

use std::time::Duration;

use crossterm::{cursor, event, terminal, execute};
use serde::Deserialize;
use tau::stream::StreamExt;

#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    id: u64,
    payload: String,
}

tau::define_plugin! {
    fn init() {
        // Enable raw mode and hide cursor
        match terminal::enable_raw_mode() {
            Ok(()) => println!("[crossterm-test] Raw mode enabled"),
            Err(e) => println!("[crossterm-test] Failed to enable raw mode: {}", e),
        }
        match execute!(std::io::stdout(), cursor::Hide) {
            Ok(()) => println!("[crossterm-test] Cursor hidden"),
            Err(e) => println!("[crossterm-test] Failed to hide cursor: {}", e),
        }
        println!("[crossterm-test] Initialized!");
    }

    fn destroy() {
        // Restore terminal state
        match execute!(std::io::stdout(), cursor::Show) {
            Ok(()) => println!("[crossterm-test] Cursor restored"),
            Err(e) => println!("[crossterm-test] Failed to show cursor: {}", e),
        }
        match terminal::disable_raw_mode() {
            Ok(()) => println!("[crossterm-test] Raw mode disabled"),
            Err(e) => println!("[crossterm-test] Failed to disable raw mode: {}", e),
        }
        println!("[crossterm-test] Destroyed!");
    }

    fn request(data: &[u8]) -> u64 {
        let msg: Message = serde_json::from_slice(data).expect("invalid JSON from host");
        let parts: Vec<&str> = msg.payload.trim().splitn(2, ' ').collect();
        let cmd = parts.first().copied().unwrap_or("");

        match cmd {
            "size" => {
                match terminal::size() {
                    Ok((cols, rows)) => println!("[crossterm-test] size: {}x{}", cols, rows),
                    Err(e) => println!("[crossterm-test] size error: {}", e),
                }
                0
            }

            "poll" => {
                let ms: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                match event::poll(Duration::from_millis(ms)) {
                    Ok(ready) => println!("[crossterm-test] poll({}ms): ready={}", ms, ready),
                    Err(e) => println!("[crossterm-test] poll error: {}", e),
                }
                0
            }

            "read" => {
                match event::read() {
                    Ok(ev) => println!("[crossterm-test] read: {:?}", ev),
                    Err(e) => println!("[crossterm-test] read error: {}", e),
                }
                0
            }

            "stream" => {
                let handle = tau::spawn(async {
                    let mut stream = event::EventStream::new();
                    println!("[crossterm-test] EventStream created");

                    // Read up to 5 events from the stream
                    for i in 0..5 {
                        match stream.next().await {
                            Some(Ok(ev)) => println!("[crossterm-test] stream event {}: {:?}", i, ev),
                            Some(Err(e)) => {
                                println!("[crossterm-test] stream error {}: {}", i, e);
                                break;
                            }
                            None => {
                                println!("[crossterm-test] stream ended at {}", i);
                                break;
                            }
                        }
                    }
                    println!("[crossterm-test] stream task done");
                });
                handle.task_id()
            }

            _ => {
                println!("[crossterm-test] unknown command: {}", cmd);
                0
            }
        }
    }
}
