//! Tau Host — async runtime + dynamic plugin loader
//!
//! The host:
//! 1. Initializes the shared runtime (exports #[no_mangle] symbols)
//! 2. Can compile plugins on-the-fly from source
//! 3. Loads plugin cdylibs via dlopen (symbols resolve from this process)
//! 4. Submits requests and drives the runtime

mod compiler;
mod runtime;

use compiler::{Compiler, ResolvedPatches};
use serde::{Deserialize, Serialize};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub extern crate tau as _;

// =============================================================================
// Message type (shared protocol with plugins)
// =============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub id: u64,
    pub payload: String,
}

// =============================================================================
// Plugin loading via dlopen
// =============================================================================

/// Plugin hooks structure — plugin_init fills this in.
#[repr(C)]
struct PluginHooks {
    process: unsafe extern "C" fn(*const u8, usize) -> u64,
}

type PluginInitFn = unsafe extern "C" fn(*mut PluginHooks, u64, *const tau::PluginGuard) -> i32;
type PluginDestroyFn = unsafe extern "C" fn();

pub struct AsyncPlugin {
    handle: *mut libc::c_void,
    hooks: PluginHooks,
    destroy: PluginDestroyFn,
    plugin_id: u64,
}

impl AsyncPlugin {
    unsafe fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use std::ffi::CString;
        let path_c = CString::new(path)?;

        let handle = libc::dlopen(path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let err = std::ffi::CStr::from_ptr(libc::dlerror());
            return Err(format!("dlopen failed: {}", err.to_string_lossy()).into());
        }

        let init_sym = libc::dlsym(handle, b"plugin_init\0".as_ptr() as *const _);
        let destroy_sym = libc::dlsym(handle, b"plugin_destroy\0".as_ptr() as *const _);

        if init_sym.is_null() || destroy_sym.is_null() {
            libc::dlclose(handle);
            return Err("Failed to find plugin_init/plugin_destroy symbols".into());
        }

        let init: PluginInitFn = std::mem::transmute(init_sym);
        let destroy: PluginDestroyFn = std::mem::transmute(destroy_sym);

        // Allocate a unique plugin ID for task tracking
        let plugin_id = runtime::allocate_plugin_id();

        let mut hooks = MaybeUninit::<PluginHooks>::uninit();

        // Create a PluginGuard for this plugin's dlopen handle
        let guard = runtime::plugin_guard::guard_from_dlopen(handle, plugin_id);
        // Heap-allocate so we can pass a stable pointer across FFI
        let guard_box = Box::new(guard);
        let guard_ptr = &*guard_box as *const tau::PluginGuard;

        // Set current plugin before init so resource/event calls are tagged
        runtime::set_current_plugin(plugin_id);
        let result = init(hooks.as_mut_ptr(), plugin_id, guard_ptr);
        runtime::clear_current_plugin();
        // Guard was cloned by the plugin's init; drop our copy
        drop(guard_box);
        if result != 0 {
            libc::dlclose(handle);
            return Err("Plugin init failed".into());
        }

        Ok(Self {
            handle,
            hooks: hooks.assume_init(),
            destroy,
            plugin_id,
        })
    }

    fn submit(&self, msg: &Message) -> u64 {
        // Set current plugin so spawned tasks are tracked
        runtime::set_current_plugin(self.plugin_id);
        let json = serde_json::to_vec(msg).unwrap();
        let task_id = unsafe { (self.hooks.process)(json.as_ptr(), json.len()) };
        runtime::clear_current_plugin();
        task_id
    }

    /// Unload the plugin, dropping all its tasks first.
    pub fn unload(self) {
        // Drop is called automatically, which handles cleanup
        drop(self);
    }
}

impl Drop for AsyncPlugin {
    fn drop(&mut self) {
        // 1. Call the plugin's destroy function (plugin can clean up voluntarily)
        unsafe { (self.destroy)() };

        // 2. Drop all tasks owned by this plugin (future destructors run, plugin still loaded)
        let tasks = runtime::drop_plugin_tasks(self.plugin_id);
        if tasks > 0 {
            println!("[Host] Dropped {} tasks from plugin {}", tasks, self.plugin_id);
        }

        // 3. Drop all event subscriptions (callback ctx dropped, plugin still loaded)
        let events = runtime::drop_plugin_events(self.plugin_id);
        if events > 0 {
            println!("[Host] Dropped {} event subscriptions from plugin {}", events, self.plugin_id);
        }

        // 4. Drop all resources owned by this plugin (drop_fn called, plugin still loaded)
        let resources = runtime::drop_plugin_resources(self.plugin_id);
        if resources > 0 {
            println!("[Host] Dropped {} resources from plugin {}", resources, self.plugin_id);
        }

        // 5. Now safe to unload the dylib — no code pointers remain
        unsafe {
            let result = libc::dlclose(self.handle);
            if result != 0 {
                let err = std::ffi::CStr::from_ptr(libc::dlerror());
                eprintln!("[Host] dlclose failed: {}", err.to_string_lossy());
            }
        }
    }
}

// =============================================================================
// Host
// =============================================================================

pub struct Host {
    compiler: Option<Compiler>,
    patches: ResolvedPatches,
    dist_lib_path: PathBuf,
}

impl Host {
    pub fn new(
        patches: ResolvedPatches,
        enable_compiler: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize runtime
        runtime::tau_runtime_init();

        let compiler = if enable_compiler {
            Compiler::resolve().ok()
        } else {
            None
        };

        if compiler.is_some() {
            println!("[Host] Compiler detected");
        }

        let exe_path = std::env::current_exe()?;
        let exe_dir = exe_path.parent().ok_or("No parent dir")?;

        // Look for libtau_rt.dylib (the shared runtime dylib)
        let dist_lib_path = if exe_dir.join("libtau_rt.dylib").exists() {
            exe_dir.to_path_buf()
        } else if exe_dir.parent().unwrap().join("lib").join("libtau_rt.dylib").exists() {
            exe_dir.parent().unwrap().join("lib")
        } else {
            exe_dir.to_path_buf()
        };

        Ok(Self {
            compiler,
            patches,
            dist_lib_path,
        })
    }

    pub fn compile_plugin(&self, source: &Path) -> Result<PathBuf, String> {
        let compiler = self.compiler.as_ref().ok_or("Compiler not available")?;
        compiler.compile_plugin(source, &self.patches, &self.dist_lib_path)
    }

    pub fn load_plugin(&self, path: &Path) -> Result<AsyncPlugin, Box<dyn std::error::Error>> {
        unsafe { AsyncPlugin::load(path.to_str().unwrap()) }
    }

    /// Submit a message to a plugin and drive the runtime until the task completes.
    pub fn run_request(&self, plugin: &AsyncPlugin, msg: &Message) {
        let task_id = plugin.submit(msg);

        // Drive the runtime until this task completes.
        runtime::tau_block_on(task_id);
    }

    pub fn std_lib_path(&self) -> Option<PathBuf> {
        self.compiler.as_ref().map(|c| c.std_dylib.parent().unwrap().to_path_buf())
    }
}

// =============================================================================
// Plugin discovery
// =============================================================================

fn discover_plugins() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let scan_dir = |dir: PathBuf| -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("Cargo.toml").exists() {
                    found.push(path);
                } else if path.is_file()
                    && path.extension().map_or(false, |e| e == "dylib" || e == "so")
                {
                    found.push(path);
                }
            }
        }
        found
    };

    // User global: ~/.tau/plugins
    if let Ok(home) = std::env::var("HOME") {
        let user_plugins = PathBuf::from(home).join(".tau").join("plugins");
        if user_plugins.exists() {
            paths.extend(scan_dir(user_plugins));
        }
    }

    // Workspace: walk up from CWD
    if let Ok(cwd) = std::env::current_dir() {
        let mut current = cwd.as_path();
        loop {
            let workspace_plugins = current.join(".tau").join("plugins");
            if workspace_plugins.exists() {
                paths.extend(scan_dir(workspace_plugins));
                break;
            }
            match current.parent() {
                Some(p) => current = p,
                None => break,
            }
        }
    }

    paths.sort();
    paths.dedup();
    paths
}

// =============================================================================
// Main
// =============================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let exe_path = std::env::current_exe().expect("Failed to get exe path");
    let exe_dir = exe_path.parent().unwrap();

    // Resolve interface crate paths (tau + tokio shim).
    // In dist layout: dist/src/{tau,tokio}
    // Resolve patch paths.
    // In dist layout: dist/src/{tau,tau-rt,tokio}/
    // In dev layout:  workspace_root/crates/{tau,tau-rt,tau-tokio}/
    //
    // patches.list is relative to workspace root. We detect which layout
    // we're in by checking if the dist src dir exists.
    let patches = {
        let dist_src_root = exe_dir.parent().unwrap().join("src");
        // dev: exe is in target/debug/tau, workspace root is ../../
        let workspace_root = exe_dir.parent().unwrap().parent().unwrap();

        if dist_src_root.join("tau").exists() {
            // Dist layout — patches.list paths are remapped to dist/src/<name>
            let entries = compiler::parse_patches()
                .into_iter()
                .map(|e| (e.name.clone(), dist_src_root.join(&e.name)))
                .collect();
            ResolvedPatches { entries }
        } else {
            // Dev layout — resolve relative to workspace root
            ResolvedPatches::resolve(workspace_root)
        }
    };

    let discovered = if args.len() <= 1 || (args.len() == 2 && args[1] == "--info") {
        Some(discover_plugins())
    } else {
        None
    };

    match args.get(1).map(|s| s.as_str()) {
        Some("--info") => {
            println!("=== Tau Host Info ===");
            println!("Version:     {}", Compiler::HOST_VERSION);
            println!("Target:      {}", Compiler::HOST_TARGET);
            println!("Panic:       {}", Compiler::HOST_PANIC);
            println!("Patch crates:");
            for (name, path) in &patches.entries {
                println!("  {} → {:?}", name, path);
            }

            match Compiler::resolve() {
                Ok(c) => {
                    println!("Rustc:       {}", c.version_string);
                    println!("Sysroot:     {:?}", c.sysroot);
                    println!("Std dylib:   {:?}", c.std_dylib);
                }
                Err(e) => println!("Compiler:    Not found ({})", e),
            }
        }

        Some("install") => {
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

            let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
            let plugins_dir = home.join(".tau").join("plugins");
            let target_dir = plugins_dir.join(parts[1]);

            if target_dir.exists() {
                println!("Already installed at {:?}", target_dir);
                return;
            }

            std::fs::create_dir_all(&plugins_dir).expect("Failed to create plugins dir");
            println!("Installing {}...", repo_slug);
            let url = format!("https://github.com/{}.git", repo_slug);
            let status = std::process::Command::new("git")
                .args(["clone", "--depth", "1", &url, target_dir.to_str().unwrap()])
                .status()
                .expect("git failed");

            if status.success() {
                println!("Installed to {:?}", target_dir);
            } else {
                eprintln!("Installation failed");
                std::process::exit(1);
            }
        }

        Some("test") => {
            // Scripted test mode: reads commands from stdin
            // Commands:
            //   load <name> <path>     — compile (if dir) and load plugin, assign name
            //   unload <name>          — unload a loaded plugin
            //   send <name> <payload>  — send a request to a named plugin
            //   sleep <ms>             — sleep for N milliseconds
            //   assert_resource <name> — check that a resource exists
            //   assert_no_resource <name> — check that a resource does NOT exist
            //   print <msg>            — print a message
            let host = Host::new(patches, true).expect("Failed to init host");
            run_test_mode(&host);
        }

        Some("bench") => {
            // Usage: tau bench <mode> [iterations]
            // Modes: load, ipc, spawn, event, all
            let mode = args.get(2).map(|s| s.as_str()).unwrap_or("all");
            let iters: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10_000);
            let host = Host::new(patches, true).expect("Failed to init host");
            run_bench(&host, mode, iters);
        }

        Some("--reload-test") => {
            // Test plugin reload: tau --reload-test <plugin-path>
            if args.len() < 3 {
                eprintln!("Usage: tau --reload-test <plugin-path>");
                std::process::exit(1);
            }
            let path = PathBuf::from(&args[2]);
            let host = Host::new(patches, path.is_dir()).expect("Failed to init host");
            
            let dylib_path = if path.is_dir() {
                println!("[Host] Compiling {:?}...", path);
                match host.compile_plugin(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Compile failed: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                path
            };
            
            test_reload(&host, &dylib_path);
        }

        _ => {
            // Collect explicit --plugin args
            let mut args_iter = args.iter().skip(1);
            let mut explicit_plugins = Vec::new();
            let mut reload_test = false;
            while let Some(arg) = args_iter.next() {
                if arg == "--plugin" {
                    if let Some(path) = args_iter.next() {
                        explicit_plugins.push(PathBuf::from(path));
                    }
                } else if arg == "--reload" {
                    reload_test = true;
                }
            }

            let mut all_paths = explicit_plugins;
            if let Some(discovered) = discovered {
                all_paths.extend(discovered);
            }

            if all_paths.is_empty() {
                println!("[Host] No plugins found. Use 'tau install <user>/<repo>' or 'tau --plugin <path>'");
                return;
            }

            let need_compiler = all_paths.iter().any(|p| p.is_dir());
            let host = Host::new(patches, need_compiler).expect("Failed to init host");

            let mut loaded_plugins = Vec::new();

            for path in &all_paths {
                if path.is_dir() {
                    if !path.join("Cargo.toml").exists() {
                        eprintln!("Skipping {:?}: no Cargo.toml", path);
                        continue;
                    }
                    println!("[Host] Compiling {:?}...", path);
                    match host.compile_plugin(path) {
                        Ok(dylib) => match host.load_plugin(&dylib) {
                            Ok(p) => loaded_plugins.push(p),
                            Err(e) => eprintln!("Load failed {:?}: {}", dylib, e),
                        },
                        Err(e) => eprintln!("Compile failed {:?}: {}", path, e),
                    }
                } else if path.extension().map_or(false, |e| e == "dylib" || e == "so") {
                    println!("[Host] Loading {:?}...", path);
                    match host.load_plugin(path) {
                        Ok(p) => loaded_plugins.push(p),
                        Err(e) => eprintln!("Load failed {:?}: {}", path, e),
                    }
                } else {
                    eprintln!("Skipping {:?}: not a crate or dylib", path);
                }
            }

            if loaded_plugins.is_empty() {
                println!("[Host] No plugins loaded.");
                return;
            }

            run_plugins(&host, &loaded_plugins);
        }
    }
}

// =============================================================================
// Scripted test mode
// =============================================================================

fn run_test_mode(host: &Host) {
    use std::collections::HashMap;
    use std::io::BufRead;

    let mut plugins: HashMap<String, AsyncPlugin> = HashMap::new();
    let mut dylib_cache: HashMap<String, PathBuf> = HashMap::new();
    let mut test_count = 0u32;
    let mut pass_count = 0u32;
    let mut fail_count = 0u32;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        let cmd = parts[0];

        match cmd {
            "load" => {
                if parts.len() < 3 {
                    eprintln!("Usage: load <name> <path>");
                    continue;
                }
                let name = parts[1].to_string();
                let path = PathBuf::from(parts[2]);

                // Compile if needed (cache the dylib path)
                let dylib_path = if let Some(cached) = dylib_cache.get(parts[2]) {
                    cached.clone()
                } else if path.is_dir() {
                    match host.compile_plugin(&path) {
                        Ok(p) => {
                            dylib_cache.insert(parts[2].to_string(), p.clone());
                            p
                        }
                        Err(e) => {
                            eprintln!("COMPILE_FAIL: {}", e);
                            continue;
                        }
                    }
                } else {
                    path
                };

                match host.load_plugin(&dylib_path) {
                    Ok(p) => {
                        println!("OK load {}", name);
                        plugins.insert(name, p);
                    }
                    Err(e) => eprintln!("LOAD_FAIL {}: {}", name, e),
                }
            }

            "unload" => {
                if parts.len() < 2 {
                    eprintln!("Usage: unload <name>");
                    continue;
                }
                let name = parts[1];
                if let Some(p) = plugins.remove(name) {
                    drop(p);
                    println!("OK unload {}", name);
                } else {
                    eprintln!("NOT_FOUND: {}", name);
                }
            }

            "send" => {
                if parts.len() < 3 {
                    eprintln!("Usage: send <name> <payload>");
                    continue;
                }
                let name = parts[1];
                let payload = parts[2].to_string();
                if let Some(p) = plugins.get(name) {
                    let msg = Message {
                        id: test_count as u64,
                        payload,
                    };
                    host.run_request(p, &msg);
                    println!("OK send {}", name);
                } else {
                    eprintln!("NOT_FOUND: {}", name);
                }
            }

            "sleep" => {
                let ms: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }

            "assert_resource" => {
                if parts.len() < 2 {
                    eprintln!("Usage: assert_resource <name>");
                    continue;
                }
                let name = parts[1];
                test_count += 1;
                let exists = runtime::resources::resource_exists(name);
                if exists {
                    pass_count += 1;
                    println!("PASS assert_resource {}", name);
                } else {
                    fail_count += 1;
                    println!("FAIL assert_resource {} — not found", name);
                }
            }

            "assert_no_resource" => {
                if parts.len() < 2 {
                    eprintln!("Usage: assert_no_resource <name>");
                    continue;
                }
                let name = parts[1];
                test_count += 1;
                let exists = runtime::resources::resource_exists(name);
                if !exists {
                    pass_count += 1;
                    println!("PASS assert_no_resource {}", name);
                } else {
                    fail_count += 1;
                    println!("FAIL assert_no_resource {} — still exists", name);
                }
            }

            "print" => {
                let msg = if parts.len() > 1 { &line[6..] } else { "" };
                println!("--- {} ---", msg);
            }

            _ => {
                eprintln!("Unknown command: {}", cmd);
            }
        }
    }

    // Cleanup remaining plugins
    let names: Vec<String> = plugins.keys().cloned().collect();
    for name in names {
        if let Some(p) = plugins.remove(&name) {
            drop(p);
        }
    }

    println!();
    println!("=== Test Results ===");
    println!("Tests:  {}", test_count);
    println!("Passed: {}", pass_count);
    println!("Failed: {}", fail_count);

    if fail_count > 0 {
        std::process::exit(1);
    }
}

fn run_plugins(host: &Host, plugins: &[AsyncPlugin]) {
    println!("[Host] Running {} plugin(s)...", plugins.len());

    let iterations = 10u64;  // reduced for testing
    let start = Instant::now();

    for i in 0..iterations {
        let msg = Message {
            id: i,
            payload: format!("hello #{}", i),
        };
        let plugin = &plugins[(i as usize) % plugins.len()];
        host.run_request(plugin, &msg);
    }

    let elapsed = start.elapsed();
    let throughput = iterations as f64 / elapsed.as_secs_f64();

    println!("=== Results ===");
    println!("Plugins:    {}", plugins.len());
    println!("Requests:   {}", iterations);
    println!("Time:       {:.2?}", elapsed);
    println!("Throughput: {:.0} req/sec", throughput);
}

/// Test plugin reload - load, run, unload, reload, run again
fn test_reload(host: &Host, dylib_path: &Path) {
    println!("[Reload Test] Loading plugin...");
    let plugin = match host.load_plugin(dylib_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[Reload Test] Failed to load: {}", e);
            return;
        }
    };

    // Run a few requests
    for i in 0..3 {
        let msg = Message { id: i, payload: format!("test #{}", i) };
        host.run_request(&plugin, &msg);
    }
    println!("[Reload Test] First run complete");

    // Unload
    println!("[Reload Test] Unloading plugin...");
    drop(plugin);
    println!("[Reload Test] Plugin unloaded");

    // Small delay to ensure cleanup
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Reload
    println!("[Reload Test] Reloading plugin...");
    let plugin = match host.load_plugin(dylib_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[Reload Test] Failed to reload: {}", e);
            return;
        }
    };

    // Run again
    for i in 0..3 {
        let msg = Message { id: i + 100, payload: format!("reload test #{}", i) };
        host.run_request(&plugin, &msg);
    }
    println!("[Reload Test] Second run complete");

    // Final unload
    drop(plugin);
    println!("[Reload Test] SUCCESS - Plugin reloaded and ran correctly!");
}

// =============================================================================
// Benchmark — simple tight loops for flamegraph profiling
// =============================================================================

fn run_bench(host: &Host, mode: &str, iters: usize) {
    let bench_dir = PathBuf::from("plugins/bench-plugin");

    eprintln!("Compiling bench-plugin...");
    let dylib = host.compile_plugin(&bench_dir).expect("compile bench-plugin");

    eprintln!("Loading...");
    let plugin = unsafe { AsyncPlugin::load(dylib.to_str().unwrap()) }.expect("load failed");

    // Warm up
    for _ in 0..10 {
        let msg = Message { id: 0, payload: "noop".to_string() };
        host.run_request(&plugin, &msg);
    }

    eprintln!("Running bench mode={} iters={}", mode, iters);

    match mode {
        "load"  => bench_load(&dylib, iters),
        "ipc"   => bench_ipc(host, &plugin, iters),
        "spawn" => bench_spawn(host, &plugin, iters),
        "event" => bench_event(host, &plugin, iters),
        "all"   => {
            bench_ipc(host, &plugin, iters);
            bench_event(host, &plugin, iters);
            bench_spawn(host, &plugin, iters);
            bench_load(&dylib, iters.min(500));
        }
        _ => eprintln!("Unknown: {}. Use load|ipc|spawn|event|all", mode),
    }

    drop(plugin);
    eprintln!("Done.");
}

#[inline(never)]
fn bench_load(dylib: &Path, n: usize) {
    for _ in 0..n {
        let p = unsafe { AsyncPlugin::load(dylib.to_str().unwrap()) }.unwrap();
        drop(p);
    }
}

#[inline(never)]
fn bench_ipc(host: &Host, plugin: &AsyncPlugin, n: usize) {
    for _ in 0..n {
        host.run_request(plugin, &Message { id: 0, payload: "resource_put 100".into() });
        host.run_request(plugin, &Message { id: 0, payload: "resource_get 100".into() });
    }
}

#[inline(never)]
fn bench_event(host: &Host, plugin: &AsyncPlugin, n: usize) {
    host.run_request(plugin, &Message { id: 0, payload: "event_sub".into() });
    for _ in 0..n {
        host.run_request(plugin, &Message { id: 0, payload: "event_emit 100".into() });
    }
}

#[inline(never)]
fn bench_spawn(host: &Host, plugin: &AsyncPlugin, n: usize) {
    for _ in 0..n {
        host.run_request(plugin, &Message { id: 0, payload: "spawn 100".into() });
    }
}
