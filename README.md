# Tau Core

A shared async runtime with dynamic plugin infrastructure. Plugins are separately-compiled `.cdylib` crates that share a single-threaded async executor with the host via dynamic linking.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host Binary                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │  Executor   │  │   Reactor   │  │  Exported FFI Symbols   │  │
│  │  (tasks,    │  │  (polling,  │  │  tau_spawn, tau_sleep,  │  │
│  │   timers)   │  │   IO)       │  │  tau_block_on, etc.     │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
        ▲                   ▲                    ▲
        │                   │                    │
        └───────────────────┴────────────────────┘
                     Dynamic Linking
                           │
┌──────────────────────────┴──────────────────────────────────────┐
│                      Plugin (.cdylib)                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ Plugin Code │  │ tokio shim  │  │   tau interface crate   │  │
│  │  (reqwest,  │  │ (redirects  │  │  (types + FFI stubs)    │  │
│  │   kube...)  │  │  to tau)    │  │                         │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## How It Works

### Symbol Resolution

1. **Host exports symbols**: The host binary exports `#[no_mangle] extern "C"` functions like `tau_spawn`, `tau_sleep`, `tau_block_on`

2. **Plugins link dynamically**: Plugins are compiled with:
   - `-C prefer-dynamic` — use dynamic linking
   - `-Wl,-undefined,dynamic_lookup` — allow undefined symbols (resolved at load time)

3. **Runtime resolution**: When a plugin is loaded via `dlopen`, its undefined symbols are resolved against the host's exports

### The Patching Mechanism

This is the **core innovation** that makes Tau work. Plugins can use standard `tokio` APIs, but we redirect them to our runtime via Cargo's `[patch]` mechanism injected at compile time.

#### Problem
- Plugins want to use `tokio::spawn`, `tokio::time::sleep`, etc.
- Real tokio has its own runtime that conflicts with our shared executor
- We need plugins to call *our* runtime, not tokio's

#### Solution: Compiler-Injected Patches

When compiling a plugin, the compiler injects `--config` flags:

```bash
cargo build \
  --config 'patch.crates-io.tokio.path="/path/to/dist/src/tokio"'
```

This replaces the real `tokio` crate with our **tokio shim** — a crate that:
- Has `name = "tokio"` and `version = "1.99.0"` (higher than any real version)
- Exports the same public API as real tokio
- Internally calls `tau::spawn`, `tau::sleep`, etc.

#### Two Modes of Operation

**Development Mode** (workspace build):
```
crates/tau-tokio/Cargo.toml:
  tau = { path = "../tau" }

→ Both tau and tokio-shim compiled from source
→ Symbol hashes match automatically
```

**Distribution Mode** (relocatable dist/):
```
dist/src/tokio/Cargo.toml:
  # tau dependency REMOVED by xtask dist

→ tau provided via: --extern tau=/path/to/libtau.dylib
→ tokio-shim compiled from source, uses prebuilt tau
→ Symbol hashes match because same libtau.dylib
```

### Symbol Hash Matching

Rust mangles symbols with a hash derived from crate metadata. For dynamic linking to work, the hash must match between:
- `libtau.dylib` (in dist/lib/)
- Plugin code that calls tau functions
- tokio-shim that calls tau functions

We ensure this by:
1. Building `libtau.dylib` once during `cargo xtask dist`
2. Providing it to all plugin compilations via `--extern tau=/path/to/libtau.dylib`
3. Stripping `tau = { path = "..." }` from tokio-shim's Cargo.toml in dist

### RUSTFLAGS Propagation

The compiler captures build-time flags and propagates them to plugins:

```rust
// Captured at host build time (build.rs)
pub const HOST_RUSTFLAGS: &str = env!("HOST_RUSTFLAGS");
pub const HOST_PANIC: &str = env!("HOST_PANIC");
pub const HOST_TARGET_FEATURES: &str = env!("HOST_TARGET_FEATURES");

// Applied to plugin compilation
CARGO_ENCODED_RUSTFLAGS = [
    HOST_RUSTFLAGS,
    "-C prefer-dynamic",
    "-C link-args=-Wl,-undefined,dynamic_lookup",
    "-L /path/to/dist/lib",
    "--extern tau=/path/to/libtau.dylib",
    "-C panic=unwind",  // must match host
].join("\x1f")
```

## Directory Structure

### Workspace
```
tau-core/
├── crates/
│   ├── tau-host/       # Host binary with runtime
│   ├── tau/            # Interface crate (dylib + rlib)
│   └── tau-tokio/      # Tokio compatibility shim
├── plugins/            # Example and test plugins
├── tests/              # Integration test scripts
└── xtask/              # Dev tasks (dist, test)
```

### Distribution
```
dist/
├── bin/tau                    # Host executable
├── lib/
│   ├── libtau.dylib          # Prebuilt interface (for --extern)
│   └── libstd-*.dylib        # Bundled Rust stdlib
├── src/
│   ├── tau/                  # Source (for IDE analysis)
│   └── tokio/                # Source (compiled for each plugin)
└── run.sh                    # Runner script
```

### Build Cache
```
~/.tau/buildcache/
└── <hash>/                   # Hash of source directory path
    ├── target/               # Cargo target directory
    │   └── release/
    │       └── lib*.dylib
    └── plugins/              # Final plugin artifacts
        └── lib*.dylib
```

## Usage

```bash
# Build distribution
cargo xtask dist --release

# Run a plugin (compiles if needed)
./dist/run.sh --plugin plugins/example-plugin

# Show system info
./dist/run.sh --info

# Run tests
cargo xtask test
```

## Writing Plugins

Plugins are standard Rust crates with `crate-type = ["cdylib"]`:

```toml
# Cargo.toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[workspace]  # Prevent workspace inheritance

[dependencies]
# tau is provided via --extern, don't list it here
tokio = { version = "1", features = ["rt", "net", "time"] }
reqwest = "0.12"  # Works! Uses our tokio shim
```

```rust
// src/lib.rs
use tau::spawn;

#[repr(C)]
pub struct PluginHooks {
    pub process: unsafe extern "C" fn(*const u8, usize) -> u64,
}

#[no_mangle]
pub extern "C" fn plugin_init(hooks: *mut PluginHooks) -> i32 {
    unsafe { (*hooks).process = process; }
    0
}

unsafe extern "C" fn process(data: *const u8, len: usize) -> u64 {
    let handle = spawn(async {
        // Can use tokio APIs here - they're redirected to tau runtime
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        println!("Hello from plugin!");
    });
    handle.task_id()
}

#[no_mangle]
pub extern "C" fn plugin_destroy() {}
```

## Supported Tokio APIs

The tokio shim implements a subset of tokio's API:

| Module | Status | Notes |
|--------|--------|-------|
| `tokio::spawn` | ✅ | Full support |
| `tokio::time` | ✅ | `sleep`, `timeout`, `interval` |
| `tokio::sync` | ✅ | `mpsc`, `oneshot`, `Mutex`, `RwLock`, `Notify`, `Semaphore` |
| `tokio::net` | ✅ | `TcpStream`, `TcpListener`, `UdpSocket` |
| `tokio::io` | ✅ | `AsyncRead`, `AsyncWrite`, `BufReader`, `BufWriter` |
| `tokio::fs` | ✅ | Via `spawn_blocking` (thread pool) |
| `tokio::task` | ✅ | `spawn`, `spawn_blocking`, `JoinHandle`, `JoinSet` |

## Known Limitations

1. **Single-threaded only**: The runtime is single-threaded. `spawn_blocking` uses OS threads.

2. **macOS only** (currently): Uses kqueue via `polling` crate. Linux (epoll) should work but is untested.

3. **Nightly Rust**: Dynamic linking of Rust code requires matching rustc versions.

4. **No hot reload**: Plugins cannot be unloaded/reloaded (TLS cleanup issues).

5. ~~**JoinHandle requires `'static`**~~: Fixed! Like real tokio, our `JoinHandle<T>` now uses vtable-based type erasure with reference-counted task cells. The `T` is only stored via `PhantomData` and accessed through monomorphized vtable functions. This allows `kube-runtime` and other advanced crates to compile.

6. **Signal handling is stubbed**: `tokio::signal` module exists but signals are never delivered.

## Compatibility Testing

### Working Crates
- ✅ `reqwest` — HTTP client works end-to-end (TLS, async I/O)
- ✅ `tokio-util` — After adding missing APIs
- ✅ `kube` — Full kube stack including `kube-runtime` compiles and runs

### APIs Added During Testing
The following APIs were added to support real-world crates:
- `tokio::time::Timeout<F>` struct and `timeout_at()` function
- `tokio::time::Sleep::deadline()`, `is_elapsed()` methods
- `tokio::sync::Mutex` (async), `RwLock` (async)
- `tokio::sync::mpsc::Receiver::poll_recv()`
- `tokio::runtime::Handle`
- `tokio::signal` module (stubs)
- Blanket impls: `impl AsyncRead for &mut T`, `impl AsyncWrite for &mut T`, etc.

## Troubleshooting

### Symbol not found: `_tau_spawn`
- Plugin was compiled with different tau hash
- Solution: Clear build cache (`rm -rf ~/.tau/buildcache`) and recompile

### can't find crate for `tau`
- Missing `--extern tau=...` flag
- Check that `dist/lib/libtau.dylib` exists

### Duplicate lang item `panic_impl`
- Panic strategy mismatch between host and plugin
- Ensure both use same `-C panic=` setting
