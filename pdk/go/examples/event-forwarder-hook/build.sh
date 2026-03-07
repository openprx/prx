#!/usr/bin/env bash
# Build event-forwarder-hook plugin with TinyGo.
# Requires: TinyGo >= 0.34  https://tinygo.org/getting-started/install/
set -euo pipefail

PLUGIN_NAME="event-forwarder-hook"
OUT="plugin.wasm"

echo "Building ${PLUGIN_NAME}..."
tinygo build \
  -target wasm32-wasip2 \
  -scheduler none \
  -no-debug \
  -opt 2 \
  -o "${OUT}" \
  .

SIZE=$(wc -c < "${OUT}")
echo "Built ${OUT} (${SIZE} bytes)"
