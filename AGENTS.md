# Agents — tau-core Development Guide

Instructions for AI agents (and humans) working on the tau-core codebase.

## Architecture Overview

```
crates/
  tau-rt/        # dylib — ONE shared copy. Runtime FFI: spawn, sleep, JoinHandle, resource, event, io
  tau/           # rlib  — compiled INTO each plugin. Per-plugin statics: PLUGIN_GUARD, define_plugin!
  tau-tokio/     # rlib  — tokio compatibility shim
  tau-host/      # binary — host runtime, plugin compiler, plugin loader
xtask/           # dev tasks: dist, test
plugins/         # example plugins (NOT workspace members — they have [workspace] in their Cargo.toml)
patches.list     # single source of truth for crate patches (used by compiler.rs AND xtask)
```

### Key Invariant: tau-rt is dylib, tau is rlib

| Crate    | crate-type | Why |
|----------|-----------|-----|
| `tau-rt` | `dylib`   | ONE copy shared by host + all plugins. Runtime symbols (`spawn`, `sleep`, etc.) must be the same instance. |
| `tau`    | `rlib`    | Compiled into each plugin separately. Each plugin gets its OWN copy of `static PLUGIN_GUARD: OnceLock<PluginGuard>`. |

This split is **load-bearing**. If `tau` were a dylib, all plugins would share a single `PLUGIN_GUARD` static, breaking per-plugin identity.

### Executor Lives in tau-host, NOT tau-rt

**This is intentional and load-bearing.**

The slab-based async executor implementation lives in `tau-host/src/runtime/executor.rs`, NOT in `tau-rt`. tau-rt contains only thin wrappers that call `extern "Rust"` functions provided by the host.

**Why?** When plugins compile against tau-rt (provided as a prebuilt dylib via `--extern`), Rust needs the rmeta metadata for ALL of tau-rt's dependencies. If tau-rt depended on `crossbeam-queue` (needed for lock-free cross-thread wakes), plugin compilation would fail:

```text
error[E0463]: can't find crate for `crossbeam_queue` which `tau_rt` depends on
```

**The solution:**
- `tau-rt` is **dependency-free** (only std) — plugins can compile against it
- `tau-host` contains the full executor with crossbeam-queue, statically linked
- tau-rt declares `extern "Rust" { fn tau_exec_*(...); }` functions
- tau-host exports these functions with `#[no_mangle] pub extern "Rust"`
- At runtime, plugin calls route through tau-rt → host

```text
┌─────────────────────────────────────────────────────────────┐
│                      Plugin (cdylib)                         │
│  spawn(), JoinHandle, sleep() — monomorphized from tau      │
└─────────────────────────────┬───────────────────────────────┘
                              │ calls
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    tau-rt (dylib)                            │
│  - Thin wrappers: executor::alloc_raw() → tau_exec_alloc_raw │
│  - NO external dependencies (only std)                      │
└──────────────────────────────┬──────────────────────────────┘
                               │ extern "Rust" calls
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                      tau-host (binary)                       │
│  - Full executor: slab, wakers, timers, crossbeam-queue     │
│  - Exports tau_exec_* functions                             │
└─────────────────────────────────────────────────────────────┘
```

**When modifying the executor:**
- Implementation changes go in `tau-host/src/runtime/executor.rs`
- If adding a new extern function:
  1. Add `#[no_mangle] pub extern "Rust" fn tau_exec_*` in tau-host
  2. Add `extern "Rust" { fn tau_exec_*; }` declaration in tau-rt
  3. Add to `ensure_executor_exports_linked()` to prevent linker stripping
  4. Add a thin wrapper function in tau-rt's executor.rs
- The host's `ensure_executor_exports_linked()` MUST be called from main() — otherwise the linker strips the tau_exec_* symbols as unused

### patches.list

`patches.list` at the workspace root is the single source of truth for which crates get patched into plugin builds. It's used by:
- `compiler.rs` via `include_str!()` — injected as `--config patch.crates-io.<name>.path="..."` when compiling plugins
- `xtask dist` — reads the file to know which source crates to copy to `dist/src/`

When adding a new patchable crate (e.g. `tau-llm`), add a line to `patches.list` and both systems pick it up automatically.

## Verification Checklist

**Run these checks after ANY change to the plugin infrastructure, crate split, linking, or distribution.**

### 1. Workspace builds clean

```bash
cargo build
cargo test --workspace
```

All tests must pass. Zero errors. Warnings are acceptable but should be tracked.

### 2. Distribution builds

```bash
cargo xtask dist
```

Verify the output:
- `dist/bin/tau` exists
- `dist/lib/libtau_rt.dylib` exists (NOT `libtau.dylib`)
- `dist/lib/libstd-*.dylib` exists
- `dist/src/` contains one directory per entry in `patches.list`
- **No** `libtau.dylib` anywhere in dist (tau is rlib, not dylib)

### 3. Plugin compiles and runs via dist

```bash
./dist/run.sh --plugin plugins/example-plugin
```

Expected output includes:
- `[Compiler] Using prebuilt tau-rt:` (tau-rt is the shared dylib)
- `[Compiler] Patching tau →` (tau is compiled from source as rlib)
- `[example-plugin] Initialized! plugin_id=1, guard refs=...`
- Async tasks execute: `Async task running!` / `Async task done!`
- Clean shutdown: `Destroyed!`

### 4. Link type verification

After building a plugin, verify with `otool -L`:

```bash
PLUGIN=$(find ~/.tau/buildcache -name "libexample_plugin.dylib" | head -1)
otool -L "$PLUGIN"
```

Must show:
- ✅ `@rpath/libtau_rt.dylib` — tau-rt linked as shared dylib
- ✅ `@rpath/libstd-*.dylib` — std linked dynamically
- ❌ Must NOT show `libtau.dylib` — tau is rlib (compiled in, not linked)

### 5. Per-plugin static verification

Each plugin must have its own `PLUGIN_GUARD` symbol:

```bash
nm "$PLUGIN" | grep PLUGIN_GUARD
```

Must show a `d` (data segment) entry. If you build two plugins, they must have this symbol at **different addresses** — proving each got its own rlib copy.

### 6. Two-plugin isolation test

```bash
./dist/run.sh --plugin plugins/example-plugin --plugin plugins/second-plugin
```

Verify:
- Plugin 1 reports `plugin_id=1`
- Plugin 2 reports `plugin_id=2`
- Each plugin's `PluginGuard::current()` returns its own ID, not the other's

### 7. patches.list sync check

Verify the compiler and xtask agree on patch entries:

```bash
# Compiler's baked-in view (test)
cargo test -p tau-host test_parse_patches -- --nocapture

# Xtask's runtime view (dist output)
cargo xtask dist 2>&1 | grep "src/"
```

Both must list the same set of crate names.

## Important: Always Run Plugins via `dist`

**Plugins must be compiled and run through the dist build, not directly via `cargo run`.**

```bash
cargo xtask dist
./dist/run.sh --plugin plugins/example-plugin
```

Running `cargo run -- --plugin ...` from the workspace will fail because the workspace `target/debug/` contains its own `libtau_rt.dylib`, which conflicts with the one built in the plugin build cache. The dist layout isolates `libtau_rt.dylib` into `dist/lib/` and provides it to plugins via `--extern`, avoiding duplicate dylib candidates.

## Common Pitfalls

### Adding a new patchable crate

1. Add to `patches.list`: `my-crate = crates/my-crate`
2. That's it — compiler.rs and xtask both read from `patches.list`
3. Run verification steps 1-3

### Moving code between tau and tau-rt

- If it needs ONE shared copy (runtime state, FFI trampolines) → `tau-rt`
- If it needs per-plugin copies (statics, macros, guard types) → `tau`
- After moving, run ALL verification steps. Especially step 4 (link types).

### Changing crate-type

**Never** change `tau` to `dylib` or `tau-rt` to `rlib` without understanding the implications:
- `tau` as dylib → all plugins share one `PLUGIN_GUARD` → broken plugin identity
- `tau-rt` as rlib → each plugin gets its own runtime state → broken task scheduling

### Plugin Cargo.toml requirements

Plugin crates must:
- Have `crate-type = ["cdylib"]`
- Have `[workspace]` (empty) to prevent cargo from treating them as workspace members
- Depend on `tau = { version = "0.1" }` (resolved via patch at build time)
- NOT declare their own `[patch.crates-io]` — the host compiler injects those

## Running the Full Verification Suite

```bash
# Quick: workspace + dist + plugin
cargo test --workspace && cargo xtask dist && ./dist/run.sh --plugin plugins/example-plugin

# Full: two-plugin isolation
./dist/run.sh --plugin plugins/example-plugin --plugin plugins/second-plugin
```

## Design Guidelines

### Same-compiler ABI invariant

All plugins are compiled by the same rustc, same target, same panic strategy, and same patched crate sources. This guarantees:

- **Identical memory layout** for all concrete Rust types (`String`, `Vec<T>`, structs, enums) across all plugins and the host.
- **Shared global allocator** — memory allocated in one plugin can be freed in another.
- **Monomorphized drop glue** — each plugin has its own copy of `Drop` impls for every type it uses. When plugin B drops a `String` that was created by plugin A, B's own `drop` glue runs. This is safe because the layout is identical.

This invariant is **load-bearing** for the entire plugin system. It means concrete data types can cross plugin boundaries freely — no serialization, no FFI wrappers, just Rust moves.

### No custom FFI primitives for things Rust already provides

**Do NOT build custom FFI infrastructure when standard Rust types work.**

Standard Rust synchronization and data types (`Arc`, `Mutex`, `VecDeque`, channels, etc.) work across plugin boundaries because of the same-compiler ABI invariant. There is no need to route data through host-side registries with opaque handles, function pointer callbacks, and manual cleanup.

**Bad — custom FFI channel:**
```
Host stores VecDeque<*mut ()> + drop_in_place_fn in a global HashMap
tau_stream_create() / tau_stream_push() / tau_stream_poll_next() FFI calls
Requires plugin_id tracking, drop_plugin_streams(), unload safety investigation
```

**Good — standard Rust types:**
```
Arc<Mutex<VecDeque<T>>> or async-channel or tokio::sync::mpsc
No FFI. No host-side state. No function pointers. No unload concerns.
Plugin A creates channel, Plugin B gets receiver via resource. Both use their
own monomorphized drop glue. Arc handles lifetime. Done.
```

The rule: if the data is a concrete Rust type (no trait objects, no fn pointers), it can cross plugin boundaries as-is. Only use FFI primitives for things that genuinely need host-side coordination (task scheduling, IO reactor, timer wheel).

### When FFI IS needed

FFI primitives (`#[no_mangle] pub extern "C"`) are needed when:

- **One shared instance is required** — the executor, reactor, and timer wheel must be singletons. Multiple copies would break scheduling. These live in `tau-rt` (dylib) or `tau-host`.
- **The host must be involved** — task spawning, IO registration, and plugin lifecycle need host-side state that persists across plugin loads/unloads.
- **Dynamic types cross boundaries** — trait objects (`dyn Stream`, `dyn Fn`) contain vtable pointers into a specific plugin's `.text` section. These must be wrapped in `PluginBox`/`PluginArc`/`PluginFn` to prevent use-after-unload.

### Plugin unload safety

On plugin unload, `AsyncPlugin::drop` runs this sequence **before** `dlclose`:

1. `plugin_destroy()` — plugin's voluntary cleanup
2. `drop_plugin_tasks(plugin_id)` — drops all task futures (runs destructors)
3. `drop_plugin_events(plugin_id)` — drops event subscription callbacks
4. `drop_plugin_resources(plugin_id)` — drops resources via stored `drop_fn`
5. `dlclose()` — unmaps the plugin binary

**Concrete data** (`String`, `Vec`, `Arc<Mutex<...>>`) that has been moved to another plugin or the host survives unload safely — the surviving holder's own drop glue handles cleanup.

**Dynamic types** (trait objects, closures, fn pointers) that point into the unloaded plugin's `.text` are UB after `dlclose`. Always wrap these in `PluginBox`/`PluginArc`/`PluginFn` so the `PluginGuard` keeps the binary alive.

**Function pointers stored in host-side registries** (e.g., `drop_fn` in resources, event callbacks) are cleaned up by the `drop_plugin_*` functions before `dlclose`. Any new host-side registry that stores function pointers MUST have a corresponding `drop_plugin_*` cleanup function and be added to the unload sequence.

### Prefer ecosystem crates over custom implementations

When a well-maintained ecosystem crate solves the problem, use it instead of writing a custom implementation. Examples:

- **Channels**: `async-channel`, `tokio::sync::mpsc` (via tau-tokio shim), `flume` — not custom FFI channels
- **Stream trait**: `futures-core::Stream` re-export — not a custom trait
- **Stream combinators**: evaluate `tokio-stream`, `futures-util` — only write custom combinators if the ecosystem crate can't compile against the tau shim
- **Synchronization**: `std::sync`, `parking_lot` — not custom FFI mutexes

These crates compile into each plugin as rlibs. The same-compiler invariant means their internal data structures have identical layout across plugins. No special handling needed.
