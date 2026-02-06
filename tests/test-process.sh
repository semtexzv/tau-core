#!/bin/bash
set -e

# Build dist
cargo xtask dist

# Run the test using dist/bin/tau
./dist/bin/tau test < tests/test-process.txt > tests/test-process.out 2>&1

# Check for failures
FAIL=0

check() {
    if grep -q "$1" tests/test-process.out; then
        echo "PASS: $2"
    else
        echo "FAIL: $2"
        FAIL=1
    fi
}

check "Echo success" "Echo test"
check "Cat success" "Pipe-cat test"

# Count checks - should be 2 checks, so 2 "check success=true" lines
COUNT=$(grep -c "check success=true" tests/test-process.out || true)
if [ "$COUNT" -eq 2 ]; then
    echo "PASS: All 2 checks confirmed success"
else
    echo "FAIL: Only $COUNT/2 checks confirmed success"
    FAIL=1
fi

if [ "$FAIL" -eq 1 ]; then
    echo "=== Output log ==="
    cat tests/test-process.out
    exit 1
fi
