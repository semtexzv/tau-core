#!/usr/bin/env bash
#
# Safety tests for tau-core plugin system.
#
# Part 1: Compile-time guard verification
#   - Box<dyn Trait>         → NOT Clone → compile error ✅
#   - Arc<dyn Fn(...)>       → IS Clone  → compiles (KNOWN GAP)
#   - Arc<dyn Trait>         → IS Clone  → compiles (KNOWN GAP)
#
# Part 2: Complex shared types across plugins
#   - AppConfig (custom Drop, HashMap, Vec)
#   - SharedState (Arc<Mutex<...>>)
#   - UpperProcessor (concrete type with trait methods)
#   - Cross-plugin reads, mutations, reload
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$ROOT"

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  PASS $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

echo "=== Building host ==="
cargo build 2>/dev/null

echo
echo "=========================================="
echo " Part 1: Clone guard verification"
echo "=========================================="
echo

# --- Box<dyn Trait>: NOT Clone → should fail ---
echo "Test: Box<dyn Trait>"
OUT=$(printf 'load bad plugins/bad-boxdyn-plugin\n' | cargo run -- test 2>&1)
if echo "$OUT" | grep -q "COMPILE_FAIL\|Compilation failed"; then
    pass "Box<dyn Trait> rejected (not Clone)"
else
    fail "Box<dyn Trait> should not compile"
fi

# --- Arc<dyn Fn(...)>: IS Clone → compiles (known gap) ---
echo "Test: Arc<dyn Fn(u64) -> String>"
OUT=$(printf 'load handler plugins/bad-closure-plugin\nsend handler test-call\nassert_resource erased-handler\nunload handler\nassert_no_resource erased-handler\n' | cargo run -- test 2>&1)
if echo "$OUT" | grep -q "OK load handler"; then
    pass "Arc<dyn Fn> compiles (KNOWN GAP — Clone guard doesn't catch)"
else
    fail "Arc<dyn Fn> should compile"
fi
if echo "$OUT" | grep -q "handler(42) = hello-42"; then
    pass "Arc<dyn Fn> works while plugin loaded"
else
    fail "Arc<dyn Fn> should work while plugin loaded"
fi
if echo "$OUT" | grep -q "PASS assert_no_resource erased-handler"; then
    pass "Arc<dyn Fn> cleaned up on unload"
else
    fail "Arc<dyn Fn> should be cleaned up on unload"
fi

# --- Arc<dyn Trait>: IS Clone → compiles (known gap) ---
echo "Test: Arc<dyn Trait>"
OUT=$(printf 'load arcdyn plugins/bad-arcdyn-plugin\nsend arcdyn test-value\nassert_resource dyn-processor\nunload arcdyn\nassert_no_resource dyn-processor\n' | cargo run -- test 2>&1)
if echo "$OUT" | grep -q "OK load arcdyn"; then
    pass "Arc<dyn Trait> compiles (KNOWN GAP — Clone guard doesn't catch)"
else
    fail "Arc<dyn Trait> should compile"
fi
if echo "$OUT" | grep -q 'TEST-VALUE'; then
    pass "Arc<dyn Trait> works while plugin loaded"
else
    fail "Arc<dyn Trait> should work while plugin loaded"
fi
if echo "$OUT" | grep -q "PASS assert_no_resource dyn-processor"; then
    pass "Arc<dyn Trait> cleaned up on unload"
else
    fail "Arc<dyn Trait> should be cleaned up on unload"
fi

echo
echo "=========================================="
echo " Part 2: Complex shared types across plugins"
echo "=========================================="
echo

OUTPUT=$(cargo run -- test < "$SCRIPT_DIR/test-safety.txt" 2>&1)

# Count assert PASS/FAIL from runtime
RUNTIME_PASS=$(echo "$OUTPUT" | grep -c "^PASS" || true)
RUNTIME_FAIL=$(echo "$OUTPUT" | grep -c "^FAIL" || true)
PASS=$((PASS + RUNTIME_PASS))
FAIL=$((FAIL + RUNTIME_FAIL))

# Show key lines
echo "$OUTPUT" | grep -E "^(---|PASS|FAIL)" | head -30
echo

echo "Checking cross-plugin behavior..."

# Reader reads writer's AppConfig
if echo "$OUTPUT" | grep -q "\[shared-reader\] READ config.name = my-app"; then
    pass "Cross-plugin: Reader reads AppConfig (String, HashMap, Vec)"
else
    fail "Cross-plugin: Reader should read AppConfig"
fi

# Reader mutates writer's SharedState via Arc<Mutex>
if echo "$OUTPUT" | grep -q "\[shared-reader\] state.version.*len"; then
    pass "Cross-plugin: Reader mutates SharedState (Arc<Mutex<...>>)"
else
    fail "Cross-plugin: Reader should mutate SharedState"
fi

# Reader uses cloned UpperProcessor
if echo "$OUTPUT" | grep -q "\[shared-reader\] processed:"; then
    pass "Cross-plugin: Reader calls Processor::process() on cloned value"
else
    fail "Cross-plugin: Reader should use cloned Processor"
fi

# After writer unload, reader sees NOT FOUND
if echo "$OUTPUT" | grep -q "\[shared-reader\] app-config NOT FOUND"; then
    pass "Cleanup: Reader sees NOT FOUND after writer unloaded"
else
    fail "Cleanup: Reader should see NOT FOUND after writer unloaded"
fi

# After writer reload, reader reads fresh resources
if echo "$OUTPUT" | grep -q "after-writer-reload"; then
    pass "Reload: Cross-plugin works after writer reload"
else
    fail "Reload: Cross-plugin should work after writer reload"
fi

# Custom Drop impl called
DROPS=$(echo "$OUTPUT" | grep -c "\[shared-types\] AppConfig.*dropped" || true)
if [ "$DROPS" -gt 0 ]; then
    pass "Drop: AppConfig custom Drop called ${DROPS} times"
else
    fail "Drop: AppConfig custom Drop should be called"
fi

echo
echo "=========================================="
echo " Part 3: Crash test — Arc<dyn Trait> after dlclose"
echo "=========================================="
echo

echo "Test: Reader holds Arc<dyn Trait> clone, writer unloaded, reader calls method"
# Run in a subprocess — we EXPECT this to crash
set +e
CRASH_OUT=$(cargo run -- test < "$SCRIPT_DIR/test-crash.txt" 2>&1)
CRASH_EXIT=$?
set -e

# Show what happened
echo "$CRASH_OUT" | grep -E "\[crash" | head -20
echo "  exit code: $CRASH_EXIT"
echo

if echo "$CRASH_OUT" | grep -q "process('after-unload')"; then
    fail "Arc<dyn Trait> call SUCCEEDED after dlclose (should have crashed!)"
elif [ $CRASH_EXIT -ne 0 ]; then
    pass "Arc<dyn Trait> CRASHED after dlclose (exit=$CRASH_EXIT — dangling vtable)"
elif ! echo "$CRASH_OUT" | grep -q "process('after-unload')"; then
    # Exit 0 but no success output — process was killed by signal
    pass "Arc<dyn Trait> CRASHED after dlclose (killed by signal)"
fi

echo
echo "=========================================="
echo " Summary"
echo "=========================================="
echo
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo
echo "  Known gaps in Clone guard:"
echo "    - Arc<dyn Trait>  compiles (vtable in plugin code)"
echo "    - Arc<dyn Fn(.)>  compiles (vtable in plugin code)"
echo "  Mitigation: host cleans up on unload before dlclose."
echo "  Danger: if another plugin clones and holds past originator unload."

if [ "$FAIL" -gt 0 ]; then
    echo
    echo "FAILURES:"
    echo "$OUTPUT" | grep "^FAIL" || true
    exit 1
fi

echo
echo "All safety tests passed!"
