#!/usr/bin/env bash
#
# Integration test for task abort across FFI boundary.
#
# Tests:
# 1. Abort an idle (sleeping) task → future is dropped, Drop impl runs
# 2. Abort an already-completed task → no-op
# 3. JoinHandle reports finished after abort
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$ROOT/dist"
TAU="$DIST/run.sh"

cd "$ROOT"

# Build dist if not present or if --rebuild passed
if [ ! -f "$DIST/bin/tau" ] || [[ "${1:-}" == "--rebuild" ]]; then
    echo "=== Building dist ==="
    cargo xtask dist
fi

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  PASS $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

echo
echo "=========================================="
echo " Task Abort Integration Tests"
echo "=========================================="
echo

OUTPUT=$("$TAU" test < "$SCRIPT_DIR/test-abort.txt" 2>&1)

echo "$OUTPUT"
echo

# --- Test 1: Abort idle task ---
echo "Checking abort idle task..."

# The spawned task should have started
if echo "$OUTPUT" | grep -q "\[abort-test\] Spawned task_id="; then
    pass "Task was spawned"
else
    fail "Task should have been spawned"
fi

# Abort should find the task
if echo "$OUTPUT" | grep -q "\[abort-test\] Aborted task_id=.*found=1"; then
    pass "Abort found the task"
else
    fail "Abort should find the task (found=1)"
fi

# Drop detector should have run
if echo "$OUTPUT" | grep -q "\[abort-test\] DropDetector::drop()"; then
    pass "DropDetector::drop() ran — future was cancelled"
else
    fail "DropDetector::drop() should run when future is cancelled"
fi

# Task should be finished after abort
if echo "$OUTPUT" | grep -q "is_finished=1.*drop_ran=true"; then
    pass "Task reports finished with drop confirmed"
else
    fail "Task should report is_finished=1 and drop_ran=true"
fi

# Should NOT see the completion message
if echo "$OUTPUT" | grep -q "Long task completed"; then
    fail "Aborted task should NOT complete"
else
    pass "Aborted task did not complete (as expected)"
fi

# --- Test 2: Abort already-completed task ---
echo
echo "Checking abort already-completed task..."

if echo "$OUTPUT" | grep -q "\[abort-test\] Quick task completed immediately"; then
    pass "Quick task completed"
else
    fail "Quick task should have completed"
fi

# Abort of completed task returns found=0 (already removed by block_on)
if echo "$OUTPUT" | grep -q "\[abort-test\] Abort completed task_id=.*found=0"; then
    pass "Abort of completed task is a no-op (task already removed)"
else
    # It's also acceptable if found=1 but task was already done
    if echo "$OUTPUT" | grep -q "\[abort-test\] Abort completed task_id="; then
        pass "Abort of completed task handled"
    else
        fail "Should see abort result for completed task"
    fi
fi

echo
echo "=========================================="
echo " Summary"
echo "=========================================="
echo
echo "  Passed: $PASS"
echo "  Failed: $FAIL"

if [ "$FAIL" -gt 0 ]; then
    echo
    echo "FAILURES detected!"
    exit 1
fi

echo
echo "All abort tests passed!"
