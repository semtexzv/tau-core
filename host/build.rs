fn main() {
    // Capture full rustc version (includes channel and commit hash)
    let output = std::process::Command::new("rustc")
        .arg("-V")
        .output()
        .expect("Failed to execute rustc");

    let version = String::from_utf8(output.stdout).expect("Invalid utf8");
    println!("cargo:rustc-env=HOST_RUSTC_VERSION={}", version.trim());

    // Capture RUSTFLAGS
    // Use 0x1f (unit separator) as the canonical separator in HOST_RUSTFLAGS
    if let Ok(flags) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        // Already encoded
        println!("cargo:rustc-env=HOST_RUSTFLAGS={}", flags);
    } else if let Ok(flags) = std::env::var("RUSTFLAGS") {
        // Convert space-separated to 0x1f-separated
        let encoded = flags.split_whitespace().collect::<Vec<_>>().join("\x1f");
        println!("cargo:rustc-env=HOST_RUSTFLAGS={}", encoded);
    } else {
        println!("cargo:rustc-env=HOST_RUSTFLAGS=");
    }

    // Capture TARGET
    let target = std::env::var("TARGET").expect("TARGET not set");
    println!("cargo:rustc-env=HOST_TARGET={}", target);

    // Capture PANIC strategy
    // Defaults to "unwind" if not set (unlikely for host build)
    let panic = std::env::var("CARGO_CFG_PANIC").unwrap_or_else(|_| "unwind".to_string());
    println!("cargo:rustc-env=HOST_PANIC={}", panic);

    // Export symbols for plugins
    println!("cargo:rustc-link-arg=-rdynamic");

    // Capture build-time env vars
    println!(
        "cargo:rustc-env=HOST_RUSTFLAGS={}",
        std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default()
    );
    // Capture TARGET FEATURES
    // Cargo sets this as a comma-separated list for the *target* being built.
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    println!("cargo:rustc-env=HOST_TARGET_FEATURES={}", features);

    // Rerun if git HEAD changes (just in case, though mostly relevant for crates)
    println!("cargo:rerun-if-changed=build.rs");
}
