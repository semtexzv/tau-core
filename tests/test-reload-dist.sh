#!/usr/bin/env bash
#
# Test reload scenarios against the DIST build.
# Uses dist/run.sh instead of cargo run.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$ROOT/dist"
TAU="$DIST/run.sh"
TEST_FILE="$SCRIPT_DIR/test-reload.txt"

if [ ! -f "$DIST/bin/tau" ]; then
    echo "Dist not built. Run: cargo xtask dist"
    exit 1
fi

echo "=== Running reload tests against DIST ==="
echo "Binary: $DIST/bin/tau"
echo

OUTPUT=$("$TAU" test < "$TEST_FILE" 2>&1)

echo "$OUTPUT"
echo

# Check for failures
FAILS=$(echo "$OUTPUT" | grep -c "^FAIL" || true)
PASSES=$(echo "$OUTPUT" | grep -c "^PASS" || true)

echo "=== Summary (DIST) ==="
echo "Passed: $PASSES"
echo "Failed: $FAILS"

if [ "$FAILS" -gt 0 ]; then
    echo
    echo "FAILURES:"
    echo "$OUTPUT" | grep "^FAIL"
    exit 1
fi

echo
echo "All dist reload tests passed!"
