#!/usr/bin/env bash
# Build the logger-hook plugin into a WASM component.
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

echo "Building logger-hook..."
echo "  WIT path : ${WIT_PATH}"
echo "  Source   : plugin.py"

componentize-py \
    --wit-path "${WIT_PATH}" \
    --world hook \
    componentize plugin.py \
    -o plugin.wasm

echo "Done: plugin.wasm ($(du -sh plugin.wasm | cut -f1))"
echo
echo "Install:"
echo "  cp plugin.wasm plugin.toml /path/to/prx/plugins/logger-hook/"
