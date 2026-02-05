# Migration Plan: ~/tau → ~/tau-core

## Architecture Comparison

### What tau-core does better (KEEP)

| Area | tau-core | tau (old) | Why tau-core wins |
|------|----------|-----------|-------------------|
| **Plugin linking** | `build.rs` + `#[link]` against host binary. Plugins link to the host executable itself — no separate dylib. | `tau-rt` is a separate `dylib` crate; plugins link to `libtau_rt.dylib` via `#[link(name = "tau_rt")]`. | tau-core's model is simpler: one binary, plugins link against it. No `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` headaches. The host IS the runtime. |
| **Executor** | `thread_local! { RefCell<Runtime> }` with explicit borrow discipline. `prepare_drive()` snapshots task IDs → poll without borrow → `mark_completed()` → `cleanup_completed()`. | `OnceLock<Executor>` + `Mutex<Slab<TaskSlot>>` + `async_task` crate. | tau-core's RefCell design enforces single-threaded invariant at compile time. No Mutex overhead. The snapshot-poll-cleanup cycle is correct for never holding borrow while calling plugin code. |
| **Plugin lifecycle** | `plugin_id` tracked on every resource (tasks, events, resources). `Runtime::unload_plugin()` cleans up all owned resources. | No plugin_id tracking. No cleanup on unload. | Critical for hot-reload and robustness. |
| **Resource system** | `resources.rs` — typed handles with `plugin_id`, host-side `HashMap<u64, ResourceSlot>`, plugin-side typed wrappers. | No equivalent. | tau-core has a proper ECS-lite resource system for cross-plugin shared state. |
| **Event system** | `events.rs` — typed event bus with subscriptions, plugin ownership tracking. | No equivalent (agent events are different — those are UI events). | Cross-plugin communication primitive. |
| **Compiler** | `compiler.rs` — in-process `rustc` invocation with correct `-L` / `--extern` flags pointing at host build artifacts. | `extension.rs` — out-of-process `cargo build` via rustup, generates full Cargo project with `[dependencies]`. | tau-core's compiler gives plugin code the exact same dependency graph as the host. tau's generates a separate project that may drift. |
| **Tokio shim** | Same binary, direct `extern "C"` calls to host runtime. Sync primitives (Mutex, Notify, Semaphore, etc.) are real implementations in the shim. | Delegates to `tau-io` → FFI to `libtau_rt.dylib`. Sync primitives in `tau-io/src/sync/`. | tau-core: zero indirection. Plugin calls `tokio::spawn` → shim calls `extern "C" tau_spawn()` → host function in same process. |

### What tau (old) has that tau-core needs (PORT)

These are features in `~/tau` that should be ported to `~/tau-core`'s architecture:

---

## Tier 1: Runtime primitives (port to `crates/tau` + `crates/tau-host`)

### 1. FfiStream\<T\> — FFI-safe async Stream
**Source**: `~/tau/crates/tau-io/src/ffi_stream.rs` (457 lines)
**Value**: Self-contained `repr(C)` stream type using function pointers. Implements `futures_core::Stream`. Panic catching via thread-local. `IntoFfiStream` trait.
**Port to**: `crates/tau/src/stream.rs`
**Adaptation needed**:
- Remove `async_ffi::ContextExt` / `FfiContext` usage — tau-core doesn't use `async-ffi`. Instead use raw `Context` directly (same binary, same compiler, no FFI boundary for the poll itself).
- Actually, since plugins ARE compiled by the same compiler and link against the same binary, `FfiStream<T>` might be simplified to just `Pin<Box<dyn Stream<Item = T> + Send>>` — no need for function pointer trampolines. The "FFI" boundary is really just a shared library call within the same process.
- **Decision point**: Do we need the `repr(C)` function-pointer approach, or can we use trait objects directly? Since tau-core guarantees same-compiler (the host compiles the plugins), **trait objects should work**. Consider keeping `FfiStream` only if ABI stability across compiler versions is needed.

### 2. Channels — FFI-safe bounded MPSC
**Source**: `~/tau/crates/tau-rt/src/channel.rs` (450 lines) + `~/tau/crates/tau-io/src/channel.rs` (508 lines)
**Value**: Ring-buffer MPSC channels with `poll_recv`, drop_fn for cleanup, typed `Sender<T>`/`Receiver<T>` wrappers, `FfiSafe` marker trait.
**Port to**: `crates/tau-host/src/runtime/channels.rs` (host side) + `crates/tau/src/channel.rs` (plugin side)
**Adaptation needed**:
- Host side: use `HashMap<u64, ChannelState>` pattern like `resources.rs` / `events.rs`, with `plugin_id` tracking.
- Plugin side: typed wrappers with `extern "C"` calls to host.
- Consider: do we actually need raw-byte ring-buffer channels? Since same compiler, could use `Arc<Mutex<VecDeque<T>>>` + waker. The raw-byte approach is only needed if T crosses a true ABI boundary.

### 3. Task abort
**Source**: Not implemented in either repo (both have placeholder `AbortHandle`).
**Value**: Essential for cancellation (drop task's future = cancel).
**Port to**: `crates/tau-host/src/runtime/executor.rs` — add `aborted: bool` to `Task`, drop future when aborted.
**Already designed**: See PRD.md US-001 through US-004.

### 4. AsyncRead / AsyncWrite / ReadBuf traits
**Source**: `~/tau/crates/tau-io/src/io_traits.rs` (304 lines)
**Value**: Mirror of tokio's traits, needed for TCP/IO stack.
**Port to**: `crates/tau/src/io.rs` (extend existing)
**Note**: tau-core's `crates/tau-tokio/src/io/mod.rs` already has extensive IO including ReadBuf. May just need the trait definitions in the `tau` crate for plugin use.

### 5. TCP / UDP / AsyncFd
**Source**: `~/tau/crates/tau-io/src/tcp.rs` (514 lines), `udp.rs` (172 lines), `async_fd.rs` (87 lines)
**Value**: Actual networking primitives using reactor.
**Port to**: `crates/tau/src/net.rs` or keep in `crates/tau-tokio/src/net/`
**Note**: tau-core already has a reactor with IO registration. These are the wrappers. May already be partially covered by the tokio shim's net module.

### 6. timeout() utility
**Source**: `~/tau/crates/tau-io/src/timeout.rs` (66 lines)
**Value**: `timeout(duration, future)` — races a future against a timer.
**Port to**: `crates/tau/src/runtime.rs` or `crates/tau-tokio/src/time/mod.rs`

---

## Tier 2: Agent layer (new crates in tau-core)

### 7. LLM types + streaming
**Source**: `~/tau/crates/tau-llm/` (3358 lines total)
- `types.rs` — Message, UserContent, AssistantContent, ToolCall, Usage, StopReason (417 lines)
- `stream.rs` — StreamEvent enum, StreamOptions, Provider trait (412 lines)
- `tool.rs` — Tool (with JSON schema from schemars), Context (196 lines)
- `partial_json.rs` — Incremental JSON parser for streaming tool args (312 lines)
- `providers/anthropic.rs` — Full Anthropic SSE streaming implementation (2007 lines)
**Port to**: `crates/tau-llm/` (new crate)
**Quality**: Very high — comprehensive types with serde roundtrip, proper SSE parsing, incremental JSON accumulator, PartialMode via jiter.
**Adaptation**: Minimal — these types are runtime-independent. Just need to make sure `reqwest` works (it already does in tau-core's http-plugin).

### 8. Agent state machine
**Source**: `~/tau/crates/tau-agent/` (4 files, ~2200 lines)
- `agent.rs` — Pure state machine: prompt → streaming → tool execution → next turn. Steering, follow-up queues. Abort via AtomicBool. (800 lines)
- `tool.rs` — AgentTool trait, ToolResult, streaming tool output (ToolStreamEvent). (180 lines)
- `events.rs` — AgentEvent enum for UI observation. (50 lines)
- `spawn.rs` — spawn_action() bridges Agent actions to async tasks. (200 lines)
**Port to**: `crates/tau-agent/` (new crate)
**Quality**: Excellent — the state machine is pure (no IO), fully tested (1800 lines of tests), clean separation of concerns. `AgentAction` tells the caller what to do, `AgentEvent` is what happened.
**Adaptation**:
- Replace `tau_io::spawn_detached` / `tau_io::FfiStream` with tau-core equivalents.
- `AtomicBool` cancellation is fine — works the same way.
- The `std::sync::mpsc::Sender<AgentEvent>` pattern in `spawn.rs` might switch to tau-core channels or just stay as-is (std channels work fine in same-process).

### 9. Auth system
**Source**: `~/tau/crates/tau-auth/` (1323 lines)
- `lib.rs` — ProviderAuth enum (ApiKey, OAuth), credential storage (463 lines)
- `oauth.rs` — OAuth flow implementation (471 lines)
- `providers/` — Anthropic and OpenAI specific auth (387 lines)
**Port to**: `crates/tau-auth/` (new crate)
**Adaptation**: None — pure application logic, no runtime dependency.

### 10. Extension ABI
**Source**: `~/tau/crates/tau-ext-abi/` (175 lines)
**Value**: C ABI for dynamically loaded tool extensions — `GetToolDefFn`, `ExecuteStreamingFn`, `PollStreamFn`, `DropStreamFn`, `FreeStringFn`.
**Port to**: Consider adapting to tau-core's existing plugin model.
**Decision**: tau-core already has `plugin_init` / `plugin_destroy` / `PluginHooks`. The extension ABI from tau adds **tool-specific** FFI (get_tool_def, execute_streaming). These are complementary — tau-core's plugin hooks are general, tau's extension ABI is specifically for agent tools.
**Adaptation**: Define tool extension hooks as part of tau-core's `PluginHooks` or as a separate extension protocol on top.

---

## Tier 3: UI / CLI (new crates, lowest priority)

### 11. TUI framework
**Source**: `~/tau/crates/tau-tui/` (5343 lines)
- Component trait, Container, Terminal abstraction
- Components: Input, Text, BoxComponent, SelectList, Spacer
- TUI event loop with crossterm integration
**Port to**: `crates/tau-tui/` (new crate)
**Adaptation**: Uses crossterm (tau-core already vendors crossterm).

### 12. CLI + Tools
**Source**: `~/tau/crates/tau-cli/` (4569 lines)
- Built-in tools: Bash (1071 lines), Read (410), Write (262), Edit (378)
- Interactive mode with full agent integration (909 lines)
- Extension compilation infrastructure (555 lines)
- Prompt / system prompt (89 lines)
**Port to**: `crates/tau-cli/` (new crate)
**Adaptation**:
- Tools are runtime-independent (process spawning, filesystem ops).
- Interactive mode needs TUI + Agent + Provider.
- Extension compilation: tau-core's `compiler.rs` is already better.

---

## Recommended Porting Order

```
Phase 1: Runtime primitives (in tau-core's existing crates)
  1.1  Task abort (PRD US-001–004)                    ← already designed
  1.2  Channels (tau-host + tau plugin-side)           ← from ~/tau
  1.3  FfiStream or simplified Stream wrapper          ← adapt from ~/tau
  1.4  timeout()                                       ← from ~/tau

Phase 2: LLM + Agent (new crates)
  2.1  tau-llm types (copy nearly verbatim)            ← from ~/tau
  2.2  tau-llm Anthropic provider                      ← from ~/tau
  2.3  tau-agent state machine                         ← from ~/tau
  2.4  tau-auth                                        ← from ~/tau

Phase 3: UI
  3.1  tau-tui                                         ← from ~/tau
  3.2  tau-cli + tools                                 ← from ~/tau
  3.3  Extension ABI for tools                         ← adapt from ~/tau
```

---

## Key Architecture Decision: FfiStream vs Trait Objects

tau-core's biggest advantage is **same-compiler linking**. This means:

| In ~/tau (old) | In ~/tau-core (new) |
|---|---|
| `FfiStream<T>` with `repr(C)` + function pointers | `Pin<Box<dyn Stream<Item = T>>>` — just a Rust trait object |
| `FfiFuture<T>` from `async-ffi` crate | Regular `Pin<Box<dyn Future<Output = T>>>` — or even `impl Future` |
| Raw-byte ring-buffer channels with `memcpy` | `Arc<Mutex<VecDeque<T>>>` + waker — or just `std::sync::mpsc` |
| `extern "C"` trampolines with panic catching | Normal Rust function calls through vtables |

**The entire `async-ffi` dependency and the `repr(C)` function-pointer pattern can be eliminated** because tau-core compiles plugins with the same compiler and links them against the same binary. Trait objects, closures, and generics all work across the plugin boundary.

**However**, if you ever want to support pre-compiled plugins (different compiler version), you'd need the FFI-safe approach back. This is a strategic decision.

### Recommendation
Keep `FfiStream<T>` as an option but make it unnecessary for the common case. Export both:
- `type Stream<T> = Pin<Box<dyn futures_core::Stream<Item = T> + Send>>` for same-compiler plugins
- `FfiStream<T>` (ported from ~/tau) for potential future ABI-stable plugins

---

## What NOT to port

| From ~/tau | Why skip |
|---|---|
| `tau-rt` crate (the dylib) | tau-core's "runtime in the host binary" model is superior |
| `tau-io/src/ffi.rs` (extern C declarations) | tau-core already has this via build.rs + #[link] |
| `async-ffi` dependency | Not needed — same compiler linking |
| `tau-tokio-util` crate | Should just work as-is if tau-tokio shim is complete enough |
| `tau-tokio-native-tls` crate | Should just work via tau-tokio shim |

---

## Line Count Summary

| What to port | Lines | Effort |
|---|---|---|
| Channels (host + plugin side) | ~960 | Medium (adapt to tau-core patterns) |
| FfiStream / Stream wrapper | ~460 | Low (simplify for same-compiler) |
| Task abort | ~100 | Low (already designed in PRD) |
| IO traits + timeout | ~370 | Low (may already exist in shim) |
| tau-llm (all) | ~3360 | Low (copy, minimal adaptation) |
| tau-agent (all) | ~2200 | Low (copy, swap spawn calls) |
| tau-auth (all) | ~1320 | None (copy verbatim) |
| tau-tui (all) | ~5340 | Low (copy, crossterm already vendored) |
| tau-cli (all) | ~4570 | Medium (integrate with tau-core tools) |
| **Total** | **~18,680** | |
