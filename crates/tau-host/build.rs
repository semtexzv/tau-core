use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/tls.c");

    // Compile C thread-local wrapper for fast executor index access.
    // This provides tls_get_executor, tls_set_executor, tls_is_current_executor
    // which are used by the executor for O(1) thread-local executor lookup.
    cc::Build::new()
        .file("src/tls.c")
        .compile("tls");

    // Capture build environment for same-compiler verification
    capture_rustc_version();
    capture_rustflags();
    capture_target();
    capture_panic_strategy();
    capture_target_features();
}

fn capture_rustc_version() {
    let output = Command::new("rustc")
        .arg("-V")
        .output()
        .expect("Failed to run rustc -V");
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("cargo:rustc-env=HOST_RUSTC_VERSION={}", version);
}

fn capture_rustflags() {
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .or_else(|_| std::env::var("RUSTFLAGS"))
        .unwrap_or_default();
    println!("cargo:rustc-env=HOST_RUSTFLAGS={}", flags);
}

fn capture_target() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| {
        // Fallback: query rustc for default target
        let output = Command::new("rustc")
            .args(["--version", "--verbose"])
            .output()
            .expect("Failed to run rustc");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find(|l| l.starts_with("host:"))
            .map(|l| l.trim_start_matches("host:").trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });
    println!("cargo:rustc-env=HOST_TARGET={}", target);
}

fn capture_panic_strategy() {
    // Default to "unwind" unless explicitly set to "abort"
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let panic = if profile == "release" {
        std::env::var("CARGO_PROFILE_RELEASE_PANIC").unwrap_or_else(|_| "unwind".to_string())
    } else {
        std::env::var("CARGO_PROFILE_DEV_PANIC").unwrap_or_else(|_| "unwind".to_string())
    };
    println!("cargo:rustc-env=HOST_PANIC={}", panic);
}

fn capture_target_features() {
    // Capture target CPU features for plugin compilation
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    // Format: comma-separated list → space-separated with + prefix
    let formatted = if features.is_empty() {
        String::new()
    } else {
        features
            .split(',')
            .map(|f| format!("+{}", f.trim()))
            .collect::<Vec<_>>()
            .join(",")
    };
    println!("cargo:rustc-env=HOST_TARGET_FEATURES={}", formatted);
}
