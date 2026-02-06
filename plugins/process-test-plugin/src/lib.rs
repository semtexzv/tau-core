//! Process test plugin — exercises tokio::process::Command shim.

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;

#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    id: u64,
    payload: String,
}

static SUCCESS: AtomicBool = AtomicBool::new(false);

tau::define_plugin! {
    fn init() {
        println!("[process-test] Initialized");
    }

    fn destroy() {
        println!("[process-test] Destroyed");
    }

    fn request(data: &[u8]) -> u64 {
        let msg: Message = serde_json::from_slice(data).expect("invalid JSON from host");
        let cmd = msg.payload.as_str();
        println!("[process-test] Command: {}", cmd);

        match cmd {
            "run-echo" => {
                SUCCESS.store(false, Ordering::SeqCst);
                tau::spawn(async {
                    println!("[process-test] Spawning echo...");
                    // Use 'echo' command which should be available on unix
                    let result = Command::new("echo")
                        .arg("hello world")
                        .output()
                        .await;

                    match result {
                        Ok(output) => {
                            let stdout_str = String::from_utf8_lossy(&output.stdout);
                            println!("[process-test] Output status={:?}, stdout={:?}", output.status, stdout_str);
                            
                            let expected = "hello world\n";
                            if output.status.success() && stdout_str == expected {
                                println!("[process-test] Echo success");
                                SUCCESS.store(true, Ordering::SeqCst);
                            } else {
                                println!("[process-test] Echo mismatch. Expected {:?}, got {:?}", expected, stdout_str);
                            }
                        }
                        Err(e) => {
                            println!("[process-test] Failed to execute echo: {}", e);
                        }
                    }
                });
                0
            }

            "spawn-kill" => {
                SUCCESS.store(false, Ordering::SeqCst);
                tau::spawn(async {
                    println!("[process-test] Spawning sleep 10...");
                    let mut child = Command::new("sleep").arg("10").spawn().expect("spawn failed");
                    let pid = child.id().unwrap();
                    println!("[process-test] Spawned pid={}", pid);
                    
                    tau::sleep(std::time::Duration::from_millis(100)).await;
                    
                    println!("[process-test] Killing...");
                    child.kill().await.expect("kill failed");
                    let status = child.wait().await.expect("wait failed");
                    
                    println!("[process-test] Child exited: {:?}", status);
                    // On Unix, killed by signal means !success
                    if !status.success() {
                        println!("[process-test] Kill success");
                        SUCCESS.store(true, Ordering::SeqCst);
                    } else {
                        println!("[process-test] Kill failed (success status?)");
                    }
                });
                0
            }

            "pipe-cat" => {
                SUCCESS.store(false, Ordering::SeqCst);
                tau::spawn(async {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    println!("[process-test] Spawning cat...");
                    let mut child = Command::new("cat")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        .expect("spawn cat failed");
                    
                    let mut stdin = child.stdin.take().expect("no stdin");
                    let mut stdout = child.stdout.take().expect("no stdout");
                    
                    tau::spawn(async move {
                        stdin.write_all(b"meow").await.expect("write failed");
                        stdin.shutdown().await.expect("shutdown failed");
                    });
                    
                    let mut buf = Vec::new();
                    stdout.read_to_end(&mut buf).await.expect("read failed");
                    let buf_str = String::from_utf8(buf).expect("invalid utf8");
                    let status = child.wait().await.expect("wait failed");
                    
                    println!("[process-test] cat output={:?} status={:?}", buf_str, status);
                    if status.success() && buf_str == "meow" {
                        println!("[process-test] Cat success");
                        SUCCESS.store(true, Ordering::SeqCst);
                    }
                });
                0
            }

            "kill-on-drop" => {
                SUCCESS.store(false, Ordering::SeqCst);
                tau::spawn(async {
                    println!("[process-test] Spawning sleep 100 with kill_on_drop...");
                    let mut cmd = Command::new("sleep");
                    cmd.arg("100");
                    cmd.kill_on_drop(true);
                    let child = cmd.spawn().expect("spawn failed");
                    let pid = child.id().expect("no pid");
                    
                    drop(child); // Should kill
                    println!("[process-test] Dropped child pid={}", pid);
                    
                    // Give it a moment to die
                    tau::sleep(std::time::Duration::from_millis(100)).await;
                    
                    // Check if running using kill -0
                    let status = Command::new("kill")
                        .arg("-0")
                        .arg(pid.to_string())
                        .status()
                        .await;
                        
                    // If kill -0 succeeds, process exists (bad). If fails, process gone (good).
                    match status {
                        Ok(s) if !s.success() => {
                            println!("[process-test] Process gone (good)");
                            SUCCESS.store(true, Ordering::SeqCst);
                        }
                        Ok(_) => println!("[process-test] Process still running (bad)"),
                        Err(e) => println!("[process-test] Failed to check process: {}", e),
                    }
                });
                0
            }

            "check" => {
                let success = SUCCESS.load(Ordering::SeqCst);
                println!("[process-test] check success={}", success);
                if success { 1 } else { 0 }
            }

            _ => {
                println!("[process-test] Unknown command: {}", cmd);
                0
            }
        }
    }
}
