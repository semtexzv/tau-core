//! Dynamic plugin compiler
//!
//! Handles detection of Rust toolchain and on-the-fly plugin compilation.
//!
//! # Same-compiler invariant
//!
//! This module enforces a critical invariant: **all plugins are compiled with the
//! exact same `rustc` version and flags as the host binary.** This is verified at
//! startup by comparing `HOST_RUSTC_VERSION` (captured at host build time) against
//! the detected `rustc` on the system.
//!
//! This invariant guarantees that `repr(Rust)` type layouts are identical across
//! the host and all plugins, which allows concrete data (String, Vec, structs, etc.)
//! to be safely sent across plugin boundaries. See the [`tau`] crate-level docs
//! for the full rules on what can and cannot cross the plugin boundary.
//!
//! # Patch list
//!
//! The set of crates patched into plugin builds is defined in `patches.list`
//! at the workspace root. Both this module (via `include_str!`) and the xtask
//! dist command read the same file, keeping them in sync.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Raw content of `patches.list` — baked in at compile time.
const PATCHES_LIST: &str = include_str!("../../../patches.list");

/// A single patch entry: crate name → source directory (relative to workspace root).
#[derive(Debug, Clone)]
pub struct PatchEntry {
    /// Crate name as it appears in `[patch.crates-io]` (e.g. "tau", "tau-rt", "tokio")
    pub name: String,
    /// Source directory relative to workspace root (e.g. "crates/tau")
    pub rel_path: String,
}

/// Parse `patches.list` into a list of entries.
pub fn parse_patches() -> Vec<PatchEntry> {
    PATCHES_LIST
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

/// Resolved patch paths (absolute) for a given root directory.
pub struct ResolvedPatches {
    pub entries: Vec<(String, PathBuf)>,
}

impl ResolvedPatches {
    /// Resolve all patch entries relative to `root`.
    pub fn resolve(root: &Path) -> Self {
        let entries = parse_patches()
            .into_iter()
            .map(|e| (e.name, root.join(&e.rel_path)))
            .collect();
        Self { entries }
    }

    /// Look up a resolved path by patch name.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p.as_path())
    }
}

/// Compiler configuration detected from environment
pub struct Compiler {
    pub rustc: PathBuf,
    pub cargo: PathBuf,
    pub sysroot: PathBuf,
    pub std_dylib: PathBuf,
    #[allow(dead_code)]
    pub target: String,
    pub version_string: String,
    /// Cached environment hash (compiler + flags + tau-rt + patches).
    /// Computed on first use via `get_or_compute_env_hash()`.
    env_hash: std::cell::OnceCell<String>,
}

impl Compiler {
    // Build-time captured environment
    pub const HOST_VERSION: &'static str = env!("HOST_RUSTC_VERSION");
    pub const HOST_RUSTFLAGS: &'static str = env!("HOST_RUSTFLAGS");
    pub const HOST_TARGET: &'static str = env!("HOST_TARGET");
    pub const HOST_PANIC: &'static str = env!("HOST_PANIC");
    pub const HOST_TARGET_FEATURES: &'static str = env!("HOST_TARGET_FEATURES");

    /// Resolve a compatible Rust compiler at runtime.
    pub fn resolve() -> Result<Self, String> {
        let rustc = Self::find_compatible_compiler()?;

        let sysroot = Self::query_sysroot(&rustc)?;
        let target = Self::HOST_TARGET.to_string();
        let std_search_dir = sysroot.join("lib/rustlib").join(&target).join("lib");
        let std_dylib = Self::find_std_dylib(&std_search_dir)?;

        let cargo = Self::find_cargo(&rustc);

        Ok(Self {
            rustc,
            cargo,
            sysroot,
            std_dylib,
            target,
            version_string: Self::HOST_VERSION.to_string(),
            env_hash: std::cell::OnceCell::new(),
        })
    }

    /// Compile a plugin from a Cargo crate directory.
    ///
    /// `patches` — resolved source paths for `[patch.crates-io]`.
    /// `dist_lib_path` — directory containing `libtau_rt.dylib` and `libstd-*.dylib`.
    pub fn compile_plugin(
        &self,
        source_dir: &Path,
        patches: &ResolvedPatches,
        dist_lib_path: &Path,
    ) -> Result<PathBuf, String> {
        let source_dir = source_dir
            .canonicalize()
            .map_err(|e| format!("Invalid source path: {}", e))?;

        if !source_dir.join("Cargo.toml").exists() {
            return Err(format!("Not a Cargo project: {:?}", source_dir));
        }

        let cache_dir = self.get_build_cache_dir(&source_dir, dist_lib_path)?;

        // Fast path: skip cargo if all sources are unchanged
        if let Some(artifact) = self.check_up_to_date(&cache_dir, &source_dir) {
            println!("[Compiler] Plugin up-to-date (skipped): {:?}", artifact);
            return Ok(artifact);
        }

        let target_dir = cache_dir.join("target");
        let is_release = !cfg!(debug_assertions);
        let profile = if is_release { "release" } else { "debug" };

        // Detect prebuilt dylibs we can pass via --extern
        let prebuilt = self.detect_prebuilt_crates(dist_lib_path);

        // Build rustflags
        let rustflags = self.build_rustflags(dist_lib_path, &prebuilt);

        // Prepare cargo command
        let mut cmd = Command::new(&self.cargo);
        cmd.current_dir(&source_dir)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env("DYLD_LIBRARY_PATH", self.build_dyld_path(dist_lib_path))
            .env("RUSTC", &self.rustc)
            .env("CARGO_ENCODED_RUSTFLAGS", rustflags)
            .env_remove("RUSTFLAGS");

        // Add system library paths for native dependencies
        self.inject_system_paths(&mut cmd, dist_lib_path);

        // Add build arguments
        if is_release {
            cmd.args(["build", "--release"]);
        } else {
            cmd.arg("build");
        }

        // Inject crate patches from patches.list
        self.inject_patches(&mut cmd, patches, &prebuilt);

        println!(
            "[Compiler] Building plugin in {:?} ({})",
            source_dir, profile
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

        // Find the artifact and copy to plugins/ subdirectory
        self.find_artifact(
            &source_dir,
            &target_dir.join(profile),
            &cache_dir.join("plugins"),
        )
    }

    /// Check if a previously-built plugin is still up-to-date by parsing the
    /// cargo dep file and checking mtimes. Returns `Some(artifact_path)` if
    /// all source deps are older than the artifact; `None` if a rebuild is needed.
    fn check_up_to_date(
        &self,
        cache_dir: &Path,
        source_dir: &Path,
    ) -> Option<PathBuf> {
        let crate_name = Self::parse_crate_name(source_dir).ok()?;
        let lib_name = format!("lib{}.dylib", crate_name.replace('-', "_"));

        // 1. Find the final artifact
        let artifact = cache_dir.join("plugins").join(&lib_name);
        if !artifact.exists() {
            return None;
        }

        // 2. Find the dep file
        let is_release = !cfg!(debug_assertions);
        let profile = if is_release { "release" } else { "debug" };
        let dep_file = cache_dir.join("target").join(profile).join(
            format!("lib{}.d", crate_name.replace('-', "_")),
        );
        if !dep_file.exists() {
            return None;
        }

        // 3. Get artifact mtime
        let artifact_mtime = std::fs::metadata(&artifact).ok()?.modified().ok()?;

        // 4. Check Cargo.toml mtime (catches dependency changes not in dep file)
        let cargo_toml = source_dir.join("Cargo.toml");
        if let Ok(meta) = std::fs::metadata(&cargo_toml) {
            if let Ok(mtime) = meta.modified() {
                if mtime > artifact_mtime {
                    return None;
                }
            }
        }

        // 5. Parse dep file and check all dependency mtimes
        let dep_contents = std::fs::read_to_string(&dep_file).ok()?;
        let deps = Self::parse_dep_file(&dep_contents)?;

        for dep_path in deps {
            match std::fs::metadata(&dep_path) {
                Ok(meta) => {
                    if let Ok(mtime) = meta.modified() {
                        if mtime > artifact_mtime {
                            return None; // Source newer than artifact
                        }
                    }
                }
                Err(_) => return None, // File missing → rebuild
            }
        }

        Some(artifact)
    }

    /// Parse a Makefile-format dep file. Returns the list of dependency paths
    /// (everything after the first `:`).
    ///
    /// Format: `<output>: <dep1> <dep2> ...`
    /// Paths may contain escaped spaces (`\ `), and lines may be continued with `\`.
    fn parse_dep_file(contents: &str) -> Option<Vec<PathBuf>> {
        // Join continuation lines (trailing backslash)
        let joined = contents.replace("\\\n", " ");

        // Find the colon separator (first `:` not preceded by a drive letter on Windows)
        let colon_pos = joined.find(": ")?;
        let deps_str = &joined[colon_pos + 2..];

        // Split on whitespace, but handle escaped spaces
        let mut deps = Vec::new();
        let mut current = String::new();

        let chars: Vec<char> = deps_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == ' ' {
                // Escaped space — part of the path
                current.push(' ');
                i += 2;
            } else if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\n' {
                // Line continuation — skip
                i += 2;
            } else if chars[i].is_whitespace() {
                if !current.is_empty() {
                    deps.push(PathBuf::from(&current));
                    current.clear();
                }
                i += 1;
            } else {
                current.push(chars[i]);
                i += 1;
            }
        }
        if !current.is_empty() {
            deps.push(PathBuf::from(&current));
        }

        Some(deps)
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    fn find_compatible_compiler() -> Result<PathBuf, String> {
        // Try rustc in PATH first
        if let Ok(path) = Self::check_compiler_version("rustc", Self::HOST_VERSION) {
            return Ok(path);
        }

        // Try rustup toolchain
        let toolchain = Self::parse_toolchain_from_version(Self::HOST_VERSION)?;
        println!("[Compiler] Resolving toolchain '{}'...", toolchain);

        // Check if installed, install if not
        if !Self::is_toolchain_installed(&toolchain) {
            println!("[Compiler] Installing toolchain '{}'...", toolchain);
            let status = Command::new("rustup")
                .args(["toolchain", "install", &toolchain, "--profile", "minimal"])
                .status()
                .map_err(|e| format!("Failed to run rustup: {}", e))?;
            if !status.success() {
                return Err(format!("Failed to install toolchain '{}'", toolchain));
            }
        }

        // Get the actual rustc path
        let output = Command::new("rustup")
            .args(["which", "rustc", "--toolchain", &toolchain])
            .output()
            .map_err(|e| format!("Failed to resolve rustc: {}", e))?;

        if output.status.success() {
            Ok(PathBuf::from(
                String::from_utf8_lossy(&output.stdout).trim(),
            ))
        } else {
            Err(format!("Could not find rustc for {}", Self::HOST_VERSION))
        }
    }

    fn parse_toolchain_from_version(version: &str) -> Result<String, String> {
        let version_token = version
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| format!("Invalid version: {}", version))?;

        if version.to_lowercase().contains("-nightly") {
            // Extract date for nightly: "rustc X.Y.Z-nightly (hash YYYY-MM-DD)"
            if let Some(date) = version.split_whitespace().last() {
                let date = date.trim_matches(')');
                if date.len() == 10 && date.contains('-') {
                    return Ok(format!("nightly-{}", date));
                }
            }
            return Ok("nightly".to_string());
        }

        Ok(version_token.to_string())
    }

    fn is_toolchain_installed(toolchain: &str) -> bool {
        Command::new("rustup")
            .args(["run", toolchain, "rustc", "-V"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn check_compiler_version(cmd: &str, expected: &str) -> Result<PathBuf, String> {
        let output = Command::new(cmd)
            .arg("-V")
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected {
            Ok(PathBuf::from(cmd))
        } else {
            Err("Version mismatch".into())
        }
    }

    fn query_sysroot(rustc: &Path) -> Result<PathBuf, String> {
        let output = Command::new(rustc)
            .args(["--print", "sysroot"])
            .output()
            .map_err(|e| format!("Failed to query sysroot: {}", e))?;
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    }

    fn find_cargo(rustc: &Path) -> PathBuf {
        if rustc.is_absolute() {
            if let Some(cargo) = rustc
                .parent()
                .map(|p| p.join("cargo"))
                .filter(|p| p.exists())
            {
                return cargo;
            }
        }
        PathBuf::from("cargo")
    }

    fn find_std_dylib(search_dir: &Path) -> Result<PathBuf, String> {
        if !search_dir.exists() {
            return Err(format!("Std lib dir not found: {:?}", search_dir));
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

    /// Compute the environment hash once and cache it. Includes compiler version,
    /// flags, tau-rt content, and patches.list — everything that's shared across
    /// all plugins. When any of these change, all plugins get fresh build dirs.
    fn get_or_compute_env_hash(&self, dist_lib_path: &Path) -> &str {
        self.env_hash.get_or_init(|| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            let mut hasher = DefaultHasher::new();

            // Compiler identity
            Self::HOST_VERSION.hash(&mut hasher);
            Self::HOST_RUSTFLAGS.hash(&mut hasher);
            Self::HOST_PANIC.hash(&mut hasher);
            Self::HOST_TARGET_FEATURES.hash(&mut hasher);

            // tau-rt dylib content (ABI fingerprint)
            let tau_rt_dylib = dist_lib_path.join("libtau_rt.dylib");
            if tau_rt_dylib.exists() {
                if let Ok(bytes) = std::fs::read(&tau_rt_dylib) {
                    bytes.hash(&mut hasher);
                }
            }

            // Patch list (which crates are patched and their paths)
            PATCHES_LIST.hash(&mut hasher);

            format!("{:x}", hasher.finish())
        })
    }

    /// Parse the package name from a plugin's Cargo.toml.
    fn parse_crate_name(source_dir: &Path) -> Result<String, String> {
        let manifest = std::fs::read_to_string(source_dir.join("Cargo.toml"))
            .map_err(|e| format!("Failed to read Cargo.toml: {}", e))?;

        manifest
            .lines()
            .find(|l| l.trim().starts_with("name"))
            .and_then(|l| l.split('=').nth(1))
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .ok_or_else(|| format!("Could not determine package name from {:?}", source_dir))
    }

    /// Build cache directory: `~/.tau/buildcache/<env_hash>/<crate_name>.<src_hash>/`
    ///
    /// The two-level hierarchy separates the environment (compiler + flags + tau-rt)
    /// from individual plugin identity (crate name + source path).
    fn get_build_cache_dir(
        &self,
        source_dir: &Path,
        dist_lib_path: &Path,
    ) -> Result<PathBuf, String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let home =
            std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        let cache_root = home.join(".tau").join("buildcache");

        // Level 1: environment hash (shared across all plugins)
        let env_hash = self.get_or_compute_env_hash(dist_lib_path);

        // Level 2: crate name + short hash of source path (per-plugin identity)
        let crate_name = Self::parse_crate_name(source_dir)?;
        let mut hasher = DefaultHasher::new();
        source_dir.hash(&mut hasher);
        let src_hash = format!("{:x}", hasher.finish());
        let short_hash = &src_hash[..8.min(src_hash.len())];

        let cache_dir = cache_root
            .join(env_hash)
            .join(format!("{}.{}", crate_name, short_hash));

        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create cache dir: {}", e))?;

        Ok(cache_dir)
    }

    fn detect_prebuilt_crates(&self, dist_lib_path: &Path) -> PrebuiltCrates {
        let mut crates = PrebuiltCrates::default();

        // Check for libtau_rt.dylib — the shared runtime dylib
        let tau_rt_dylib = dist_lib_path.join("libtau_rt.dylib");
        if tau_rt_dylib.exists() {
            crates.tau_rt = Some(tau_rt_dylib);
        }

        crates
    }

    fn build_rustflags(&self, dist_lib_path: &Path, prebuilt: &PrebuiltCrates) -> String {
        let mut flags = Vec::new();

        // Inherit host rustflags
        if !Self::HOST_RUSTFLAGS.is_empty() {
            flags.extend(Self::HOST_RUSTFLAGS.split('\x1f').map(String::from));
        }

        // Dynamic linking flags
        flags.extend([
            "-C".into(),
            "prefer-dynamic".into(),
            "-C".into(),
            "link-args=-Wl,-undefined,dynamic_lookup".into(),
            "-L".into(),
            dist_lib_path.display().to_string(),
            "-C".into(),
            format!("panic={}", Self::HOST_PANIC),
        ]);

        // Target features (if any)
        if !Self::HOST_TARGET_FEATURES.is_empty() {
            flags.extend([
                "-C".into(),
                format!("target-feature={}", Self::HOST_TARGET_FEATURES),
            ]);
        }

        // Add --extern for prebuilt dylibs
        if let Some(tau_rt_path) = &prebuilt.tau_rt {
            flags.extend([
                "-L".into(),
                tau_rt_path.parent().unwrap().display().to_string(),
                format!("--extern=tau_rt={}", tau_rt_path.display()),
            ]);
            println!("[Compiler] Using prebuilt tau-rt: {:?}", tau_rt_path);
        }

        flags.join("\x1f")
    }

    fn build_dyld_path(&self, dist_lib_path: &Path) -> String {
        let toolchain_lib = self.std_dylib.parent().unwrap();
        format!("{}:{}", dist_lib_path.display(), toolchain_lib.display())
    }

    fn inject_system_paths(&self, cmd: &mut Command, dist_lib_path: &Path) {
        let dist_root = dist_lib_path.parent().unwrap_or(dist_lib_path);

        fn append_path(var: &str, path: &Path) -> String {
            std::env::var(var)
                .map(|existing| format!("{}:{}", path.display(), existing))
                .unwrap_or_else(|_| path.display().to_string())
        }

        cmd.env("LIBRARY_PATH", append_path("LIBRARY_PATH", dist_lib_path))
            .env(
                "PKG_CONFIG_PATH",
                append_path("PKG_CONFIG_PATH", &dist_lib_path.join("pkgconfig")),
            )
            .env(
                "CPATH",
                append_path("CPATH", &dist_root.join("include")),
            );
    }

    /// Inject `--config patch.crates-io.<name>.path="<abs>"` for every entry
    /// in `patches.list`. If a prebuilt dylib exists for a crate (currently
    /// only `tau-rt`), that crate is skipped — cargo will pick it up via the
    /// `--extern` flag in rustflags instead.
    fn inject_patches(
        &self,
        cmd: &mut Command,
        patches: &ResolvedPatches,
        prebuilt: &PrebuiltCrates,
    ) {
        for (name, path) in &patches.entries {
            // Skip tau-rt if we have a prebuilt dylib
            if name == "tau-rt" && prebuilt.tau_rt.is_some() {
                continue;
            }
            cmd.arg("--config")
                .arg(format!("patch.crates-io.{}.path=\"{}\"", name, path.display()));
            println!("[Compiler] Patching {} → {:?}", name, path);
        }
    }

    fn find_artifact(
        &self,
        source_dir: &Path,
        build_dir: &Path,
        plugins_dir: &Path,
    ) -> Result<PathBuf, String> {
        let package_name = Self::parse_crate_name(source_dir)?;
        let crate_name = package_name.replace('-', "_");
        let dylib_name = format!("lib{}.dylib", crate_name);
        let built_path = build_dir.join(&dylib_name);

        if !built_path.exists() {
            return Err(format!(
                "Artifact {} not found in {:?}",
                dylib_name, build_dir
            ));
        }

        // Copy to plugins/ subdirectory for clean organization
        std::fs::create_dir_all(plugins_dir)
            .map_err(|e| format!("Failed to create plugins dir: {}", e))?;

        let final_path = plugins_dir.join(&dylib_name);
        std::fs::copy(&built_path, &final_path)
            .map_err(|e| format!("Failed to copy artifact: {}", e))?;

        println!("[Compiler] Plugin ready: {:?}", final_path);
        Ok(final_path)
    }
}

/// Tracks which crates have prebuilt dylibs available
#[derive(Default)]
struct PrebuiltCrates {
    /// Pre-built `libtau_rt.dylib` — the shared runtime
    tau_rt: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_compiler() {
        let compiler = Compiler::resolve().expect("Should resolve compiler");
        println!("rustc: {:?}", compiler.rustc);
        println!("sysroot: {:?}", compiler.sysroot);
        println!("std dylib: {:?}", compiler.std_dylib);
    }

    #[test]
    fn test_parse_dep_file_simple() {
        let contents = "/path/to/output.dylib: /path/to/src/lib.rs /path/to/src/main.rs";
        let deps = Compiler::parse_dep_file(contents).unwrap();
        assert_eq!(deps, vec![
            PathBuf::from("/path/to/src/lib.rs"),
            PathBuf::from("/path/to/src/main.rs"),
        ]);
    }

    #[test]
    fn test_parse_dep_file_continuation() {
        let contents = "/path/to/output.dylib: /path/to/src/lib.rs \\\n/path/to/src/main.rs";
        let deps = Compiler::parse_dep_file(contents).unwrap();
        assert_eq!(deps, vec![
            PathBuf::from("/path/to/src/lib.rs"),
            PathBuf::from("/path/to/src/main.rs"),
        ]);
    }

    #[test]
    fn test_parse_dep_file_escaped_spaces() {
        let contents = "/path/output.dylib: /path/my\\ file.rs /path/other.rs";
        let deps = Compiler::parse_dep_file(contents).unwrap();
        assert_eq!(deps, vec![
            PathBuf::from("/path/my file.rs"),
            PathBuf::from("/path/other.rs"),
        ]);
    }

    #[test]
    fn test_parse_dep_file_empty() {
        let contents = "/path/output.dylib: ";
        let deps = Compiler::parse_dep_file(contents).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_dep_file_no_colon() {
        let contents = "no colon here";
        assert!(Compiler::parse_dep_file(contents).is_none());
    }

    #[test]
    fn test_parse_patches() {
        let patches = parse_patches();
        assert!(patches.len() >= 2, "Expected at least 2 patches, got {}", patches.len());

        let names: Vec<&str> = patches.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tau"), "Missing tau patch");
        assert!(names.contains(&"tokio"), "Missing tokio patch");
        // tau-rt is NOT patched — it's always provided as a prebuilt dylib via --extern

        for e in &patches {
            println!("  {} → {}", e.name, e.rel_path);
        }
    }
}
