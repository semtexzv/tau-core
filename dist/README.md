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
