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
