//! Dynamic plugin compiler
//!
//! Handles detection of Rust toolchain and on-the-fly plugin compilation.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Compiler configuration detected from environment
pub struct Compiler {
    /// Path to rustc binary
    pub rustc: PathBuf,
    /// Path to cargo
    pub cargo: PathBuf,
    /// Rust sysroot
    pub sysroot: PathBuf,
    /// Path to std dylib (cached lookup)
    pub std_dylib: PathBuf,
    /// Target triple
    pub target: String,
    /// Full version string
    pub version_string: String,
}

impl Compiler {
    /// Captured build-time environment variables
    pub const HOST_VERSION: &'static str = env!("HOST_RUSTC_VERSION");
    pub const HOST_RUSTFLAGS: &'static str = env!("HOST_RUSTFLAGS");
    pub const HOST_TARGET: &'static str = env!("HOST_TARGET");
    pub const HOST_PANIC: &'static str = env!("HOST_PANIC");
    pub const HOST_TARGET_FEATURES: &'static str = env!("HOST_TARGET_FEATURES");
    // Removed HOST_SYSROOT and HOST_RUSTC as they are local paths

    /// Resolve a compatible Rust compiler at runtime
    pub fn resolve() -> Result<Self, String> {
        // 1. Try to find a compiler that matches HOST_VERSION
        let rustc = Self::find_compatible_compiler()?;

        // 2. Query sysroot from that compiler
        let output = Command::new(&rustc)
            .arg("--print")
            .arg("sysroot")
            .output()
            .map_err(|e| format!("Failed to run rustc --print sysroot: {}", e))?;

        let sysroot = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

        // 3. Find std dylib (using cached lookup logic)
        // We know target from build time, assuming we found a compiler for the same target.
        // In theory we should also verify target, but version match usually implies it for simplicity on same host.
        let target = Self::HOST_TARGET.to_string();
        let std_search_dir = sysroot.join("lib/rustlib").join(&target).join("lib");
        let std_dylib = Self::find_std_dylib(&std_search_dir)?;

        // 4. Find cargo (assume next to rustc, or in PATH)
        // If rustc is `rustup run ... rustc`, we can't just take parent.
        // We really want `cargo` from the same toolchain.
        // If we found `rustc` in PATH, `cargo` is likely there too.
        // If we found it via `rustup`, we should use `rustup run ... cargo` or just `cargo +toolchain`.
        // Simplify: Just use "cargo" and hope it's in PATH and compatible,
        // OR better: derive from rustc path if it looks absolute.

        let cargo = if rustc.is_absolute() {
            rustc
                .parent()
                .map(|p| p.join("cargo"))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("cargo"))
        } else {
            // It's a command like "rustc" or "rustup", so rely on PATH for cargo?
            // If we used rustup wrapper logic, we might need to be smarter.
            // For now, let's assume `cargo` in PATH matches `rustc` in PATH unless we do explicit rustup handling.
            PathBuf::from("cargo")
        };

        Ok(Self {
            rustc,
            cargo,
            sysroot,
            std_dylib,
            target,
            version_string: Self::HOST_VERSION.to_string(),
        })
    }

    fn find_compatible_compiler() -> Result<PathBuf, String> {
        // A. Try `rustc` in PATH first (fast path)
        if let Ok(path) = Self::check_compiler("rustc", Self::HOST_VERSION) {
            return Ok(path);
        }

        // B. Try to resolve specific toolchain via rustup
        let toolchain = Self::calculate_required_toolchain(Self::HOST_VERSION)?;
        println!(
            "[Compiler] Host built with {}, resolving toolchain '{}'...",
            Self::HOST_VERSION,
            toolchain
        );

        // Check if toolchain is installed
        // We use `rustup run <toolchain> rustc --version` to check existence and correctness
        let check_cmd = format!("rustup run {} rustc", toolchain);
        // Split for Command
        let parts: Vec<&str> = check_cmd.split_whitespace().collect();
        let cmd = parts[0];
        let args = &parts[1..];

        let output = Command::new(cmd)
            .args(args)
            .arg("-V")
            .output()
            .map_err(|e| e.to_string())?;

        if !output.status.success() {
            println!(
                "[Compiler] Toolchain '{}' not found or broken. Attempting install (minimal profile)...",
                toolchain
            );
            let status = Command::new("rustup")
                .args(["toolchain", "install", &toolchain, "--profile", "minimal"])
                .status()
                .map_err(|e| format!("Failed to run rustup install: {}", e))?;

            if !status.success() {
                return Err(format!("Failed to install toolchain '{}'", toolchain));
            }
        }

        // Now try to run it
        // We just return the wrapped command?
        // Wait, `Compiler` struct needs `rustc` as PathBuf.
        // `rustup run ...` is a command wrapper.
        // We need the direct path to the rustc executable proxy.
        // `rustup which rustc --toolchain <toolchain>` gives exactly that.

        let output = Command::new("rustup")
            .args(["which", "rustc", "--toolchain", &toolchain])
            .output()
            .map_err(|e| format!("Failed to resolve rustc path: {}", e))?;

        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            return Ok(path);
        }

        Err(format!(
            "Could not find compatible rustc for {}",
            Self::HOST_VERSION
        ))
    }

    /// Parse HOST_VERSION to determine the required toolchain name
    /// Strategies:
    /// - Stable: "rustc 1.85.0 (hash date)" -> "1.85.0"
    /// - Beta: "rustc 1.86.0-beta.2 (hash date)" -> "1.86.0-beta.2"
    /// - Nightly: "rustc 1.87.0-nightly (hash 2026-02-04)" -> "nightly-2026-02-04"
    fn calculate_required_toolchain(host_version: &str) -> Result<String, String> {
        let version_lower = host_version.to_lowercase();

        // Format: rustc <version_token> (<hash> <date>)
        let version_token = host_version
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| format!("Invalid version string: {}", host_version))?;

        if version_lower.contains("-nightly") {
            // For nightly, use date-based versioning to pinpoint the snapshot
            let date_part = host_version
                .split_whitespace()
                .last()
                .unwrap_or("")
                .trim_matches(')');
            let has_date = date_part.len() == 10 && date_part.contains('-');

            if has_date {
                return Ok(format!("nightly-{}", date_part));
            }
            // Fallback to generic nightly if date parsing fails, though unlikely
            return Ok("nightly".to_string());
        }

        // For Stable and Beta, the version string itself (e.g. "1.85.0" or "1.86.0-beta.2")
        // is the valid rustup toolchain name.
        Ok(version_token.to_string())
    }

    fn check_compiler(cmd: &str, expected_version: &str) -> Result<PathBuf, String> {
        let output = Command::new(cmd)
            .arg("-V") // Just version line is enough for initial check
            .output()
            .map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err("Failed execution".into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let detected = stdout.trim();

        // Exact string match check
        // "rustc 1.85.0 (hash date)" vs "rustc 1.85.0 (hash date)"
        if detected == expected_version {
            return Ok(PathBuf::from(cmd));
        }

        // Or maybe just version number match for stable?
        // But we want ABI safety. Hash/Date is important.

        Err("Version mismatch".into())
    }

    fn find_std_dylib(search_dir: &Path) -> Result<PathBuf, String> {
        if !search_dir.exists() {
            return Err(format!("Std lib directory not found: {:?}", search_dir));
        }

        for entry in std::fs::read_dir(search_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("libstd-") && name_str.ends_with(".dylib") {
                return Ok(entry.path());
            }
        }

        Err("std dylib not found".into())
    }

    /// Compile a plugin from a Cargo crate directory
    pub fn compile_plugin(
        &self,
        source_dir: &Path,
        output_dir: &Path,
        _interface_path: &Path, // Not strictly needed if building an existing crate
        dist_lib_path: &Path,
    ) -> Result<PathBuf, String> {
        // Validate source is a crate
        // Canonicalize path to ensure we have absolute paths for target dir resolution
        let source_dir = source_dir
            .canonicalize()
            .map_err(|e| format!("Invalid source path: {}", e))?;

        if !source_dir.join("Cargo.toml").exists() {
            return Err(format!("Not a Cargo project: {:?}", source_dir));
        }

        // Set DYLD_LIBRARY_PATH for compilation
        // Prioritize dist libs (runtime), then toolchain libs (std)
        let toolchain_lib_path = self.std_dylib.parent().unwrap();
        let dyld_path = format!(
            "{}:{}",
            dist_lib_path.display(),
            toolchain_lib_path.display()
        );

        // Determine build profile (inherit from Host)
        let is_release = !cfg!(debug_assertions);
        let build_args = if is_release {
            vec!["build", "--release"]
        } else {
            vec!["build"]
        };
        let target_subdir = if is_release { "release" } else { "debug" };

        // Explicitly set target directory to `~/.tau/buildcache/<hash>`
        // This ensures:
        // 1. Easy cleanup (just delete ~/.tau/buildcache)
        // 2. No conflict between plugins
        // 3. Shared artifacts if multiple hosts run the same plugin

        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                // Fallback for non-unix or weird envs
                PathBuf::from(".")
            });
        let cache_root = home.join(".tau").join("buildcache");

        // Hash the absolute source path to get a unique stable ID
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        source_dir.hash(&mut hasher);
        let hash = hasher.finish();

        let forced_target_dir = cache_root.join(format!("{:x}", hash));

        // Ensure cache dir exists
        std::fs::create_dir_all(&forced_target_dir)
            .map_err(|e| format!("Failed to create cache dir: {}", e))?;

        // Check for vendored crates in dist/vendor/
        // If present, inject source replacement config
        let vendor_dir = dist_lib_path
            .parent()
            .unwrap_or(dist_lib_path)
            .join("vendor");
        let use_vendored = vendor_dir.exists() && vendor_dir.is_dir();

        // Check for prebuilt crates in dist/lib/prebuilt/
        // These are rlib/rmeta files that we inject via --extern to skip rebuilding
        let prebuilt_dir = dist_lib_path.join("prebuilt");
        let mut extern_flags: Vec<String> = Vec::new();
        if prebuilt_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&prebuilt_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    // Only process .rlib files (they have corresponding .rmeta)
                    if path.extension().map_or(false, |e| e == "rlib") {
                        if let Some(filename) = path.file_name().and_then(|s| s.to_str()) {
                            // Parse lib<crate>-<hash>.rlib -> crate name
                            if filename.starts_with("lib") && filename.contains('-') {
                                let name_part = &filename[3..]; // strip "lib"
                                if let Some(dash_pos) = name_part.rfind('-') {
                                    let crate_name = &name_part[..dash_pos];

                                    extern_flags.push(format!(
                                        "--extern={}={}",
                                        crate_name,
                                        path.display()
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Prepare cargo command
        let mut cmd = Command::new(&self.cargo);

        cmd.current_dir(&source_dir)
            .env("CARGO_TARGET_DIR", &forced_target_dir)
            .env("DYLD_LIBRARY_PATH", dyld_path)
            // Force use of our captured rustc to match ABI
            .env("RUSTC", &self.rustc);

        // Inject configuration for system lookup to help plugins find dependencies
        // located in the distribution directory.
        // - LIBRARY_PATH: For linkers to find static/dynamic libs (non-Rust)
        // - PKG_CONFIG_PATH: For pkg-config to find .pc files
        // - CPATH: For C headers

        let dist_root = dist_lib_path.parent().unwrap_or(dist_lib_path);
        let include_path = dist_root.join("include");
        let pkgconfig_path = dist_lib_path.join("pkgconfig");

        fn append_path(var: &str, path: &Path) -> String {
            if let Ok(existing) = std::env::var(var) {
                format!("{}:{}", path.display(), existing)
            } else {
                path.display().to_string()
            }
        }

        cmd.env("LIBRARY_PATH", append_path("LIBRARY_PATH", dist_lib_path))
            .env(
                "PKG_CONFIG_PATH",
                append_path("PKG_CONFIG_PATH", &pkgconfig_path),
            )
            .env("CPATH", append_path("CPATH", &include_path));

        // Construct CARGO_ENCODED_RUSTFLAGS
        let mut flags = Vec::new();

        // Inherit HOST_RUSTFLAGS (0x1f separated)
        if !Self::HOST_RUSTFLAGS.is_empty() {
            flags.extend(Self::HOST_RUSTFLAGS.split('\x1f').map(|s| s.to_string()));
        }

        // Add plugin compilation flags
        flags.push("-C".to_string());
        flags.push("prefer-dynamic".to_string());

        flags.push("-C".to_string());
        flags.push("link-args=-Wl,-undefined,dynamic_lookup".to_string());

        flags.push("-L".to_string());
        flags.push(dist_lib_path.display().to_string());

        flags.push("-C".to_string());
        flags.push(format!("panic={}", Self::HOST_PANIC));

        if !Self::HOST_TARGET_FEATURES.is_empty() {
            flags.push("-C".to_string());
            flags.push(format!("target-feature={}", Self::HOST_TARGET_FEATURES));
        }

        // Add --extern flags for prebuilt crates
        if !extern_flags.is_empty() {
            println!("[Compiler] Using prebuilt crates: {:?}", extern_flags);
            flags.extend(extern_flags);
        }

        let encoded_flags = flags.join("\x1f");

        cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded_flags)
            .env_remove("RUSTFLAGS") // Ensure we don't mix them
            .args(&build_args);

        // Inject vendored sources config if vendor directory exists
        if use_vendored {
            println!("[Compiler] Using vendored crates from: {:?}", vendor_dir);
            // source.crates-io.replace-with = "vendored-sources"
            cmd.arg("--config")
                .arg("source.crates-io.replace-with=\"vendored-sources\"");
            // source.vendored-sources.directory = "<vendor_dir>"
            cmd.arg("--config").arg(format!(
                "source.vendored-sources.directory=\"{}\"",
                vendor_dir.display()
            ));
        }

        println!(
            "[Compiler] Building plugin crate in {:?} ({})",
            source_dir, target_subdir
        );

        // Execute build
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run cargo: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!("Compilation failed:\n{}\n{}", stdout, stderr));
        }

        // Locate the output dylib
        // We look for lib*.dylib in target/<profile>/
        // Note: The crate name might differ from directory name, but typically it matches or we scan.
        // A robust way uses `cargo metadata`, but scanning is simpler for now.
        let target_dir = forced_target_dir.join(target_subdir);
        println!("[Compiler] Looking for artifacts in: {:?}", target_dir);

        // Parse Cargo.toml to find package name
        let manifest_path = source_dir.join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Failed to read Cargo.toml: {}", e))?;

        let package_name = manifest
            .lines()
            .find(|l| l.trim().starts_with("name"))
            .and_then(|l| l.split('=').nth(1))
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .ok_or("Could not determine package name from Cargo.toml")?;

        // Normalize package name to crate name (dash to underscore)
        let crate_name = package_name.replace('-', "_");
        let expected_dylib_name = format!("lib{}.dylib", crate_name);

        println!("[Compiler] Expecting artifact: {}", expected_dylib_name);

        let dylib_path = target_dir.join(&expected_dylib_name);

        if !dylib_path.exists() {
            return Err(format!(
                "Expected artifact {} not found in {:?}",
                expected_dylib_name, target_dir
            ));
        }

        // Copy to output directory
        let final_name = dylib_path.file_name().unwrap();
        let final_path = output_dir.join(final_name);
        std::fs::copy(&dylib_path, &final_path).map_err(|e| e.to_string())?;

        Ok(final_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_compiler() {
        let compiler = Compiler::resolve().expect("Should resolve compiler");
        println!("rustc: {:?}", compiler.rustc);
        println!("sysroot: {:?}", compiler.sysroot);
        println!("target: {}", compiler.target);
        println!("std dylib: {:?}", compiler.std_dylib);
    }
}
