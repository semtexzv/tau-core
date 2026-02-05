#!/bin/bash
# µTokio Distribution Script
# Packages the host, runtime, interface, and examples for distribution

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="${SCRIPT_DIR}/dist"
VERSION="0.1.0"

FLAGS=""

BUILD_TYPE="debug"
BUILD_FLAG=""
TARGET_DIR="debug"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)
            BUILD_TYPE="release"
            BUILD_FLAG="--release"
            TARGET_DIR="release"
            shift
            ;;
        --version)
            FLAGS="+$2"
            shift 2
            ;;
        --stable)
            FLAGS="+stable"
            shift
            ;;
        --beta)
            FLAGS="+beta"
            shift
            ;;
        --nightly)
            FLAGS="+nightly"
            shift
            ;;
        *)
            echo "Unknown argument: $1"
            exit 1
            ;;
    esac
done

echo "=== µTokio Distribution Builder ==="
echo "Building version ${VERSION} (${BUILD_TYPE})..."

# Clean and create dist directory
# Ensure we don't pick up old artifacts (optional but safer during rename)
rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"
mkdir -p "${DIST_DIR}/bin"
mkdir -p "${DIST_DIR}/lib"
mkdir -p "${DIST_DIR}/interface"
mkdir -p "${DIST_DIR}/examples"

# Build release
echo ""
echo "[1/5] Building ${BUILD_TYPE} binaries..."
cargo $FLAGS build $BUILD_FLAG

# Copy binaries
echo "[2/5] Copying binaries..."
cp "${SCRIPT_DIR}/target/${TARGET_DIR}/tau" "${DIST_DIR}/bin/tau"
cp "${SCRIPT_DIR}/target/${TARGET_DIR}/libtau.dylib" "${DIST_DIR}/lib/"

# Set RPATH for the tau binary so it can find libstd and runtime in ../lib
install_name_tool -add_rpath "@executable_path/../lib" "${DIST_DIR}/bin/tau"

# Rewrite absolute path to libtau.dylib in the binary to use @rpath
# Extract the current absolute path (or any path ending in libtau.dylib)
TAU_LIB_REF=$(otool -L "${DIST_DIR}/bin/tau" | grep "libtau.dylib" | awk '{print $1}')
if [ ! -z "$TAU_LIB_REF" ]; then
    echo "Rewriting tau reference to libtau exporting path: $TAU_LIB_REF"
    install_name_tool -change "$TAU_LIB_REF" "@rpath/libtau.dylib" "${DIST_DIR}/bin/tau"
fi

# Set install name for libtau.dylib to be relative
install_name_tool -id "@rpath/libtau.dylib" "${DIST_DIR}/lib/libtau.dylib"

# Copy Rust standard library (for independent execution)
echo "[3/5] Copying Rust standard library..."
if command -v rustc &> /dev/null; then
    RUST_SYSROOT=$(rustc $FLAGS --print sysroot)
    # Detect target architecture directory
    if [[ $(uname -m) == "arm64" ]]; then
        STD_LIB_PATH="${RUST_SYSROOT}/lib/rustlib/aarch64-apple-darwin/lib"
    else
        STD_LIB_PATH="${RUST_SYSROOT}/lib/rustlib/x86_64-apple-darwin/lib"
    fi
    
    if [ -d "${STD_LIB_PATH}" ]; then
        # Copy libstd and other potential requirements (libtest not needed)
        # We need libstd-* and potentially others if they are linked
        cp "${STD_LIB_PATH}"/libstd-*.dylib "${DIST_DIR}/lib/"
        echo "Copied libstd to dist/lib"
    else
         echo "Warning: Could not find std lib path: ${STD_LIB_PATH}"
    fi
else
    echo "Warning: rustc not found, skipping libstd copy"
fi

# Copy interface crate (Required for plugin compilation)
echo "[4/5] Copying interface crate..."
cp -r "${SCRIPT_DIR}/tau" "${DIST_DIR}/interface/tau"
rm -rf "${DIST_DIR}/interface/tau/target"

# Vendor dependencies for offline plugin compilation
echo "[5/6] Vendoring dependencies..."
cargo $FLAGS $BUILD_TARGET vendor "${DIST_DIR}/vendor" > /dev/null 2>&1

# Setup Taukio Shim (Prebuilt)
echo "Setting up Taukio shim (Tokio replacement)..."
mkdir -p "${DIST_DIR}/lib/prebuilt"
cp target/${TARGET_DIR}/deps/libtaukio-*.rlib "${DIST_DIR}/lib/prebuilt/"
cp target/${TARGET_DIR}/deps/libtaukio-*.rmeta "${DIST_DIR}/lib/prebuilt/"

# Setup Double Shim in vendor to satisfy cargo resolution without ICE
# Cargo will build this tiny crate, which links to our prebuilt taukio
mkdir -p "${DIST_DIR}/vendor/tokio/src"
cat > "${DIST_DIR}/vendor/tokio/Cargo.toml" << 'EOF'
[package]
name = "tokio"
version = "1.49.0"
edition = "2021"
[features]
default = ["rt", "macros", "time", "sync", "io-util", "net", "fs"]
full = ["rt-multi-thread", "rt", "macros", "time", "sync", "io-util", "net", "fs", "signal", "process"]
rt = []
rt-multi-thread = []
macros = []
time = []
sync = []
io-util = []
net = []
fs = []
signal = []
process = []
EOF
# The magic: re-export the prebuilt taukio crate
echo 'extern crate taukio; pub use taukio::*;' > "${DIST_DIR}/vendor/tokio/src/lib.rs"
echo '{"files":{}}' > "${DIST_DIR}/vendor/tokio/.cargo-checksum.json"

echo "Vendored $(ls "${DIST_DIR}/vendor" | wc -l | tr -d ' ') crates to dist/vendor/"

# Create run script
echo "[6/6] Creating run scripts..."
cat > "${DIST_DIR}/run.sh" << 'EOF'
#!/bin/bash
# µTokio Runner Script
# Sets up environment and runs the host

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Prioritize bundled libs (runtime + std), then system path
# This allows running even if system rust is missing or mismatched
export DYLD_LIBRARY_PATH="${SCRIPT_DIR}/lib:${DYLD_LIBRARY_PATH}"

# Run tau
exec "${SCRIPT_DIR}/bin/tau" "$@"
EOF
chmod +x "${DIST_DIR}/run.sh"

# Create info script
cat > "${DIST_DIR}/info.sh" << 'EOF'
#!/bin/bash
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${SCRIPT_DIR}/run.sh" --info
EOF
chmod +x "${DIST_DIR}/info.sh"

# Create README
cat > "${DIST_DIR}/README.md" << 'EOF'
# µTokio Runtime

Self-contained async plugin system host.

## Components
- `bin/tau`: The host executable.
- `lib/libstd-*.dylib`: Bundled Rust standard library.
- `interface/`: API crate for compiling plugins.

## Usage
Run the host using the wrapper script to automaticall set library paths:

```bash
./run.sh install <user>/<repo>  # Install plugin from GitHub
./run.sh <plugin_name>           # Run installed plugin
./run.sh <plugin.dylib>          # Run precompiled plugin
./run.sh <plugin_source/>        # Compile and run local directory
./run.sh --info                  # Show system info
```
EOF

# Result
echo ""
echo "=== Distribution Ready ==="
echo "Location: ${DIST_DIR}"
ls -lh "${DIST_DIR}/bin/"
ls -lh "${DIST_DIR}/lib/"
echo "Total: $(du -sh "${DIST_DIR}" | cut -f1)"

