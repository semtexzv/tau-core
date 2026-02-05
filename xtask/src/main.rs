//! Tau-core development tasks.
//!
//! Usage:
//!   cargo xtask dist [--release]
//!   cargo xtask test [--dist]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("dist") => {
            let release = args.iter().any(|a| a == "--release");
            dist(release);
        }
        Some("test") => {
            let dist_mode = args.iter().any(|a| a == "--dist");
            test(dist_mode);
        }
        _ => {
            eprintln!("Usage:");
            eprintln!("  cargo xtask dist [--release]   Build distribution");
            eprintln!("  cargo xtask test [--dist]      Run reload + safety tests");
            std::process::exit(1);
        }
    }
}

// =============================================================================
// dist
// =============================================================================

fn dist(release: bool) {
    let root = workspace_root();
    let profile = if release { "release" } else { "debug" };
    let dist_dir = root.join("dist");

    println!("=== Tau Core Distribution ({}) ===", profile);

    // Clean
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).expect("Failed to clean dist/");
    }
    for sub in ["bin", "lib", "src/tau", "src/tokio"] {
        fs::create_dir_all(dist_dir.join(sub)).expect("Failed to create dist subdirs");
    }

    // 1. Build
    println!("\n[1/5] Building...");
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&root).arg("build");
    if release {
        cmd.arg("--release");
    }
    run(&mut cmd);

    let target_dir = root.join("target").join(profile);

    // 2. Copy binaries
    println!("[2/5] Copying binaries...");
    cp(&target_dir.join("tau"), &dist_dir.join("bin/tau"));
    cp(&target_dir.join("libtau.dylib"), &dist_dir.join("lib/libtau.dylib"));

    // Fix RPATH so binary finds libs in ../lib
    run_silent(Command::new("install_name_tool")
        .args(["-add_rpath", "@executable_path/../lib"])
        .arg(dist_dir.join("bin/tau")));

    // Rewrite libtau.dylib reference to use @rpath
    let tau_ref = otool_find_ref(&dist_dir.join("bin/tau"), "libtau.dylib");
    if let Some(old_ref) = tau_ref {
        println!("  Rewriting: {} → @rpath/libtau.dylib", old_ref);
        run_silent(Command::new("install_name_tool")
            .args(["-change", &old_ref, "@rpath/libtau.dylib"])
            .arg(dist_dir.join("bin/tau")));
    }
    run_silent(Command::new("install_name_tool")
        .args(["-id", "@rpath/libtau.dylib"])
        .arg(dist_dir.join("lib/libtau.dylib")));

    // 3. Copy Rust standard library
    println!("[3/5] Copying Rust std...");
    let sysroot = rustc_sysroot();
    let target_triple = current_target();
    let std_lib_dir = sysroot
        .join("lib/rustlib")
        .join(&target_triple)
        .join("lib");

    let mut found_std = false;
    if std_lib_dir.exists() {
        for entry in fs::read_dir(&std_lib_dir).expect("read std lib dir") {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("libstd-") && name.ends_with(".dylib") {
                cp(&entry.path(), &dist_dir.join("lib").join(entry.file_name()));
                println!("  Copied {}", name);
                found_std = true;
            }
        }
    }
    if !found_std {
        eprintln!("  Warning: libstd dylib not found in {:?}", std_lib_dir);
    }

    // 4. Copy source crates
    println!("[4/5] Copying source crates...");
    let crates_dir = root.join("crates");

    // tau
    copy_dir(&crates_dir.join("tau/src"), &dist_dir.join("src/tau/src"));
    cp(&crates_dir.join("tau/Cargo.toml"), &dist_dir.join("src/tau/Cargo.toml"));
    cp(&crates_dir.join("tau/build.rs"), &dist_dir.join("src/tau/build.rs"));

    // tokio shim
    copy_dir(&crates_dir.join("tau-tokio/src"), &dist_dir.join("src/tokio/src"));
    cp(&crates_dir.join("tau-tokio/Cargo.toml"), &dist_dir.join("src/tokio/Cargo.toml"));
    cp(&crates_dir.join("tau-tokio/build.rs"), &dist_dir.join("src/tokio/build.rs"));

    // Strip `tau = { path = ... }` from dist tokio Cargo.toml (provided via --extern)
    let tokio_toml_path = dist_dir.join("src/tokio/Cargo.toml");
    let content = fs::read_to_string(&tokio_toml_path).unwrap();
    let filtered: String = content
        .lines()
        .filter(|line| !line.trim_start().starts_with("tau = "))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&tokio_toml_path, filtered).unwrap();

    println!("  Source crates copied to src/");

    // 5. Create scripts
    println!("[5/5] Creating scripts...");
    write_executable(
        &dist_dir.join("run.sh"),
        r#"#!/bin/bash
# Tau Runner — sets up library paths and runs the host
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export DYLD_LIBRARY_PATH="${SCRIPT_DIR}/lib:${DYLD_LIBRARY_PATH}"
exec "${SCRIPT_DIR}/bin/tau" "$@"
"#,
    );
    write_executable(
        &dist_dir.join("info.sh"),
        r#"#!/bin/bash
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${SCRIPT_DIR}/run.sh" --info
"#,
    );

    fs::write(dist_dir.join("README.md"), DIST_README).unwrap();

    // Summary
    println!("\n=== Distribution Ready ===");
    println!("Location: {}", dist_dir.display());
    print_dir_sizes(&dist_dir);
}

// =============================================================================
// test
// =============================================================================

fn test(dist_mode: bool) {
    let root = workspace_root();

    if !dist_mode {
        println!("=== Building host ===");
        run(Command::new("cargo").current_dir(&root).arg("build"));
    }

    let test_dir = root.join("tests");
    let scripts: &[&str] = if dist_mode {
        &["test-reload-dist.sh", "test-safety.sh"]
    } else {
        &["test-reload.sh", "test-safety.sh"]
    };

    let mut all_pass = true;
    for script in scripts {
        let path = test_dir.join(script);
        if !path.exists() {
            eprintln!("Skipping {} (not found)", script);
            continue;
        }
        println!("\n=== Running {} ===", script);
        let status = Command::new("bash")
            .arg(&path)
            .current_dir(&root)
            .status()
            .expect("Failed to run test script");
        if !status.success() {
            eprintln!("FAILED: {}", script);
            all_pass = false;
        }
    }

    if !all_pass {
        std::process::exit(1);
    }
    println!("\nAll tests passed!");
}

// =============================================================================
// Helpers
// =============================================================================

fn workspace_root() -> PathBuf {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .stderr(Stdio::null())
        .output()
        .expect("cargo metadata failed");

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Quick parse — find "workspace_root":"..."
    if let Some(pos) = stdout.find("\"workspace_root\":\"") {
        let start = pos + "\"workspace_root\":\"".len();
        let end = stdout[start..].find('"').unwrap() + start;
        return PathBuf::from(&stdout[start..end]);
    }
    // Fallback: walk up from current dir
    let mut dir = env::current_dir().unwrap();
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates").exists() {
            return dir;
        }
        if !dir.pop() {
            panic!("Could not find workspace root");
        }
    }
}

fn rustc_sysroot() -> PathBuf {
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc --print sysroot failed");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn current_target() -> String {
    let output = Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("rustc -vV failed");
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.strip_prefix("host:").unwrap().trim().to_string())
        .expect("Could not determine host target")
}

fn run(cmd: &mut Command) {
    let status = cmd.status().expect("Failed to execute command");
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn run_silent(cmd: &mut Command) {
    let _ = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn cp(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap_or_else(|e| panic!("cp {:?} → {:?}: {}", src, dst, e));
}

fn copy_dir(src: &Path, dst: &Path) {
    if dst.exists() {
        fs::remove_dir_all(dst).unwrap();
    }
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn otool_find_ref(binary: &Path, lib_name: &str) -> Option<String> {
    let output = Command::new("otool")
        .args(["-L"])
        .arg(binary)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines()
        .find(|l| l.contains(lib_name))
        .map(|l| l.trim().split_whitespace().next().unwrap().to_string())
}

fn print_dir_sizes(dir: &Path) {
    println!();
    for sub in ["bin", "lib", "src"] {
        let sub_dir = dir.join(sub);
        if sub_dir.exists() {
            print!("{}/: ", sub);
            for entry in fs::read_dir(&sub_dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_file() {
                    let size_kb = meta.len() / 1024;
                    print!("{} ({}K)  ", entry.file_name().to_string_lossy(), size_kb);
                } else if meta.is_dir() {
                    print!("{}/  ", entry.file_name().to_string_lossy());
                }
            }
            println!();
        }
    }

    let total = dir_size(dir);
    println!("\nTotal: {}M", total / (1024 * 1024));
}

fn dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries {
            let entry = entry.unwrap();
            size += dir_size(&entry.path());
        }
    }
    size
}

const DIST_README: &str = r#"# Tau Core Runtime

Self-contained async plugin system with tokio-compatible API.

## Components
- `bin/tau` — Host executable (exports runtime symbols)
- `lib/libtau.dylib` — Shared interface library
- `lib/libstd-*.dylib` — Bundled Rust standard library
- `src/` — Source crates for plugin compilation
  - `src/tau/` — Tau runtime interface crate
  - `src/tokio/` — Tokio compatibility shim

## Usage
```bash
./run.sh --info                    # Show system info
./run.sh --plugin <plugin.dylib>   # Run a precompiled plugin
./run.sh --plugin <plugin_crate/>  # Compile and run a plugin crate
```
"#;
