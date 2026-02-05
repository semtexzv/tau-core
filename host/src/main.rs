//! Host application with async plugin interaction
//!
//! The host:
//! 1. Can compile plugins on-the-fly from source
//! 2. Loads runtime dylib with RTLD_GLOBAL (makes symbols visible)
//! 3. Loads plugin (resolves symbols from runtime)
//! 4. Submits requests and drives runtime

mod compiler;
mod runtime;
use compiler::Compiler;
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Timer {
    _name: &'static str,
    start: Instant,
}

impl Timer {
    fn new(name: &'static str) -> Self {
        Self {
            _name: name,
            start: Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _duration = self.start.elapsed();
        // Timer suppressed
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub id: u64,
    pub payload: String,
}

use tau::types::{FfiPoll, FfiWaker, TaskId};

/// Plugin hooks structure
#[repr(C)]
struct PluginHooks {
    process: unsafe extern "C" fn(*const u8, usize) -> TaskId,
}

type PluginInitFn = unsafe extern "C" fn(*mut PluginHooks) -> i32;
type PluginDestroyFn = unsafe extern "C" fn();

pub struct AsyncPlugin {
    _handle: *mut libc::c_void,
    hooks: PluginHooks,
    destroy: PluginDestroyFn,
}

impl AsyncPlugin {
    unsafe fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use std::ffi::CString;
        let path_c = CString::new(path)?;

        let handle = libc::dlopen(path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let err = std::ffi::CStr::from_ptr(libc::dlerror());
            return Err(format!("dlopen plugin failed: {}", err.to_string_lossy()).into());
        }

        let init_sym = libc::dlsym(handle, b"plugin_init\0".as_ptr() as *const _);
        let destroy_sym = libc::dlsym(handle, b"plugin_destroy\0".as_ptr() as *const _);

        if init_sym.is_null() || destroy_sym.is_null() {
            return Err("Failed to find plugin symbols".into());
        }

        let init: PluginInitFn = std::mem::transmute(init_sym);
        let destroy: PluginDestroyFn = std::mem::transmute(destroy_sym);

        let mut hooks = MaybeUninit::<PluginHooks>::uninit();
        let result = init(hooks.as_mut_ptr());
        if result != 0 {
            return Err("Plugin init failed".into());
        }
        let hooks = hooks.assume_init();

        Ok(Self {
            _handle: handle,
            hooks,
            destroy,
        })
    }

    fn submit(&self, msg: &Message) -> TaskId {
        let json = serde_json::to_vec(msg).unwrap();
        unsafe { (self.hooks.process)(json.as_ptr(), json.len()) }
    }
}

impl Drop for AsyncPlugin {
    fn drop(&mut self) {
        unsafe { (self.destroy)() };
    }
}

/// Host context with runtime and optional compiler
pub struct Host {
    compiler: Option<Compiler>,
    _runtime_path: PathBuf,
    interface_path: PathBuf,
    dist_lib_path: PathBuf,
}

impl Host {
    /// Create a new host, loading the runtime
    /// Create a new host, loading the runtime
    pub fn new(
        interface_path: &Path,
        enable_compiler: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let _total = Timer::new("Host::new");

        // Runtime init (internal)
        {
            let _t = Timer::new("Runtime::init");
            runtime::tau_runtime_init();
        }

        // Try to detect compiler
        let compiler = if enable_compiler {
            let _t = Timer::new("Compiler::resolve");
            Compiler::resolve().ok()
        } else {
            None
        };

        if compiler.is_some() {
            println!("[Host] Compiler detected, dynamic compilation available");
        }

        // Dist / Dev lib path logic
        let exe_path = std::env::current_exe()?;
        let exe_dir = exe_path.parent().ok_or("No parent dir")?;

        // We need to know where `libtau.dylib` is to link against it.
        // We can assume it is in `../lib` relative to exe (dist structure)
        // or adjacent (dev structure).
        let dist_lib_path = if exe_dir.join("libtau.dylib").exists() {
            exe_dir.to_path_buf()
        } else if exe_dir
            .parent()
            .unwrap()
            .join("lib")
            .join("libtau.dylib")
            .exists()
        {
            exe_dir.parent().unwrap().join("lib")
        } else {
            exe_dir.to_path_buf()
        };

        Ok(Self {
            compiler,
            _runtime_path: PathBuf::new(),
            interface_path: interface_path.to_path_buf(),
            dist_lib_path,
        })
    }

    /// Compile a plugin from source
    pub fn compile_plugin(&self, source: &Path, output_dir: &Path) -> Result<PathBuf, String> {
        let compiler = self.compiler.as_ref().ok_or("Compiler not available")?;
        compiler.compile_plugin(
            source,
            output_dir,
            &self.interface_path,
            &self.dist_lib_path,
        )
    }

    /// Load a plugin (from dylib path)
    pub fn load_plugin(&self, path: &Path) -> Result<AsyncPlugin, Box<dyn std::error::Error>> {
        unsafe { AsyncPlugin::load(path.to_str().unwrap()) }
    }

    /// Run a single request through a plugin
    pub fn run_request(&self, plugin: &AsyncPlugin, msg: &Message) -> Message {
        let task_id = plugin.submit(msg);

        loop {
            // Static runtime call
            runtime::tau_drive();
            // We need a way to check if task is ready without consuming it?
            // Or `tau_poll_task` returns state.
            // But we need to construct a dummy waker?

            // The `poll_task` function exposed by runtime expects a waker.
            // For checking completion from host, we can pass null waker if supported,
            // or a dummy one.
            let waker = tau::types::FfiWaker::null();

            if runtime::tau_poll_task(task_id, waker) == tau::types::FfiPoll::Ready {
                // For now return input (need result retrieval)
                return msg.clone();
            }
        }
    }

    /// Get std dylib path (for DYLD_LIBRARY_PATH)
    pub fn std_lib_path(&self) -> Option<PathBuf> {
        self.compiler
            .as_ref()
            .map(|c| c.std_dylib.parent().unwrap().to_path_buf())
    }
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("Usage:");
    eprintln!("  tau install <user>/<repo>     - Install plugin from GitHub");
    eprintln!("  tau                           - Run implicitly discovered plugins");
    eprintln!("  tau --plugin <path>           - Run specific plugin (can be repeated)");
    eprintln!("  tau --info                    - Show system info");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let _setup_timer = Timer::new("Startup Path Resolution");

    // Determine paths
    // 2. Load Runtime
    // We ARE the runtime now.
    // But we still need to locate the interface library for plugins to link against?
    // Actually, plugins link against `libtau.dylib`.
    // The interface path resolution is still needed for compilation.

    let exe_path = std::env::current_exe().expect("Failed to get exe path");
    let exe_dir = exe_path.parent().unwrap();

    // Interface (for dev/compile mode) - try to find it relative to source checkout or dist
    // In dist: ../interface/tau
    // In dev: ../../../tau
    let interface_path_dist = exe_dir.parent().unwrap().join("interface").join("tau");
    let interface_path_dev = exe_dir.parent().unwrap().parent().unwrap().join("tau");

    let interface_path = if interface_path_dist.exists() {
        interface_path_dist
    } else {
        interface_path_dev
    };

    // End of path resolution
    drop(_setup_timer);

    let discovered = if args.len() <= 1 || (args.len() == 2 && args[1] == "--info") {
        // Default mode
        Some(discover_plugins())
    } else {
        None
    };

    match args.get(1).map(|s| s.as_str()) {
        Some("--info") => {
            println!("=== µTokio Host Info ===");

            println!("\n[Compiled Info]");
            println!("Build Version:   {}", Compiler::HOST_VERSION);
            println!("Build Flags:     {}", Compiler::HOST_RUSTFLAGS);
            println!("Panic Strategy:  {}", Compiler::HOST_PANIC);
            println!("Target Features: {}", Compiler::HOST_TARGET_FEATURES);

            println!("\n[Runtime Info]");
            println!("Interface Path:  {:?}", interface_path);

            match {
                let _t = Timer::new("Compiler::resolve (info)");
                Compiler::resolve()
            } {
                Ok(c) => {
                    println!("Detected Rust:   {}", c.version_string);
                    println!("Sysroot:         {:?}", c.sysroot);
                    println!("Std Dylib:       {:?}", c.std_dylib);
                    println!("Target:          {}", c.target);
                    // Since we init from HOST_* env vars, it matches by definition
                    println!("Status:          MATCH (Captured)");
                }
                Err(e) => println!("Compiler:        Not found ({})", e),
            }
        }

        Some("--compile") => {
            if args.len() < 3 {
                eprintln!("Missing source file");
                std::process::exit(1);
            }

            let source = PathBuf::from(&args[2]);
            if !source.exists() {
                eprintln!("Source file not found: {:?}", source);
                std::process::exit(1);
            }

            println!("[Host] Initializing...");
            let host = Host::new(&interface_path, true).expect("Failed to create host");

            println!("[Host] Compiling plugin from {:?}...", source);
            let output_dir = exe_dir;
            let plugin_path = host
                .compile_plugin(&source, &output_dir)
                .expect("Compilation failed");
            println!("[Host] Compiled: {:?}", plugin_path);

            println!("[Host] Loading computed plugin...");
            let plugin = host
                .load_plugin(&plugin_path)
                .expect("Failed to load plugin");
            run_plugins(&host, &[plugin]);
        }

        Some("--install") | Some("install") => {
            if args.len() < 3 {
                eprintln!("Usage: tau install <user>/<repo>");
                std::process::exit(1);
            }
            let repo_slug = &args[2];
            let parts: Vec<&str> = repo_slug.split('/').collect();
            if parts.len() != 2 {
                eprintln!("Invalid format. Use <user>/<repo>");
                std::process::exit(1);
            }

            let home = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."));
            let plugins_dir = home.join(".tau").join("plugins");
            let target_dir = plugins_dir.join(parts[1]);

            if target_dir.exists() {
                println!("Plugin already installed in {:?}", target_dir);
                return;
            }

            std::fs::create_dir_all(&plugins_dir).expect("Failed to create plugins dir");

            println!("Installing {}...", repo_slug);
            let url = format!("https://github.com/{}.git", repo_slug);

            // Use shallow clone
            let status = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &url, target_dir.to_str().unwrap()])
                .status()
                .expect("Failed to run git");

            if status.success() {
                println!("Installed to {:?}", target_dir);
            } else {
                eprintln!("Installation failed");
                std::process::exit(1);
            }
        }

        _ => {
            let mut args_iter = args.iter().skip(1);
            let mut expl_plugins = Vec::new();

            while let Some(arg) = args_iter.next() {
                if arg == "--plugin" {
                    if let Some(path) = args_iter.next() {
                        expl_plugins.push(PathBuf::from(path));
                    }
                }
            }
            for p in &expl_plugins {
                println!("  Found (explicit): {:?}", p);
            }

            let mut all_paths = expl_plugins;
            if let Some(discovered) = discovered {
                all_paths.extend(discovered);
            }

            if all_paths.is_empty() {
                println!("[Host] No plugins found. (Use 'tau install' or create .tau/plugins)");
                return;
            }

            println!("[Host] Initializing...");

            // Enable compiler if ANY input is a directory
            let need_compiler = all_paths.iter().any(|p| p.is_dir());

            let host = Host::new(&interface_path, need_compiler).expect("Failed to create host");

            let mut loaded_plugins = Vec::new();
            let output_dir = std::env::current_dir().unwrap();

            for path in all_paths {
                if path.is_dir() {
                    if !path.join("Cargo.toml").exists() {
                        eprintln!("Skipping {:?}: No Cargo.toml", path);
                        continue;
                    }
                    println!("[Host] Compiling {:?}...", path);
                    match host.compile_plugin(&path, &output_dir) {
                        Ok(dylib) => {
                            println!("[Host] Loading {:?}...", dylib);
                            match host.load_plugin(&dylib) {
                                Ok(p) => loaded_plugins.push(p),
                                Err(e) => eprintln!("Failed to load {:?}: {}", dylib, e),
                            }
                        }
                        Err(e) => eprintln!("Failed to compile {:?}: {}", path, e),
                    }
                } else if path.extension().map_or(false, |e| e == "dylib") {
                    println!("[Host] Loading {:?}...", path);
                    match host.load_plugin(&path) {
                        Ok(p) => loaded_plugins.push(p),
                        Err(e) => eprintln!("Failed to load {:?}: {}", path, e),
                    }
                } else {
                    eprintln!("Skipping {:?}: Not a crate or dylib", path);
                }
            }

            if !loaded_plugins.is_empty() {
                run_plugins(&host, &loaded_plugins);
            }
        }
    }
}

fn discover_plugins() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Helper to scan a dir for possible plugins
    let scan_dir = |dir: PathBuf| -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // If directory with Cargo.toml -> Crate
                // If file with .dylib -> Plugin binary
                if path.is_dir() && path.join("Cargo.toml").exists() {
                    found.push(path);
                } else if path.is_file() && path.extension().map_or(false, |e| e == "dylib") {
                    found.push(path);
                }
            }
        }
        found
    };

    // 1. User Global: ~/.tau/plugins
    if let Ok(home) = std::env::var("HOME") {
        let user_plugins = PathBuf::from(home).join(".tau").join("plugins");
        if user_plugins.exists() {
            paths.extend(scan_dir(user_plugins));
        }
    }

    // 2. Workspace: Walk up from CWD
    if let Ok(cwd) = std::env::current_dir() {
        let mut current = cwd.as_path();
        loop {
            let workspace_plugins = current.join(".tau").join("plugins");
            if workspace_plugins.exists() {
                paths.extend(scan_dir(workspace_plugins));
                // Stop at first workspace found (standard behavior), or keep going?
                // Let's assume nested workspaces are rare/confusing, stop at first.
                break;
            }
            match current.parent() {
                Some(p) => current = p,
                None => break,
            }
        }
    }

    // Deduplicate? Paths are absolute, so simple dedup works.
    paths.sort();
    paths.dedup();
    paths
}

fn run_plugins(host: &Host, plugins: &[AsyncPlugin]) {
    println!("[Host] Running {} loaded plugins...", plugins.len());

    // Warmup all
    for plugin in plugins {
        for i in 0..100 {
            let msg = Message {
                id: i,
                payload: "warmup".to_string(),
            };
            host.run_request(plugin, &msg);
        }
    }

    // Benchmark loop (simple check for now)
    let iterations = 10_000u64;
    let payload = "hello tau".to_string();

    let start = Instant::now();
    for i in 0..iterations {
        let msg = black_box(Message {
            id: i,
            payload: payload.clone(),
        });
        // Distribute load? Or run on all?
        // Let's run on round-robin
        let plugin = &plugins[(i as usize) % plugins.len()];
        let _ = black_box(host.run_request(plugin, &msg));
    }
    let elapsed = start.elapsed();

    println!("=== Tau Runtime Stats ===");
    println!("Plugins Active: {}", plugins.len());
    println!("Total Requests: {}", iterations);
    println!("Total Time:     {:.2?}", elapsed);
    let throughput = iterations as f64 / elapsed.as_secs_f64();
    println!("Throughput:     {:.0} req/sec", throughput);
}
