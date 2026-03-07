#!/usr/bin/env bash
# Build the hello-tool plugin into a WASM component.
#
# Prerequisites:
#   pip install componentize-py>=0.16
#
# Usage:
#   ./build.sh
#   # → plugin.wasm (WASM component)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WIT_PATH="${SCRIPT_DIR}/../../../../wit"

echo "Building hello-tool (json_formatter)..."
echo "  WIT path : ${WIT_PATH}"
echo "  Source   : plugin.py"

componentize-py \
    --wit-path "${WIT_PATH}" \
    --world tool \
    componentize plugin.py \
    -o plugin.wasm

echo "Done: plugin.wasm ($(du -sh plugin.wasm | cut -f1))"
echo
echo "Install:"
echo "  cp plugin.wasm plugin.toml /path/to/prx/plugins/json-formatter/"
