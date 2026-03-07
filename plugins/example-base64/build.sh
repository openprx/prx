#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Building example-base64 plugin..."

# Check prerequisites
if ! command -v cargo-component &>/dev/null; then
    echo "Error: cargo-component not found."
    echo "Install with: cargo install cargo-component"
    exit 1
fi

if ! rustup target list --installed | grep -q wasm32-wasip2; then
    echo "Error: wasm32-wasip2 target not installed."
    echo "Install with: rustup target add wasm32-wasip2"
    exit 1
fi

# Build the WASM component
cargo component build --release

# Copy to plugin directory
WASM_FILE="target/wasm32-wasip2/release/example_base64.wasm"
if [ -f "$WASM_FILE" ]; then
    cp "$WASM_FILE" plugin.wasm
    echo "==> Built: plugin.wasm ($(wc -c < plugin.wasm) bytes)"
else
    echo "Error: Build output not found at $WASM_FILE"
    exit 1
fi
