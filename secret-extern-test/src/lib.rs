#[no_mangle]
pub extern "C" fn plugin_run() {
    // Use tokio even though it's not in Cargo.toml!
    let _handle = tokio::spawn(async {
        println!("[Secret] Spawn works!");
    });
}
