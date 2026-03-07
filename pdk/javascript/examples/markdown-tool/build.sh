#!/usr/bin/env bash
# build.sh — Build the markdown-tool WASM component
#
# Requirements:
#   Node.js >= 20
#   npm install (run once to install deps)
#
# Usage:
#   ./build.sh           # TypeScript compile + componentize
#   ./build.sh --tsc     # TypeScript compile only
#   ./build.sh --clean   # Clean build artifacts

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

clean() {
  echo "[build] Cleaning..."
  rm -rf dist plugin.wasm
}

build_tsc() {
  echo "[build] Compiling TypeScript..."
  npx tsc
  echo "[build] TypeScript OK"
}

build_wasm() {
  echo "[build] Componentizing to WASM..."
  npx jco componentize dist/plugin.js \
    --wit ../../../../wit \
    --world tool \
    -o plugin.wasm
  local size
  size=$(du -h plugin.wasm | cut -f1)
  echo "[build] plugin.wasm created (${size})"
}

case "${1:-}" in
  --clean)
    clean
    ;;
  --tsc)
    build_tsc
    ;;
  *)
    build_tsc
    build_wasm
    echo "[build] Done: plugin.wasm"
    ;;
esac
