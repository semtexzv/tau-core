#!/bin/bash
# µTokio Runner Script
# Sets up environment and runs the host

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Prioritize bundled libs (runtime + std), then system path
# This allows running even if system rust is missing or mismatched
export DYLD_LIBRARY_PATH="${SCRIPT_DIR}/lib:${DYLD_LIBRARY_PATH}"

# Run tau
exec "${SCRIPT_DIR}/bin/tau" "$@"
