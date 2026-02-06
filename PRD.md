# PRD: Core Infrastructure Primitives

## Introduction

Tau-core is a shared async runtime with dynamic plugin infrastructure. Plugins are separately-compiled `.cdylib` crates that share a single-threaded async executor with the host via dynamic linking, with a tokio compatibility shim that redirects standard `tokio::*` APIs to the tau runtime.

Several foundational primitives are missing from the runtime:

1. **Task abort** — `JoinHandle::abort()` is currently a no-op. Dropping or aborting a task should actually cancel it: drop the future, clean up timers, and notify waiters. This is how Rust async cancellation works — by dropping futures.

2. **Async streams** — there is no `Stream` trait or stream combinators. The runtime supports `Future` (poll once → result) but not `Stream` (poll repeatedly → sequence of items). Streams are the building block for LLM token streaming, event pipelines, transform chains, and reactive data flow. No custom FFI channel is needed — standard Rust channel crates (the tokio mpsc shim, `async-channel`, etc.) work across plugins because all plugins share the same compiler ABI and global allocator.

3. **AsyncFd** — tau-rt has raw IO reactor FFI (`tau_io_register`, `tau_io_poll_ready`, `tau_io_deregister`) but no safe `AsyncFd` wrapper. Plugins and vendored crates (crossterm) need a safe, `std::task::Context`-based async fd type.

4. **Crossterm without mio** — The `crossterm` crate depends on `mio` + `signal-hook-mio` for event polling. Since tau-core already has a reactor (backed by `polling` crate), we vendor crossterm and replace its mio-based event source with one using tau's IO primitives. This removes the mio dependency and unifies all IO through the tau reactor.

5. **Fast plugin recompilation check** — `compile_plugin()` always invokes `cargo build`, even when nothing has changed (~265ms overhead for no-op). Cargo emits `.d` dep files (Makefile format) listing every source file that contributed to the artifact. By parsing the dep file and checking mtimes before invoking cargo, we can skip the build entirely when no sources have changed (~13ms).

All primitives must integrate with the existing tokio shim so that crates like `reqwest`, `kube`, `tokio-stream`, and `futures` work transparently when compiled against the shim.

## Goals

- Make `JoinHandle::abort()` actually cancel the associated task (drop future, wake waiters with `JoinError`)
- Make `AbortHandle::abort()` work the same way (decoupled from JoinHandle lifetime)
- Ensure cancelled tasks' futures are dropped during the same drive cycle (not deferred)
- Re-export `futures_core::Stream` in the `tau` crate — the standard async iterator trait, compatible with the entire ecosystem
- Implement core stream combinators: `map`, `filter`, `filter_map`, `take_while`, `merge`, and async `then`/`async_map`
- Provide a `tokio-stream`-compatible shim (same Cargo `[patch]` mechanism as the tokio shim) so crates depending on `tokio-stream` compile against our implementation
- Channels use standard Rust types (`Arc<Mutex<VecDeque<T>>>`) — no custom FFI channel infrastructure. The existing `tokio::sync::mpsc` shim and ecosystem crates like `async-channel` already work across plugins thanks to the same-compiler ABI invariant.
- Add `AsyncFd` to `tau-rt` as the safe wrapper for the IO reactor
- Vendor crossterm with mio replaced by tau's reactor primitives, patched into plugins via `patches.list`
- Support both sync (`crossterm::event::poll`/`read`) and async (`EventStream`) event APIs
- Maintain all existing tokio shim compatibility — `reqwest`, `kube`, `tokio-util` must continue to compile and work
- Restructure build cache: separate environment hash (compiler + flags + tau-rt) from plugin identity, enable dep-file fast path to skip cargo entirely when sources are unchanged
- All changes pass `cargo build` for the workspace and `cargo xtask test`

## User Stories

### Phase 1: Task Abort

> **Reference: tokio abort semantics we are emulating**
>
> In tokio, `JoinHandle::abort()` works as follows:
>
> 1. **Abort marks the task, it doesn't kill it instantly.** The task's future is dropped at the *next* `.await` point — not mid-execution. If the task is currently being polled, abort sets a flag; the runtime drops the future after the current poll returns. If the task is idle (waiting on a waker), it is dropped immediately.
> 2. **The future's `Drop` impl runs.** This is how cleanup happens — dropping the future drops all locals held across await points (files, locks, buffers, child tasks). This is Rust's cancellation model.
> 3. **`JoinHandle` resolves to `Err(JoinError)`.** The error has `is_cancelled() == true`. If the task already completed (or panicked) before abort is called, abort is a no-op — the `JoinHandle` yields the original result.
> 4. **Dropping a `JoinHandle` does NOT abort the task.** The task becomes "detached" and keeps running. Only an explicit `.abort()` or `AbortHandle::abort()` cancels it.
> 5. **`AbortHandle` is a detached cancellation handle.** It can be cloned, sent to other tasks, and used to abort without owning the `JoinHandle`. Obtained via `JoinHandle::abort_handle()`.
> 6. **`JoinSet::abort_all()`** calls `.abort()` on every tracked task. `JoinSet::shutdown()` aborts all and then awaits completion of each (drains the set).
> 7. **Abort is idempotent.** Calling `.abort()` on an already-aborted or already-finished task is a no-op.
>
> Our implementation must match these semantics. The key difference from tokio is that we are single-threaded, so "currently being polled" means the abort call happens from within the same task's poll (re-entrant abort) or from a different task's poll in the same drive cycle.

---

### US-001: Add `tau_task_abort` FFI export to the host executor [x]

**Description:** As a plugin developer, I want to abort a spawned task so that its future is dropped and resources are freed.

> **🔍 Research before implementing:**
> - Read `crates/tau-host/src/runtime/executor.rs` — understand `Task` struct, `ready_queue`, `drive()` loop, `cleanup_completed()`
> - Read tokio's task harness: https://docs.rs/tokio/latest/src/tokio/runtime/task/harness.rs.html — see how `remote_abort()` sets the CANCELLED flag, how the scheduler drops the future
> - Read tokio's `OwnedTasks::close_and_shutdown_all()` for bulk abort semantics
> - Key concern: what if `abort()` is called while the task is currently being polled? In tokio, the task is NOT dropped mid-poll — the abort flag is checked after `poll()` returns. Our single-threaded runtime has the same constraint: if task A's poll calls `task_B.abort()`, task B's future must not be dropped until after A's poll returns and the drive loop checks B.

**Acceptance Criteria:**
- [x] Add `aborted: bool` flag to the `Task` struct in `crates/tau-host/src/runtime/executor.rs`
- [x] Add `pub fn abort_task(&mut self, task_id: u64) -> bool` method on `Runtime` that sets the `aborted` flag and returns whether the task existed
- [x] Add `#[no_mangle] pub extern "C" fn tau_task_abort(task_id: u64) -> u8` in `crates/tau-host/src/runtime/mod.rs` that calls `rt.borrow_mut().abort_task(task_id)`, returns 1 if found, 0 if not
- [x] Aborted tasks are removed from `ready_queue` immediately
- [x] Aborted tasks' futures are dropped (the existing `Task::drop` calls `(drop_fn)(future_ptr)`) during the next `cleanup_completed()` or immediately
- [x] Associated timers for aborted tasks are cleaned up
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests (`cargo xtask test`) still pass

---

### US-002: Wire abort through `tau` crate and `JoinHandle` [x]

**Description:** As a plugin developer, I want `JoinHandle::abort()` to actually cancel the task so I get Rust-standard cancellation semantics.

> **🔍 Research before implementing:**
> - Read `crates/tau/src/runtime.rs` — current `JoinHandle`, `TaskCell`, `Stage` enum, `WrapperFuture`
> - Read `crates/tau-rt/src/runtime.rs` — FFI declarations (`tau_spawn`, `tau_task_poll`, etc.)
> - Read tokio `JoinHandle` API: https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html — note that `.await`ing an aborted handle returns `Err(JoinError)`, and `JoinError` has `is_cancelled()`, `is_panic()`, `into_panic()`
> - Read tokio `JoinError`: https://docs.rs/tokio/latest/tokio/task/struct.JoinError.html — understand the three states: completed, cancelled, panicked
> - Key: dropping a `JoinHandle` does NOT abort — it detaches the task. Only explicit `.abort()` cancels.

**Acceptance Criteria:**
- [x] Add `extern "C" { fn tau_task_abort(task_id: u64) -> u8; }` declaration in `crates/tau/src/runtime.rs`
- [x] `JoinHandle::abort(&self)` calls `tau_task_abort(self.task_id)`
- [x] When an aborted task's `WrapperFuture` is dropped, it marks the `TaskCell` as complete with an "aborted" state (new `Stage::Aborted` variant)
- [x] `JoinHandle` polling a task in `Stage::Aborted` returns `Poll::Ready` with a value that the tokio shim can convert to `Err(JoinError)` with `is_cancelled() == true`
- [x] `JoinHandle::is_finished()` returns `true` for aborted tasks
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-003: Wire abort through tokio shim `JoinHandle` and `AbortHandle` [x]

**Description:** As a user of tokio APIs, I want `tokio::JoinHandle::abort()` and `AbortHandle::abort()` to cancel tasks so that crates using tokio's cancellation patterns work correctly.

> **🔍 Research before implementing:**
> - Read `crates/tau-tokio/src/task/mod.rs` — current `JoinHandle`, `AbortHandle`, `JoinSet`, `JoinError` stubs
> - Read `crates/tau-tokio/src/lib.rs` — `spawn()`, how it wraps `tau::spawn()`
> - Read tokio `AbortHandle`: https://docs.rs/tokio/latest/tokio/task/struct.AbortHandle.html — `abort()`, `is_finished()`. Note it's `Clone + Send + Sync`.
> - Read tokio `JoinSet`: https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html — `abort_all()`, `shutdown()` (async, aborts + awaits all), `join_next()` returns `Option<Result<T, JoinError>>`, `detach_all()`
> - Key: `JoinSet::shutdown()` is an async method that calls `abort_all()` then loops `join_next().await` until `None`
> - Key: `JoinSet::join_next()` must handle interleaved completed/aborted tasks — cancelled tasks yield `Err(JoinError)` just like completed ones yield `Ok(T)`

**Acceptance Criteria:**
- [x] `crates/tau-tokio/src/lib.rs` `JoinHandle::abort()` delegates to `self.inner.abort()` (already does, now it works)
- [x] `AbortHandle` in `crates/tau-tokio/src/task/mod.rs` stores the `task_id` (currently it's a phantom marker)
- [x] `AbortHandle::abort()` calls `tau_task_abort(task_id)` via a new FFI function or through the `tau` crate
- [x] `AbortHandle::is_finished()` calls through to check task state
- [x] `JoinHandle` future impl returns `Err(JoinError)` when the task was aborted, with `JoinError::is_cancelled() == true`
- [x] `JoinSet::abort_all()` actually aborts all tasks (it calls `handle.abort()` which now works)
- [x] `JoinSet::shutdown()` aborts and then drains all handles
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-004: Abort integration test [x]

**Description:** As a developer, I want a test that proves task abort works end-to-end across the FFI boundary.

> **🔍 Research before implementing:**
> - Read existing test plugins `plugins/example-plugin/` and `plugins/second-plugin/` for the plugin structure pattern
> - Read `crates/tau-host/src/main.rs` — how plugins are loaded, how requests are dispatched
> - Read tokio's own abort test: https://github.com/tokio-rs/tokio/blob/master/tokio/tests/task_abort.rs — covers: abort before first poll, abort during sleep, abort already-completed task, `JoinError::is_cancelled()`
> - Test should cover: (1) abort idle task → immediate drop, (2) abort already-completed → no-op, (3) `Drop` impl runs on cancel (put a print/side-effect in the Drop), (4) `JoinHandle.await` → `JoinError::is_cancelled()`

**Acceptance Criteria:**
- [x] Create `plugins/abort-test-plugin/` with a plugin that:
  - Spawns a long-running task (e.g., loops with `tau::sleep(1s).await`)
  - Stores the `JoinHandle` in a resource
  - On request "abort": calls `handle.abort()` on the stored handle
  - On request "check": reports whether the handle is finished
- [x] Add a test script in `tests/` that: loads the plugin, sends "spawn", sends "abort", sends "check", verifies the task is finished
- [x] Test passes via `cargo xtask test` or manual invocation
- [x] The aborted task's future destructor actually runs (verify with a print in a Drop impl)
- [x] `cargo build` succeeds for the workspace

---

### US-004a: Make `tau_task_abort` work from background threads (cross-thread abort) [x]

**Description:** `AbortHandle::abort()` silently fails when called from a `spawn_blocking` thread because `tau_task_abort` accesses the thread-local `RUNTIME`, which is empty on background threads. Since `AbortHandle` is `Clone + Send + Sync` (it only holds a `u64`), users reasonably expect it to work from any thread — matching tokio's `AbortHandle` semantics.

**Root cause:** `tau_task_abort` in `crates/tau-host/src/runtime/mod.rs` calls `RUNTIME.with(|rt| rt.borrow_mut().abort_task(task_id))`, which accesses a thread-local `RefCell<Runtime>`. Background threads get an empty default `Runtime`.

**Fix:** Follow the existing `WAKE_QUEUE` pattern — add a global `ABORT_QUEUE: OnceLock<Mutex<VecDeque<u64>>>`. `tau_task_abort` pushes to this queue and calls `try_notify_reactor()`. `prepare_drive()` drains the queue and calls `abort_task()` for each entry. This is the same pattern the wake system uses and keeps the actual abort logic on the main thread.

**Acceptance Criteria:**
- [x] Add `static ABORT_QUEUE: OnceLock<Mutex<VecDeque<u64>>>` in `executor.rs` (same pattern as `WAKE_QUEUE`)
- [x] `tau_task_abort` pushes `task_id` to `ABORT_QUEUE` and calls `try_notify_reactor()` (instead of accessing thread-local RUNTIME directly)
- [x] `prepare_drive()` drains `ABORT_QUEUE` and calls `self.abort_task(id)` for each entry
- [x] `tau_task_abort` return value: since the abort is now async (queued), always return 1 ("accepted"). Callers should not rely on the return value to know if the task existed — use `tau_task_is_finished` instead.
- [x] Add a test case in `abort-test-plugin`: spawn a task, pass its `task_id` to `spawn_blocking`, abort from the background thread, verify the task is cancelled
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-REVIEW-PHASE1: Review Task Abort (US-001 through US-004) [x]

**Description:** Review US-001 through US-004 as a cohesive system.

**Acceptance Criteria:**
- [ ] Identify phase scope: US-001 to US-004
- [ ] Run: `git log --oneline --all | grep -E "US-00[1-4]"`
- [ ] Review all phase code files together
- [ ] Evaluate quality:
  - Good taste: Simple and elegant across all tasks?
  - No special cases: Edge cases handled through design?
  - Data structures: Consistent and appropriate?
  - Complexity: Can anything be simplified?
  - Duplication: Any repeated logic BETWEEN tasks?
  - Integration: Do components work together cleanly?
- [ ] Cross-task analysis:
  - Verify abort during active polling doesn't cause double-free (future dropped by abort AND by poll completion)
  - Verify abort from `spawn_blocking` thread works (wake queue → reactor notify)
  - Verify `plugin_task_count` and `drop_plugin_tasks` still work correctly with aborted tasks
  - Verify re-entrancy: aborting a task from within another task's poll doesn't panic the RefCell
  - Check that `JoinSet::join_next()` correctly handles a mix of completed and aborted tasks
- [ ] If issues found:
  - Insert fix tasks after the failing task (US-004a, US-004b, etc.)
  - Append review findings to progress.txt
  - Do NOT mark this review task [x]
- [ ] If no issues:
  - Append "## Phase 1 review PASSED" to progress.txt
  - Mark this review task [x]
  - Commit: "docs: phase 1 review complete"

---

### Phase 1b: Compiler — Build Cache & Dep-File Fast Path

---

### US-COMP-001: Restructure build cache with environment hash [x]

**Description:** The current build cache layout hashes everything together (source dir + compiler version + tau-rt dylib content) into a single flat directory per plugin. This means: tau-rt is re-read and hashed on every build, compiler/env changes create N orphaned directories (one per plugin), and there's no way to garbage-collect stale caches.

Restructure the cache into a two-level hierarchy: **environment hash** → **plugin directory**.

> **🔍 Research before implementing:**
> - Read `crates/tau-host/src/compiler.rs` `get_build_cache_dir()` — the current hashing logic
> - Read `crates/tau-host/src/compiler.rs` `compile_plugin()` — how the cache dir is used
> - Key: the environment hash should be computed ONCE at `Compiler::resolve()` time and stored on the `Compiler` struct. Individual `compile_plugin()` calls reuse it.

**New cache layout:**

```
~/.tau/buildcache/
  <env_hash>/                           # hash(compiler_version + rustflags + panic + target_features + tau_rt_content + patches_list)
    example-plugin.<src_hash>/          # crate name + short hash of canonicalized source path
      target/                           # cargo target dir
      plugins/                          # final artifact copies
    abort-test-plugin.<src_hash>/
      target/
      plugins/
```

**What goes into the environment hash:**
- `Compiler::HOST_VERSION` (rustc version string)
- `Compiler::HOST_RUSTFLAGS`
- `Compiler::HOST_PANIC`
- `Compiler::HOST_TARGET_FEATURES`
- Content of `libtau_rt.dylib` (symbol hashes determine ABI — rebuilding tau-rt must invalidate all plugins)
- The raw text of `patches.list` (which crates are patched and their relative paths)

**What keys a plugin within the environment dir:**
- Crate name (parsed from `Cargo.toml`) + short hash of the canonicalized source path (to disambiguate same-name plugins from different locations)

**Acceptance Criteria:**
- [x] Compute `env_hash` once in `Compiler::resolve()` (or a new `Compiler::init_cache()` method), store on the struct
- [x] `get_build_cache_dir()` returns `~/.tau/buildcache/<env_hash>/<crate_name>.<src_hash>/`
- [x] Plugin crate name is parsed from `Cargo.toml` for the directory name (human-readable)
- [x] Old flat cache dirs (`~/.tau/buildcache/<single_hash>/`) are ignored (backward compat — they'll be cleaned up manually or by a future GC story)
- [x] When the environment changes (new compiler, rebuilt tau-rt, changed patches), all plugins get fresh build dirs automatically
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass
- [x] Manually verify: rebuild tau-rt → `cargo xtask dist` → `./dist/run.sh --plugin plugins/example-plugin` creates a new `<env_hash>/` dir

---

### US-COMP-002: Dep-file fast path — skip cargo when sources unchanged [ ]

**Description:** `compile_plugin()` always invokes `cargo build`, even when nothing has changed. A no-op cargo build takes ~265ms (manifest parsing, lockfile resolution, patch injection, fingerprinting). Cargo emits `.d` dep files in Makefile format listing every source file that contributed to the final artifact. By parsing the dep file and checking mtimes, we can skip cargo entirely when no sources have changed (~13ms).

> **🔍 Research before implementing:**
> - Read the dep file format: `target/debug/lib<crate>.d` contains one line: `<output>: <dep1> <dep2> ...` — Makefile syntax, space-separated
> - Examine existing dep files: `~/.tau/buildcache/*/target/debug/lib*.d` — verify the format, check that ALL transitive source deps are listed (plugin sources + patched crate sources + registry crate sources)
> - Key: the dep file only covers source files. It does NOT list `libtau_rt.dylib` — that's already handled by the environment hash (US-COMP-001). If the env hash changes, we get a fresh build dir with no dep file, so cargo runs.
> - Key: if the dep file or the artifact doesn't exist, fall through to cargo (first build).
> - Key: if any listed source file is missing (deleted), fall through to cargo.
> - Edge case: `Cargo.toml` and `Cargo.lock` changes (new dependency added) are NOT in the dep file. Consider also checking `Cargo.toml` mtime against the artifact.

**Acceptance Criteria:**
- [ ] Add a method `check_up_to_date(&self, cache_dir: &Path, source_dir: &Path) -> Option<PathBuf>` to `Compiler` that:
  1. Finds the final artifact in `<cache_dir>/plugins/lib<crate>.dylib`
  2. Finds the dep file at `<cache_dir>/target/<profile>/lib<crate>.d`
  3. If either doesn't exist → return `None` (needs build)
  4. Parses the dep file: extract all dependency paths after the `:` separator
  5. Stats the artifact mtime
  6. Also checks `<source_dir>/Cargo.toml` mtime against the artifact (catches dependency changes not in the dep file)
  7. Stats every listed dependency — if ANY is newer than the artifact, or missing → return `None`
  8. If all deps are older → return `Some(artifact_path)` (skip cargo)
- [ ] `compile_plugin()` calls `check_up_to_date()` before invoking cargo. If it returns `Some(path)`, print `[Compiler] Plugin up-to-date (skipped): <path>` and return immediately
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass
- [ ] Manually verify: `./dist/run.sh --plugin plugins/example-plugin` twice — second run prints "up-to-date" and is measurably faster

---

### Phase 2: Stream Trait

> **Reference: futures-core Stream trait**
>
> The standard `Stream` trait lives in `futures-core` (not `futures` — that's a mega-crate re-exporting it):
> ```rust
> pub trait Stream {
>     type Item;
>     fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
>     fn size_hint(&self) -> (usize, Option<usize>) { (0, None) }
> }
> ```
> - Docs: https://docs.rs/futures-core/latest/futures_core/stream/trait.Stream.html
> - `futures-core` is a minimal crate (~no deps) that ONLY defines traits. Every async crate depends on it (`hyper`, `tower`, `tonic`, `kube`, `reqwest` via `hyper`).
> - `tokio-stream` re-exports `futures_core::Stream` and provides `StreamExt` (combinators + adapters).
> - `futures` re-exports `futures_core::Stream` and provides its own `StreamExt` (slightly different set of combinators).
> - Key decision: we re-export the SAME trait from `futures-core`, so there's zero impedance mismatch with the ecosystem. We do NOT define our own Stream trait.
>
> **Why no custom FFI channel:**
>
> The existing `tokio::sync::mpsc` shim (`Arc<Mutex<VecDeque<T>>>`) and ecosystem crates like `async-channel` already work across plugins. Thanks to the same-compiler ABI invariant, `Arc`, `Mutex`, `VecDeque`, and any concrete `T` have identical layout in all plugins. They share the global allocator. When one plugin drops the last `Arc` reference, its own monomorphized drop glue runs — no function pointers stored in host memory, no `plugin_id` tracking, no cleanup on unload. Channels are just Rust types. The `Stream` trait is just the interface receivers implement.

---

### US-005: Re-export `futures_core::Stream` in the `tau` crate [x]

**Description:** As a plugin developer, I want a `Stream` trait available through the `tau` crate so I can produce and consume asynchronous sequences of values, using the standard `futures_core::Stream` trait that the ecosystem already depends on.

**Acceptance Criteria:**
- [x] Add `futures-core = "0.3"` to `crates/tau/Cargo.toml` dependencies
- [x] Create `crates/tau/src/stream.rs`
- [x] Re-export the trait: `pub use futures_core::Stream;`
- [x] Add `pub mod stream;` to `crates/tau/src/lib.rs`
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-005a: Remove custom FFI stream channel infrastructure [x]

**Description:** Commits `8847b5d` and `980fe4d` added a custom FFI stream channel system (`tau_stream_*` exports, host-side `StreamState` ring buffer registry, `StreamSender<T>`/`StreamReceiver<T>` wrappers) based on an old US-006 task that was removed from the PRD. This directly violates the PRD's design principle: "No custom FFI primitives for things Rust already provides." Standard Rust channel crates (`tokio::sync::mpsc` via the shim, `async-channel`, `flume`) already work across plugins thanks to the same-compiler ABI invariant. The custom FFI infrastructure must be removed.

**Acceptance Criteria:**
- [x] Remove `crates/tau-host/src/runtime/streams.rs` entirely (host-side `StreamState`, ring buffer, all `tau_stream_*` FFI exports)
- [x] Remove `pub mod streams;` from `crates/tau-host/src/runtime/mod.rs`
- [x] Revert `crates/tau/src/stream.rs` to ONLY the `futures_core::Stream` re-export (remove all FFI declarations, `StreamSender<T>`, `StreamReceiver<T>`, `channel()`, `PushResult`, `SendFuture`, `make_ffi_waker`)
- [x] Verify no other files reference `tau_stream_*` or `StreamSender`/`StreamReceiver`
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-REVIEW-PHASE2: Review Stream Core (US-005) [x]

**Description:** Review US-005 as a cohesive system.

**Acceptance Criteria:**
- [ ] Identify phase scope: US-005
- [ ] Review all phase code files together
- [ ] Evaluate quality:
  - Good taste: Simple and elegant?
  - Confirm `tau::stream::Stream` is a re-export of `futures_core::Stream` (not a separate trait)
  - Confirm no custom FFI channel infrastructure remains (no `tau_stream_*` FFI, no host-side `StreamState`)
  - Verify `futures-core` resolves to one version across tau + plugin deps
- [ ] If issues found:
  - Insert fix tasks after the failing task (US-008a, US-008b, etc.)
  - Append review findings to progress.txt
  - Do NOT mark this review task [x]
- [ ] If no issues:
  - Append "## Phase 2 review PASSED" to progress.txt
  - Mark this review task [x]
  - Commit: "docs: phase 2 review complete"

---

### Phase 3: Stream Combinators

> **Reference: existing combinator implementations to study**
>
> - `futures-rs` StreamExt source: https://github.com/rust-lang/futures-rs/tree/master/futures-util/src/stream — canonical implementations of all combinators (`map.rs`, `filter.rs`, `filter_map.rs`, `then.rs`, `take_while.rs`, `fold.rs`, `for_each.rs`, `next.rs`, `collect.rs`)
> - `tokio-stream` StreamExt source: https://github.com/tokio-rs/tokio/tree/master/tokio-stream/src — slightly different set, includes `merge`, `StreamMap`
> - Our `StreamExt` should have the same method signatures as `futures::StreamExt` where they overlap. This way plugins that import both won't get ambiguity errors (Rust resolves to the same-named method if signatures match).
> - Pin projection: use `pin-project-lite` or manual `unsafe` pin projections. `futures-rs` uses `pin_project!` macro from `pin-project-lite`. Read: https://docs.rs/pin-project-lite/latest/pin_project_lite/

---

### US-009: Synchronous combinators — `map`, `filter`, `filter_map`, `take_while` [x]

**Description:** As a plugin developer, I want to transform and filter streams so I can build data pipelines without manual poll loops.

> **🔍 Research before implementing:**
> - Read `futures-util` source for each combinator struct: https://github.com/rust-lang/futures-rs/blob/master/futures-util/src/stream/stream/map.rs — see how `Map<St, F>` uses `pin_project!`, stores stream + closure, delegates `poll_next`
> - Read the `filter.rs` source — note it loops in `poll_next` to skip non-matching items (doesn't return `Pending` on filter miss — it re-polls the source)
> - Read `take_while.rs` — note it transitions to a "done" state once the predicate returns false, then always returns `None`
> - Key: each combinator struct must implement `Stream`. The `StreamExt` trait just provides the constructor method. Keep structs in separate files or one `combinators.rs`.

**Acceptance Criteria:**
- [x] Create `crates/tau/src/stream/combinators.rs` (or keep in `stream.rs` if small enough)
- [x] `StreamExt` trait with default method implementations (extension trait on `futures_core::Stream`):
  - `fn map<F, B>(self, f: F) -> Map<Self, F>` where `F: FnMut(Self::Item) -> B`
  - `fn filter<F>(self, f: F) -> Filter<Self, F>` where `F: FnMut(&Self::Item) -> bool`
  - `fn filter_map<F, B>(self, f: F) -> FilterMap<Self, F>` where `F: FnMut(Self::Item) -> Option<B>`
  - `fn take_while<F>(self, f: F) -> TakeWhile<Self, F>` where `F: FnMut(&Self::Item) -> bool`
- [x] Each combinator is a struct implementing `Stream`, with correct `Pin` projections
- [x] All combinators are `Send` when the underlying stream and closure are `Send`
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-010: Async combinator — `then` (async map) [x]

**Description:** As a plugin developer, I want an async transform on streams so I can do async work (HTTP calls, DB queries, LLM calls) per stream item.

> **🔍 Research before implementing:**
> - Read `futures-util` `then.rs`: https://github.com/rust-lang/futures-rs/blob/master/futures-util/src/stream/stream/then.rs — the two-state machine: either waiting for the source stream or waiting for the in-flight future
> - Note the difference between `then` (async map, always produces a value) and `filter_map` (sync, may skip). `then` is the async equivalent of `map`.
> - Key subtlety: the in-flight future must be pinned. `futures-rs` uses `#[pin] future: Option<Fut>` inside a `pin_project!` struct. When the future completes, it's set to `None` and the source stream is polled for the next item.
> - Also study `and_then` in futures-rs — similar but for `Stream<Item = Result<T, E>>` where the async closure returns `Result`. We may want this later for error chains.

**Acceptance Criteria:**
- [x] Add `fn then<F, Fut>(self, f: F) -> Then<Self, F, Fut>` to `StreamExt` where `F: FnMut(Self::Item) -> Fut`, `Fut: Future`
- [x] `Then` struct holds the source stream and an `Option<Fut>` for the in-flight future
- [x] Polling: if a future is in flight, poll it; if ready yield the result and poll the source for the next item; if source yields, create a new future via `f(item)` and poll it
- [x] Only one item is in-flight at a time (sequential async processing, not concurrent)
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-011: Utility combinators — `next`, `collect`, `for_each`, `fold` [x]

**Description:** As a plugin developer, I want convenience methods for consuming streams so I can await the next item or collect all items.

> **🔍 Research before implementing:**
> - Read `futures-util` `next.rs`: https://github.com/rust-lang/futures-rs/blob/master/futures-util/src/stream/stream/next.rs — `Next` is a future that borrows `&mut St`, polls it once. Note: requires `St: Unpin` because it takes `&mut self`.
> - Read `collect.rs`: https://github.com/rust-lang/futures-rs/blob/master/futures-util/src/stream/stream/collect.rs — takes ownership of stream, polls until `None`, extends the collection
> - Read `fold.rs`, `for_each.rs` — same consume-until-None pattern
> - Key: these are "terminal" operations — they consume the stream. They're implemented as futures (not streams). The `StreamExt` methods return these futures, which the caller `.await`s.
> - Key: `next()` takes `&mut self` (borrows, doesn't consume). `collect/fold/for_each` take `self` (consume).

**Acceptance Criteria:**
- [x] Add to `StreamExt`:
  - `async fn next(&mut self) -> Option<Self::Item>` where `Self: Unpin` — await the next item
  - `async fn collect<C: Default + Extend<Self::Item>>(self) -> C` where `Self: Sized` — collect all items into a container
  - `async fn for_each<F>(self, f: F)` where `F: FnMut(Self::Item)`, `Self: Sized` — consume all items
  - `async fn fold<B, F>(self, init: B, f: F) -> B` where `F: FnMut(B, Self::Item) -> B`, `Self: Sized`
- [x] Each is implemented as a future that polls the stream internally
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-012: `merge` — combine two streams into one [x]

**Description:** As a plugin developer, I want to merge multiple streams so I can process items from different sources in arrival order.

> **🔍 Research before implementing:**
> - Read `tokio-stream` `merge.rs`: https://github.com/tokio-rs/tokio/blob/master/tokio-stream/src/stream_ext/merge.rs — note the `flag: bool` that alternates which stream is polled first for fairness
> - Read `futures-rs` `select.rs` (their name for merge): https://github.com/rust-lang/futures-rs/blob/master/futures-util/src/stream/select.rs — same alternation approach, uses `FusedStream` to track which streams are done
> - Key: `Merge` must handle one stream finishing before the other — it should continue yielding from the remaining stream until both are done, then return `None`.
> - Key: the fairness toggle prevents starvation: poll A first on even calls, B first on odd calls (or vice versa).

**Acceptance Criteria:**
- [x] Add `fn merge<S2>(self, other: S2) -> Merge<Self, S2>` to `StreamExt` where `S2: Stream<Item = Self::Item>`
- [x] `Merge` polls both inner streams fairly (alternate which is polled first to avoid starvation)
- [x] Yields items from whichever stream is ready
- [x] Completes only when both streams are exhausted
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-013: Stream combinator tests [x]

**Description:** As a developer, I want tests for all combinators to verify correctness.

**Acceptance Criteria:**
- [x] Create `plugins/stream-combinator-test-plugin/` (or extend `stream-test-plugin`) that tests:
  - `map`: transform items, verify output sequence
  - `filter`: drop items, verify only matching items pass through
  - `filter_map`: combined filter+transform
  - `take_while`: early termination
  - `then`: async transform with sleep, verify ordering preserved
  - `next`: consume one item
  - `collect`: gather into `Vec`
  - `merge`: two producers, verify all items received
- [x] Test is runnable via `cargo xtask test` or manual invocation
- [x] All sub-tests pass
- [x] `cargo build` succeeds for the workspace

---

### US-REVIEW-PHASE3: Review Stream Combinators (US-009 through US-013) [x]

**Description:** Review US-009 through US-013 as a cohesive system.

**Acceptance Criteria:**
- [ ] Identify phase scope: US-009 to US-013
- [ ] Run: `git log --oneline --all | grep -E "US-0(09|1[0-3])"`
- [ ] Review all phase code files together
- [ ] Evaluate quality:
  - Good taste: Simple and elegant across all tasks?
  - No special cases: Edge cases handled through design?
  - Data structures: Consistent and appropriate?
  - Complexity: Can anything be simplified?
  - Duplication: Any repeated logic BETWEEN tasks?
  - Integration: Do components work together cleanly?
- [ ] Cross-task analysis:
  - Verify `Pin` projection safety in all combinator structs (no unsound `get_unchecked_mut` usage)
  - Verify `then` correctly handles the case where the inner future is dropped mid-execution (abort scenario)
  - Verify `merge` fairness — neither side starves the other
  - Verify all combinators propagate `None` (stream termination) correctly
  - Check that `StreamExt` doesn't conflict with `futures::StreamExt` if both are in scope (method names should match)
- [ ] If issues found:
  - Insert fix tasks after the failing task (US-013a, US-013b, etc.)
  - Append review findings to progress.txt
  - Do NOT mark this review task [x]
- [ ] If no issues:
  - Append "## Phase 3 review PASSED" to progress.txt
  - Mark this review task [x]
  - Commit: "docs: phase 3 review complete"

---

### Phase 4: Tokio-Stream Shim

> **Reference: tokio-stream crate structure**
>
> `tokio-stream` (https://docs.rs/tokio-stream/latest/tokio_stream/) is a separate crate from `tokio` that provides:
> - Re-export of `futures_core::Stream`
> - `StreamExt` trait with combinators (`next`, `map`, `filter`, `merge`, `timeout`, `throttle`, etc.)
> - Adapter types: `ReceiverStream` (wraps `mpsc::Receiver`), `UnboundedReceiverStream`, `BroadcastStream`, `WatchStream`, `SignalStream`
> - `StreamMap` — a keyed collection of streams, polls all, yields `(K, V)` pairs
> - `wrappers` module with `IntervalStream`, `TcpListenerStream`, etc.
>
> Key feature flags: `sync` (mpsc/watch/broadcast adapters), `time` (interval/timeout), `net` (tcp/unix listeners), `signal`, `io-util`, `fs`.
>
> Our approach: rather than shimming `tokio-stream`, we test that the **real** `tokio-stream` crate compiles against our **tokio shim** (tau-tokio). Since `tokio-stream` depends on `tokio`, and our `patches.list` replaces `tokio` with `tau-tokio`, `tokio-stream` will build against our shim. We only need to add any missing API surface in `tau-tokio` that `tokio-stream` requires.

---

### US-014: Verify real `tokio-stream` compiles against our tokio shim [x]

**Description:** As a plugin developer, I want the real `tokio-stream` crate to compile against our tokio shim so that crates depending on `tokio-stream` work transparently — no separate shim crate needed.

> **🔍 Research before implementing:**
> - Read `tokio-stream` Cargo.toml for its tokio dependency: https://github.com/tokio-rs/tokio/blob/master/tokio-stream/Cargo.toml — note which tokio features it requires (`sync`, `time`, etc.)
> - Read `crates/tau-tokio/src/lib.rs` and `crates/tau-tokio/Cargo.toml` — what features/modules we currently expose
> - Read `tokio-stream/src/wrappers/mpsc_bounded.rs`: https://github.com/tokio-rs/tokio/blob/master/tokio-stream/src/wrappers/mpsc_bounded.rs — `ReceiverStream` wraps `tokio::sync::mpsc::Receiver` and implements `Stream`. Check if our `mpsc::Receiver` has all the methods it calls.
> - Key: `tokio-stream` may use `tokio::sync::mpsc::Receiver::poll_recv()` — verify our shim has this method.
> - Key: if `tokio-stream` default features pull in modules we don't support (e.g., `signal`), the test plugin should disable those features.

**Acceptance Criteria:**
- [x] Create a test plugin that depends on `tokio-stream = "0.1"` (the real crate) and uses `ReceiverStream`, `StreamExt`
- [x] Compile the test plugin using the existing `--config 'patch.crates-io.tokio.path=...'` mechanism (our tokio shim)
- [x] If compilation fails due to missing APIs in `tau-tokio`: add the missing APIs to the tokio shim (e.g., `broadcast` channel if a default feature needs it, or additional methods on existing types)
- [x] If `tokio-stream` pulls in feature-gated tokio modules we don't support: configure the test plugin's `tokio-stream` dependency with only the features that work (e.g., `default-features = false, features = ["sync", "time"]`)
- [x] The test plugin can: create an `mpsc` channel, wrap receiver in `ReceiverStream`, use `.next()`, `.map()`, `.filter()` from `tokio_stream::StreamExt`
- [x] Document which `tokio-stream` features work and which don't (if any) in a comment in the test plugin
- [x] `cargo build` succeeds for the workspace
- [x] Existing tests still pass

---

### US-015: Verify `futures-core` resolves correctly in plugin builds [ ]

**Description:** As a developer, I want to verify that `futures-core` (used by `tau` and by downstream crates like `hyper`, `tower`, `kube-runtime`) resolves to a single copy during plugin compilation, so there are no duplicate `Stream` trait conflicts.

> **🔍 Research before implementing:**
> - Run `cargo tree -p <plugin> -i futures-core` to see how many copies resolve. Should be exactly 1.
> - Read about Cargo version resolution: https://doc.rust-lang.org/cargo/reference/resolver.html — semver-compatible versions (0.3.x) unify to one copy. If tau pins `futures-core = "0.3.30"` and hyper wants `"0.3.28"`, Cargo picks the latest compatible (`0.3.30`). No conflict.
> - Key risk: if someone depends on `futures-core = "0.4"` (doesn't exist yet, but hypothetically) that would be a separate copy with a different `Stream` trait. Not a real concern today.
> - Verification: compile a plugin that depends on `reqwest` (which pulls in `hyper` → `futures-core`) and also uses `tau::stream::Stream`. The `Stream` trait from both must be the same type.

**Acceptance Criteria:**
- [ ] Verify that when a plugin depends on both `tau` (via `--extern`) and a crate that pulls in `futures-core` (e.g., `reqwest`, `kube`), Cargo resolves to one `futures-core` version
- [ ] If there's a version conflict (tau pins `futures-core 0.3.x`, downstream wants `0.3.y`): relax the version bound in `tau/Cargo.toml` to `"0.3"`
- [ ] `http-plugin` (reqwest) still compiles and loads
- [ ] `kube-plugin` still compiles and loads
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-016: End-to-end compatibility test with ecosystem crates [ ]

**Description:** As a developer, I want to verify the stream shims don't break existing plugin compilation.

**Acceptance Criteria:**
- [ ] `cargo xtask test` passes (all existing plugins compile and tests pass)
- [ ] `kube-plugin` compiles with `kube = { features = ["runtime"] }` which pulls in `kube-runtime` (uses streams internally)
- [ ] `http-plugin` (reqwest) still compiles and runs
- [ ] `tokio-plugin` still compiles and runs
- [ ] If `kube-runtime` now uses our stream trait: verify a `watcher()` or `watch()` call compiles (even if it fails at runtime due to no cluster)
- [ ] `cargo build` succeeds for the workspace

---

### US-REVIEW-PHASE4: Review Tokio-Stream Shim (US-014 through US-016) [ ]

**Description:** Review US-014 through US-016 as a cohesive system.

**Acceptance Criteria:**
- [ ] Identify phase scope: US-014 to US-016
- [ ] Run: `git log --oneline --all | grep -E "US-01[4-6]"`
- [ ] Review all phase code files together
- [ ] Evaluate quality:
  - Good taste: Simple and elegant across all tasks?
  - No special cases: Edge cases handled through design?
  - Data structures: Consistent and appropriate?
  - Complexity: Can anything be simplified?
  - Duplication: Any repeated logic BETWEEN tasks?
  - Integration: Do components work together cleanly?
- [ ] Cross-task analysis:
  - Verify the `tokio-stream` shim version (0.1.99) wins over real `tokio-stream` in Cargo resolution
  - Verify `ReceiverStream` correctly bridges `mpsc::Receiver` → `Stream`
  - Verify patch injection in `compiler.rs` includes both `tokio` and `tokio-stream` patches
  - Verify dist layout includes `tokio-stream` shim source
  - Verify `futures-core` resolves to one version across tau + plugin deps (no duplicate trait definitions)
- [ ] If issues found:
  - Insert fix tasks after the failing task (US-016a, US-016b, etc.)
  - Append review findings to progress.txt
  - Do NOT mark this review task [x]
- [ ] If no issues:
  - Append "## Phase 4 review PASSED" to progress.txt
  - Mark this review task [x]
  - Commit: "docs: phase 4 review complete"

---

### Phase 5: AsyncFd & Crossterm Vendor

---

### US-017: Add `AsyncFd` to `tau-rt` [ ]

**Description:** As a plugin developer, I want a safe `AsyncFd` type that wraps a raw file descriptor and provides async readability/writability polling through the tau reactor, so I don't have to manually juggle `FfiWaker` and raw FFI calls.

> **🔍 Research before implementing:**
> - Read tokio `AsyncFd`: https://docs.rs/tokio/latest/tokio/io/unix/struct.AsyncFd.html — wraps a `RawFd`, provides `readable()` / `writable()` returning `AsyncFdReadyGuard` that must be used to clear readiness. Note the guard-based API: `let guard = fd.readable().await?; guard.try_io(|inner| inner.read(buf))`. We may simplify this since we're single-threaded.
> - Read tokio `AsyncFd` source: https://github.com/tokio-rs/tokio/blob/master/tokio/src/io/async_fd.rs — how it registers with the reactor, stores interest, handles edge-triggered vs level-triggered readiness
> - Read our existing IO FFI: `crates/tau-rt/src/io.rs` — `tau_io_register`, `tau_io_poll_ready`, `tau_io_clear_ready`, `tau_io_deregister`, `FfiWaker` struct
> - Read old tau fork's `AsyncFd`: `~/tau/crates/tau-io/src/async_fd.rs` — simpler than tokio's, uses `FfiWaker` directly
> - Read `crates/tau-tokio/src/net/mod.rs` — see how `TcpStream` currently manually does the register/poll_ready/clear_ready dance. `AsyncFd` should encapsulate this.
> - Key: our `AsyncFd` is simpler than tokio's — no guard, no edge-triggered concerns (the `polling` crate handles re-arming). Just `poll_read_ready(cx) -> Poll<()>` and `clear_read_ready()`.
> - Key: `AsyncFd` does NOT own the fd. Caller opens/closes the fd. `AsyncFd::drop` only deregisters from reactor.

**Acceptance Criteria:**
- [ ] Create `AsyncFd` struct in `crates/tau-rt/src/io.rs` (extend existing file):
  - `AsyncFd::new(fd: RawFd) -> io::Result<Self>` — registers fd for READABLE|WRITABLE with the reactor
  - `AsyncFd::with_interest(fd: RawFd, interest: u8) -> io::Result<Self>` — registers with specific interest flags
  - `AsyncFd::poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>` — polls for readability using `tau_io_poll_ready` with a waker extracted from `cx`
  - `AsyncFd::poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>` — polls for writability
  - `AsyncFd::clear_read_ready(&self)` — calls `tau_io_clear_ready` for the read direction
  - `AsyncFd::clear_write_ready(&self)` — calls `tau_io_clear_ready` for the write direction
  - `AsyncFd::readable(&self) -> impl Future` — async wrapper around `poll_read_ready`
  - `AsyncFd::writable(&self) -> impl Future` — async wrapper around `poll_write_ready`
  - `Drop` calls `tau_io_deregister`
  - `AsyncFd::as_raw_fd(&self) -> RawFd`
  - `AsyncFd::handle(&self) -> u64` — returns the reactor handle
- [ ] `AsyncFd` does NOT own the fd (caller manages fd lifetime)
- [ ] Helper: `fn ffi_waker_from_cx(cx: &mut Context<'_>) -> FfiWaker` — converts a std Waker to FfiWaker (clone waker, box it, set wake_fn)
- [ ] Re-export `AsyncFd` from `crates/tau-rt/src/lib.rs`
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-018: Add `make_ffi_waker` utility and refactor tau-tokio to use it [ ]

**Description:** As a developer, I want a single `make_ffi_waker(cx)` utility so the duplicated waker-boxing pattern in tau-tokio/net is eliminated. This utility lives in `tau-rt` and is used by `AsyncFd`, the tokio shim, and the crossterm vendor.

> **🔍 Research before implementing:**
> - Read `crates/tau-tokio/src/net/mod.rs` — search for `make_ffi_waker` or the waker boxing pattern (clone waker → box → FfiWaker). Count how many copies exist.
> - Read `crates/tau-rt/src/io.rs` — the `FfiWaker` struct definition: `{ data: *mut (), wake_fn: Option<extern "C" fn(*mut ())> }`
> - Read `std::task::Waker` docs: https://doc.rust-lang.org/std/task/struct.Waker.html — `clone()`, `wake()`, `wake_by_ref()`. We need `clone()` to take ownership, then `Box::into_raw` to get a `*mut ()`.
> - Key: the `wake_fn` must call `Box::from_raw` to reclaim the `Waker`, then call `waker.wake()`. This frees the box. If the reactor replaces a stored `FfiWaker` with a new one (on re-poll), the old waker's box leaks unless the reactor explicitly drops it. Check `crates/tau-host/src/runtime/reactor.rs` to see if it drops replaced wakers.

**Acceptance Criteria:**
- [ ] Add `pub fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker` to `crates/tau-rt/src/io.rs`
  - Clones the waker from `cx`, boxes it, returns `FfiWaker { data, wake_fn }`
  - The `wake_fn` unboxes and calls `waker.wake()`
- [ ] Refactor all `make_ffi_waker` implementations in `crates/tau-tokio/src/net/mod.rs` (TcpStream, OwnedReadHalf, OwnedWriteHalf, UdpSocket, ConnectFuture — currently 5 identical copies) to use `tau::io::make_ffi_waker(cx)` instead
- [ ] `AsyncFd::poll_read_ready` and `poll_write_ready` use this utility internally
- [ ] `cargo build` succeeds for the workspace (zero warnings from removed duplicates)
- [ ] Existing tests still pass

---

### US-019: Vendor crossterm — initial fork with features trimmed [ ]

**Description:** As a developer, I want a vendored copy of crossterm 0.28 in the workspace with mio-related features removed, so we can replace the event system with tau's reactor.

> **🔍 Research before implementing:**
> - Read crossterm Cargo.toml (upstream): https://github.com/crossterm-rs/crossterm/blob/master/Cargo.toml — understand the feature graph: `events` pulls in `mio` + `signal-hook` + `signal-hook-mio`, `event-stream` adds `futures-core` + `async-trait` (or tokio)
> - Read our old vendor fork: `~/tau/vendor/crossterm/Cargo.toml` — see what was already changed
> - Read `crossterm/src/lib.rs` — what modules are `cfg`-gated behind features
> - Key: crossterm's non-event functionality (cursor, style, terminal control, execute/queue macros) has ZERO dependency on mio. Only `src/event/` touches mio. So stripping mio only affects the event system.
> - Key: the `event-stream` feature in upstream crossterm depends on `tokio`. Our fork replaces that with `tau` + `futures-core`.

**Acceptance Criteria:**
- [ ] Copy crossterm 0.28 source to `vendor/crossterm/` (from crates.io or the old tau fork at `~/tau/vendor/crossterm/`)
- [ ] Edit `vendor/crossterm/Cargo.toml`:
  - Remove `mio`, `signal-hook`, `signal-hook-mio` from `[dependencies]`
  - Remove the `events` feature (which pulls in mio)
  - Change `default` feature to `["bracketed-paste"]` (no `events`, no `windows`)
  - Add `tau = { version = "0.1" }` dependency
  - Add `futures-core = { version = "0.3", optional = true }` dependency
  - Change `event-stream` feature to `["dep:futures-core"]`
  - Keep `use-dev-tty` and `bracketed-paste` features as-is
- [ ] Add `crossterm = vendor/crossterm` to `patches.list`
- [ ] The crate compiles with `default-features = false, features = ["bracketed-paste"]` (no event system yet — that comes in the next story)
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-020: Replace mio event source with tau reactor (sync poll/read) [ ]

**Description:** As a crossterm user, I want `crossterm::event::poll()` and `crossterm::event::read()` to work using tau's reactor instead of mio, so sync terminal event reading works without mio.

> **🔍 Research before implementing:**
> - Read existing mio event source: `~/tau/vendor/crossterm/src/event/source/unix/mio.rs` (already read above) — the `UnixInternalEventSource` pattern: registers tty + signals with mio `Poll`, reads bytes in a loop, parses via `Parser`
> - Read crossterm `EventSource` trait: search for `trait EventSource` in `~/tau/vendor/crossterm/src/event/source/` — `try_read(&mut self, timeout) -> io::Result<Option<InternalEvent>>` and optionally `waker()`
> - Read crossterm's `poll()` and `read()` public API: `~/tau/vendor/crossterm/src/event/read.rs` or `mod.rs` — they create an `InternalEventReader` that calls `EventSource::try_read` in a loop
> - Read crossterm's SIGWINCH handling in the mio source — it uses `signal-hook-mio` `Signals` struct. Our replacement uses a self-pipe: `pipe()` → signal handler writes a byte → read end registered with reactor.
> - Key: the sync source needs to actually block. Since we can't use `mio::Poll` anymore, options: (1) use `polling::Poller` directly, (2) use `tau_block_on` with an async wrapper. Option 1 is simpler — create a `polling::Poller`, register the tty fd and sigwinch pipe, call `poller.wait(events, timeout)`.
> - Read `polling` crate docs: https://docs.rs/polling/latest/polling/ — `Poller::new()`, `unsafe Poller::add(fd, Event, PollMode)`, `Poller::wait(&self, events, timeout)`

**Acceptance Criteria:**
- [ ] Create `vendor/crossterm/src/event/source/unix/tau.rs` replacing `mio.rs`:
  - `UnixInternalEventSource` struct holds: reactor handle for tty fd, reactor handle for SIGWINCH self-pipe read end, Parser, tty FileDesc, read buffer
  - Constructor: opens tty, sets non-blocking, registers with reactor via `tau::io::register()`. Creates SIGWINCH self-pipe, registers read end with reactor. Installs signal handler.
  - `try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>>`: uses `polling::Poller` (or `tau::block_on` / `tau::drive` with timeout) to wait for readiness on tty or sigwinch pipe, then reads and parses
- [ ] Update `vendor/crossterm/src/event/source/unix.rs` to use `tau` module instead of `mio` module (the `cfg` dispatch)
- [ ] Replace mio-based `Waker` in `vendor/crossterm/src/event/sys/unix/waker/` with a tau reactor notify-based implementation (calls `tau_reactor_notify`)
- [ ] `crossterm::event::poll(Duration)` works — blocks up to timeout, returns `Ok(true)` if event available
- [ ] `crossterm::event::read()` works — blocks until an event is available, returns it
- [ ] SIGWINCH produces `Event::Resize` events
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-021: Implement async `EventStream` using `AsyncFd` [ ]

**Description:** As a plugin developer, I want `crossterm::event::EventStream` to implement `futures_core::Stream` using `AsyncFd` for tty and SIGWINCH polling, so I can await terminal events in async code.

> **🔍 Research before implementing:**
> - Read old tau fork's stream.rs: `~/tau/vendor/crossterm/src/event/stream.rs` — already implements `Stream for EventStream` using `tau-io::AsyncFd`. This is our direct reference.
> - Read upstream crossterm's stream.rs: https://github.com/crossterm-rs/crossterm/blob/master/src/event/stream.rs — uses `tokio::io::unix::AsyncFd` for the tokio backend, or `async-std` equivalent. Note the pattern: wake up on any IO, try_read, if WouldBlock re-register.
> - Read `futures_core::Stream` trait (above) — `poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>`
> - Key: `EventStream::poll_next` must check BOTH the sigwinch pipe and the tty. If either is readable, attempt to read. If SIGWINCH pipe has data, drain it and emit `Event::Resize`. If tty has data, read bytes and parse. If neither is ready, register wakers for both via `AsyncFd::poll_read_ready(cx)`.
> - Key: if the parser has buffered events from a previous read (multi-byte escape sequences can produce multiple events from one read), return them immediately without polling fds.
> - Key subtlety: `poll_read_ready` returns `Poll::Ready(())` when ready. After reading, call `clear_read_ready()` so the next poll re-checks. If the read got `WouldBlock`, clear readiness and return `Pending`.

**Acceptance Criteria:**
- [ ] Create `vendor/crossterm/src/event/stream.rs` (replaces the old mio/tokio-based stream):
  - `EventStream` struct holds: `AsyncFd` for tty, `AsyncFd` for SIGWINCH pipe read end, Parser, tty FileDesc, read buffer, raw SIGWINCH pipe fd
  - Constructor: same setup as sync source (tty, self-pipe, signal handler) but using `tau::io::AsyncFd`
  - `EventStream::new() -> Self`
  - `EventStream::next(&mut self) -> Option<io::Result<Event>>` — convenience async method
- [ ] Implement `Stream for EventStream`:
  - `poll_next`: check parser buffer → poll SIGWINCH `AsyncFd` for read → poll tty `AsyncFd` for read → parse bytes → return event or `Pending`
  - Wakers registered via `AsyncFd::poll_read_ready(cx)` — standard `Context`, no `async-ffi` / `FfiContext` needed
- [ ] `Drop` cleans up: close self-pipe, restore SIGWINCH to SIG_DFL, AsyncFd deregisters from reactor
- [ ] Gated behind `#[cfg(feature = "event-stream")]`
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-022: Crossterm integration test — sync and async events [ ]

**Description:** As a developer, I want a test plugin that proves crossterm's sync and async event APIs work through the tau reactor.

> **🔍 Research before implementing:**
> - Read existing test plugins `plugins/example-plugin/`, `plugins/second-plugin/` for the pattern
> - Read crossterm's public API: https://docs.rs/crossterm/latest/crossterm/ — `terminal::enable_raw_mode()`, `terminal::disable_raw_mode()`, `terminal::size()`, `event::poll(Duration)`, `event::read()`, `event::EventStream`
> - Read how `patches.list` works: the plugin's `Cargo.toml` depends on `crossterm = "0.28"`, the compiler injects `--config 'patch.crates-io.crossterm.path=...'` pointing to our vendor fork
> - Key: the test doesn't need to verify every key event — just that (1) the plugin compiles, (2) `terminal::size()` returns a valid result, (3) `EventStream` can be created without panicking, (4) the reactor correctly wakes on stdin readability

**Acceptance Criteria:**
- [ ] Create `plugins/crossterm-test-plugin/` that depends on `crossterm = { version = "0.28", features = ["event-stream"] }`
- [ ] Plugin init: enables raw mode, hides cursor
- [ ] Plugin destroy: disables raw mode, shows cursor
- [ ] On request "poll <ms>": calls `crossterm::event::poll(Duration::from_millis(ms))`, reports result
- [ ] On request "read": calls `crossterm::event::read()`, prints the event
- [ ] On request "stream": spawns a task that creates an `EventStream`, reads 5 events via `.next().await`, prints them
- [ ] On request "size": calls `crossterm::terminal::size()`, prints result
- [ ] Plugin compiles via `./dist/run.sh --plugin plugins/crossterm-test-plugin`
- [ ] Basic smoke test: load plugin, send "size", verify response
- [ ] `cargo build` succeeds for the workspace
- [ ] Existing tests still pass

---

### US-REVIEW-PHASE5: Review AsyncFd & Crossterm (US-017 through US-022) [ ]

**Description:** Review US-017 through US-022 as a cohesive system.

**Acceptance Criteria:**
- [ ] Identify phase scope: US-017 to US-022
- [ ] Run: `git log --oneline --all | grep -E "US-0(1[7-9]|2[0-2])"`
- [ ] Review all phase code files together
- [ ] Evaluate quality:
  - Good taste: Simple and elegant across all tasks?
  - No special cases: Edge cases handled through design?
  - Data structures: Consistent and appropriate?
  - Complexity: Can anything be simplified?
  - Duplication: Any repeated logic BETWEEN tasks?
  - Integration: Do components work together cleanly?
- [ ] Cross-task analysis:
  - Verify `AsyncFd` waker lifecycle: waker boxed once per poll, freed on wake or on next poll. No leaks.
  - Verify crossterm's `EventStream` correctly registers wakers for BOTH tty and SIGWINCH fds — a key event shouldn't be missed because only one fd's waker fires
  - Verify SIGWINCH self-pipe doesn't leak fds on drop
  - Verify sync `poll()`/`read()` works when called from within `tau::block_on` (no reactor deadlock — the sync source should use `polling` directly or tau's drive loop)
  - Verify the vendored crossterm patches cleanly into plugin builds (patches.list entry works, no version conflicts)
  - Verify `make_ffi_waker` doesn't double-free: if the reactor calls `wake_fn` AND the next poll creates a new waker, the old one must be properly freed
- [ ] If issues found:
  - Insert fix tasks after the failing task (US-022a, US-022b, etc.)
  - Append review findings to progress.txt
  - Do NOT mark this review task [x]
- [ ] If no issues:
  - Append "## Phase 5 review PASSED" to progress.txt
  - Mark this review task [x]
  - Commit: "docs: phase 5 review complete"

---

## Non-Goals

- **TUI framework** — tau-tui component system, layout engine, differential rendering are a separate PRD built on top of the crossterm shim
- **Conversation tree / session model** — out of scope, will be a separate `tau-agent` / `taugent` crate later
- **Agent loop orchestration** — out of scope, built on top of these primitives later
- **Tool schema registry** — out of scope, depends on a redesigned plugin interface
- **Plugin interface redesign** — out of scope; the `define_plugin!` / `request(&[u8]) -> u64` hook is unchanged
- **Lifecycle hook system** — out of scope, will be layered on streams later
- **Multi-threaded runtime** — the runtime remains single-threaded; `spawn_blocking` uses OS threads
- **Hot reload** — plugin unload/reload is not addressed
- **Backpressure across FFI for existing event bus** — `tau::event` remains as-is (fire-and-forget pub/sub)
- **Windows support for crossterm vendor** — macOS and Linux only; crossterm's Windows code is untouched but untested
- **Mouse/paste events in crossterm** — bracketed paste is kept; mouse support depends on escape sequence parsing which is inherited unchanged from upstream

## Technical Considerations

### Existing Architecture Constraints

- **RefCell borrow discipline:** The executor uses `thread_local! { RefCell<Runtime> }`. Any new host-side state must follow the same rule: never hold the borrow while calling plugin code.

- **Tokio shim patching mechanism:** New shim crates (`tokio-stream`, optionally `futures-core`) use the same `--config 'patch.crates-io.<name>.path=...'` injection in `compiler.rs`. The dist layout needs matching `dist/src/<name>/` directories.

- **Symbol hash matching:** New shim crates must participate in the same symbol hash matching scheme. In dev mode, they're workspace members with `path` dependencies. In dist mode, they use `--extern tau=...` to match the prebuilt `libtau.dylib`.

- **No custom FFI for channels:** Channels (`tokio::sync::mpsc`, `async-channel`, etc.) are standard Rust types compiled into each plugin. They use `Arc<Mutex<VecDeque<T>>>` or similar — no host-side state, no FFI, no `plugin_id` tracking. The same-compiler ABI invariant guarantees they work across plugin boundaries for concrete types. See `AGENTS.md` "Design Guidelines" for the full rationale.

### Performance Considerations

- The `Stream` trait and combinators live in the `tau` crate (plugin side) — they're monomorphized per-plugin with zero FFI overhead.
- Channels are standard Rust types (e.g., `Arc<Mutex<VecDeque<T>>>`) — no FFI overhead, no serialization. Items are moved (memcpy of `size_of::<T>()` bytes), same as any Rust value transfer.
- Combinator structs should be `#[repr(transparent)]` or minimal-size where possible to avoid bloating future state.

### AsyncFd & Crossterm Architecture

- **`AsyncFd` lives in `tau-rt` (dylib)** — it wraps the existing FFI calls (`tau_io_register`, `tau_io_poll_ready`, etc.) in a safe struct. Since it's in the dylib, there's ONE implementation shared by all plugins. It takes `&mut Context<'_>` (standard Rust async), NOT a custom FFI context type. The conversion to `FfiWaker` happens inside `AsyncFd`.

- **`make_ffi_waker` utility** — converts `&Context<'_>` → `FfiWaker` by cloning the waker, boxing it, and providing a `wake_fn` that unboxes and calls `Waker::wake()`. This eliminates the 5 identical copies currently in `tau-tokio/src/net/mod.rs`. The reactor's `tau_io_poll_ready` stores the `FfiWaker` and calls `wake_fn` when the fd becomes ready — this frees the box. If the task is re-polled before the reactor fires, a new waker replaces the old one (the old one is leaked — acceptable because the reactor also drops stored wakers on deregister).

- **Crossterm vendor strategy** — full fork of crossterm 0.28 in `vendor/crossterm/`. The fork removes the `mio`, `signal-hook`, `signal-hook-mio` deps and replaces the event source with a tau reactor-based implementation. The rest of crossterm (escape sequences, terminal control, cursor, style) is unchanged. Patched into plugin builds via `patches.list` entry `crossterm = vendor/crossterm`.

- **Sync event polling** — crossterm's `poll(timeout)` and `read()` need to block. In the tau runtime, this is done by calling `tau::drive()` in a loop with the reactor's `polling::Poller::wait(timeout)`. Since we're single-threaded, this is the same pattern as `tau_block_on`. The sync source can use `polling::Poller` directly (the host's reactor is accessible) or use a simpler approach: register with reactor, then spin `tau::drive()` until ready or timeout.

- **Async EventStream** — implements `futures_core::Stream<Item = io::Result<Event>>` using two `AsyncFd`s (tty + SIGWINCH self-pipe). `poll_next` checks both fds via `AsyncFd::poll_read_ready(cx)`, reads bytes, parses events. No `async-ffi` / `FfiContext` needed — `AsyncFd` converts the standard `Context` internally.

- **SIGWINCH handling** — same self-pipe trick as the old tau fork: `pipe()` → signal handler writes to write-end → read-end is registered with reactor → `EventStream` drains it and emits `Event::Resize`.

### Compatibility Notes

- We re-export `futures_core::Stream` directly — no custom trait. This means `tau::stream::Stream` IS `futures_core::Stream`, zero compatibility concerns.
- `futures-core` is a pure trait-definition crate with no runtime dependency. It's safe to include as a normal dependency of `tau`. Plugins that depend on crates using `futures-core` (e.g., `hyper`, `tower`, `kube-runtime`) will resolve to the same `futures-core` version.
- `tokio-stream 0.1.x` re-exports `futures_core::Stream`. Our `tokio-stream` shim does the same.
- Our `StreamExt` is an extension trait on `futures_core::Stream`. If plugins also import `futures::StreamExt`, the methods will overlap. This is the same situation as real tokio-stream vs futures — Rust handles it via explicit trait imports.
