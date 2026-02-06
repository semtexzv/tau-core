#!/bin/bash
# Crossterm plugin smoke test — verifies plugin compiles, loads, and size works
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=== Crossterm Plugin Smoke Test ==="

# Run test mode with commands
OUTPUT=$("$ROOT/dist/run.sh" test < "$SCRIPT_DIR/test-crossterm.txt" 2>&1)
echo "$OUTPUT"

# Check: plugin loaded
if echo "$OUTPUT" | grep -q "OK load ct"; then
    echo "PASS: Plugin loaded"
else
    echo "FAIL: Plugin did not load"
    exit 1
fi

# Check: size command returned valid result (NxN format)
if echo "$OUTPUT" | grep -qE '\[crossterm-test\] size: [0-9]+x[0-9]+'; then
    echo "PASS: terminal::size() returned valid result"
else
    echo "FAIL: terminal::size() did not return valid result"
    exit 1
fi

# Check: plugin initialized (raw mode + cursor)
if echo "$OUTPUT" | grep -q '\[crossterm-test\] Initialized!'; then
    echo "PASS: Plugin initialized"
else
    echo "FAIL: Plugin did not initialize"
    exit 1
fi

echo "=== All crossterm smoke tests passed ==="
