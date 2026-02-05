use std::time::Duration;

#[no_mangle]
pub extern "C" fn plugin_main() {
    // Use tokio API - should get our taukio shim!
    let _handle = tokio::spawn(async {
        println!("[TokioTest] Spawned async task!");
        42
    });
    
    // Use tokio::time
    let _sleep_future = tokio::time::sleep(Duration::from_millis(100));
    
    // Use tokio::sync
    let (tx, _rx) = tokio::sync::mpsc::channel::<i32>(10);
    let _ = tx.send(123);
    
    println!("[TokioTest] Taukio shim works! Used spawn, sleep, mpsc");
}
