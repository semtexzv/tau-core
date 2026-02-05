#!/usr/bin/env bash
#
# Test script for tau-core plugin reload scenarios.
#
# Exercises:
#   1. Single plugin load/run/unload/reload
#   2. Two plugins communicating, reload one while other stays
#   3. Reload the other one
#   4. Both reloaded, still communicate
#   5. Resource cleanup on unload
#   6. Resource survival across reload of a DIFFERENT plugin
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_FILE="$SCRIPT_DIR/test-reload.txt"

# Build host first
echo "=== Building host ==="
cd "$ROOT"
cargo build 2>/dev/null

echo "=== Running reload tests ==="
echo

OUTPUT=$(cargo run -- test < "$TEST_FILE" 2>&1)

echo "$OUTPUT"
echo

# Check for failures
FAILS=$(echo "$OUTPUT" | grep -c "^FAIL" || true)
PASSES=$(echo "$OUTPUT" | grep -c "^PASS" || true)

echo "=== Summary ==="
echo "Passed: $PASSES"
echo "Failed: $FAILS"

if [ "$FAILS" -gt 0 ]; then
    echo
    echo "FAILURES:"
    echo "$OUTPUT" | grep "^FAIL"
    exit 1
fi

echo
echo "All reload tests passed!"
