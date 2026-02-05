fn main() {
    // Capture full rustc version (includes channel and commit hash)
    let output = std::process::Command::new("rustc")
        .arg("-V")
        .output()
        .expect("Failed to execute rustc");

    let version = String::from_utf8(output.stdout).expect("Invalid utf8");
    println!("cargo:rustc-env=HOST_RUSTC_VERSION={}", version.trim());

    // Capture RUSTFLAGS (0x1f unit separator encoding)
    if let Ok(flags) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        println!("cargo:rustc-env=HOST_RUSTFLAGS={}", flags);
    } else if let Ok(flags) = std::env::var("RUSTFLAGS") {
        let encoded = flags.split_whitespace().collect::<Vec<_>>().join("\x1f");
        println!("cargo:rustc-env=HOST_RUSTFLAGS={}", encoded);
    } else {
        println!("cargo:rustc-env=HOST_RUSTFLAGS=");
    }

    // Capture TARGET
    let target = std::env::var("TARGET").expect("TARGET not set");
    println!("cargo:rustc-env=HOST_TARGET={}", target);

    // Capture PANIC strategy
    let panic = std::env::var("CARGO_CFG_PANIC").unwrap_or_else(|_| "unwind".to_string());
    println!("cargo:rustc-env=HOST_PANIC={}", panic);

    // Capture target features
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    println!("cargo:rustc-env=HOST_TARGET_FEATURES={}", features);

    // Export symbols so plugins can resolve them at load time.
    // macOS: -export_dynamic makes the executable's symbols visible to dlopen'd libs.
    // Linux: -rdynamic does the same for ELF.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-export_dynamic");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-rdynamic");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../patches.list");
}
