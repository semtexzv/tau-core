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
// patches.list parsing (shared with compiler.rs)
// =============================================================================

/// A patch entry from patches.list: crate name → source dir relative to workspace root.
struct PatchEntry {
    name: String,
    rel_path: String,
}

/// Parse the workspace's patches.list file.
fn parse_patches(root: &Path) -> Vec<PatchEntry> {
    let content = fs::read_to_string(root.join("patches.list"))
        .expect("patches.list not found in workspace root");
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (name, path) = line.split_once('=')?;
            Some(PatchEntry {
                name: name.trim().to_string(),
                rel_path: path.trim().to_string(),
            })
        })
        .collect()
}

// =============================================================================
// dist
// =============================================================================

fn dist(release: bool) {
    let root = workspace_root();
    let profile = if release { "release" } else { "debug" };
    let dist_dir = root.join("dist");
    let patches = parse_patches(&root);

    println!("=== Tau Core Distribution ({}) ===", profile);

    // Clean
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).expect("Failed to clean dist/");
    }
    fs::create_dir_all(dist_dir.join("bin")).unwrap();
    fs::create_dir_all(dist_dir.join("lib")).unwrap();

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
    cp(
        &target_dir.join("libtau_rt.dylib"),
        &dist_dir.join("lib/libtau_rt.dylib"),
    );

    // Fix RPATH so binary finds libs in ../lib
    run_silent(
        Command::new("install_name_tool")
            .args(["-add_rpath", "@executable_path/../lib"])
            .arg(dist_dir.join("bin/tau")),
    );

    // Rewrite libtau_rt.dylib reference to use @rpath
    let tau_rt_ref = otool_find_ref(&dist_dir.join("bin/tau"), "libtau_rt.dylib");
    if let Some(old_ref) = tau_rt_ref {
        println!("  Rewriting: {} → @rpath/libtau_rt.dylib", old_ref);
        run_silent(
            Command::new("install_name_tool")
                .args(["-change", &old_ref, "@rpath/libtau_rt.dylib"])
                .arg(dist_dir.join("bin/tau")),
        );
    }
    run_silent(
        Command::new("install_name_tool")
            .args(["-id", "@rpath/libtau_rt.dylib"])
            .arg(dist_dir.join("lib/libtau_rt.dylib")),
    );

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

    // Crates provided as prebuilt dylibs (via --extern, NOT built from source).
    // Their path deps must be stripped from dist Cargo.tomls to avoid E0464.
    let prebuilt_names: Vec<&str> = vec!["tau-rt"];

    // 4. Copy source crates (driven by patches.list)
    println!("[4/5] Copying source crates...");
    for patch in &patches {
        let src_dir = root.join(&patch.rel_path);
        let dst_dir = dist_dir.join("src").join(&patch.name);
        fs::create_dir_all(&dst_dir).unwrap();

        // Copy src/
        if src_dir.join("src").exists() {
            copy_dir(&src_dir.join("src"), &dst_dir.join("src"));
        }
        // Copy Cargo.toml
        if src_dir.join("Cargo.toml").exists() {
            cp(&src_dir.join("Cargo.toml"), &dst_dir.join("Cargo.toml"));
        }
        // Copy build.rs if present
        if src_dir.join("build.rs").exists() {
            cp(&src_dir.join("build.rs"), &dst_dir.join("build.rs"));
        }

        println!("  {} → src/{}/", patch.rel_path, patch.name);
    }

    // Strip path deps that point to prebuilt dylibs.
    //
    // WHY: The host compiler passes `--extern=tau_rt=/path/to/libtau_rt.dylib`
    // so plugins link against the ONE shared copy. If tau's Cargo.toml also has
    // `tau-rt = { path = "../tau-rt" }`, cargo builds tau-rt from source too,
    // producing TWO dylib candidates → E0464 "multiple candidates for dylib".
    //
    // By stripping the path dep, cargo resolves tau_rt solely via --extern.
    // Same logic applies to any crate provided as a prebuilt dylib.
    for patch in &patches {
        let toml_path = dist_dir.join("src").join(&patch.name).join("Cargo.toml");
        if toml_path.exists() {
            strip_prebuilt_deps(&toml_path, &prebuilt_names);
        }
    }

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

/// Strip path dependencies on prebuilt dylib crates from a Cargo.toml.
///
/// When the host compiler provides a crate via `--extern=foo=/path/to/libfoo.dylib`,
/// any `foo = { path = "..." }` dep in the Cargo.toml would cause cargo to also
/// build it from source, resulting in two dylib candidates (E0464).
///
/// This function removes lines like `tau-rt = { path = "../tau-rt" }` for crates
/// in the prebuilt set, so --extern is the sole provider.
fn strip_prebuilt_deps(toml_path: &Path, prebuilt: &[&str]) {
    let content = fs::read_to_string(toml_path).unwrap();
    let filtered: String = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Check if this line is a dep on a prebuilt crate
            for name in prebuilt {
                if trimmed.starts_with(name) && trimmed.contains("path =") {
                    return false;
                }
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(toml_path, filtered).unwrap();
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
- `lib/libtau_rt.dylib` — Shared runtime library (ONE copy, linked by all plugins)
- `lib/libstd-*.dylib` — Bundled Rust standard library
- `src/` — Source crates for plugin compilation (driven by `patches.list`)
  - `src/tau/` — Plugin SDK (rlib, compiled into each plugin)
  - `src/tau-rt/` — Runtime interface (dylib, shared)
  - `src/tokio/` — Tokio compatibility shim

## Architecture

```
Plugin A (cdylib)          Plugin B (cdylib)
  ┌─────────────┐           ┌─────────────┐
  │ tau (rlib)   │           │ tau (rlib)   │
  │ own statics  │           │ own statics  │
  └──────┬──────┘           └──────┬──────┘
         │  links to dylib         │
         ▼                         ▼
  ┌────────────────────────────────────┐
  │    libtau_rt.dylib (shared)        │
  │  spawn, sleep, resource, event, io │
  └────────────────────────────────────┘
```

## Usage
```bash
./run.sh --info                    # Show system info
./run.sh --plugin <plugin.dylib>   # Run a precompiled plugin
./run.sh --plugin <plugin_crate/>  # Compile and run a plugin crate
```
"#;
