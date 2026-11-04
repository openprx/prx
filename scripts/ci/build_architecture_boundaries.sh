#!/usr/bin/env bash
#
# Build the architecture_boundaries test binary on its own, optimized.
#
# The suite is 255 textual scanners over the whole source tree. Unoptimized,
# on a 4-vCPU runner, it takes about 10 minutes and single probes exceed the
# 60 s budget; it also needs no code of the crate itself, only std and
# parking_lot, except `retired_feature`, whose three tests use the crate and
# stay in the nextest archive (see nextest_test_count.py). So it is compiled directly with rustc, with the same test
# harness and source files, at opt-level 3, instead of building the 800k-line
# crate (about 25 minutes cold) only to link an unused library.
#
# Usage: scripts/ci/build_architecture_boundaries.sh OUTPUT_BINARY
set -euo pipefail

output="${1:?usage: build_architecture_boundaries.sh OUTPUT_BINARY}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

version="$(awk '/^name = "parking_lot"$/ { getline; gsub(/[^0-9.]/, "", $0); print; exit }' "$root/Cargo.lock")"
mkdir -p "$work/deps/src"
cat >"$work/deps/Cargo.toml" <<EOF
[package]
name = "arch-deps"
version = "0.0.0"
edition = "2021"
[dependencies]
parking_lot = "=$version"
EOF
: >"$work/deps/src/lib.rs"
(cd "$work/deps" && cargo build --release --quiet --target-dir "$work/target")

rlib="$(ls "$work"/target/release/deps/libparking_lot-*.rlib | head -1)"
CARGO_MANIFEST_DIR="$root" rustc --edition 2024 --test --cfg arch_standalone --check-cfg "cfg(arch_standalone)" -C opt-level=3 \
    -A unexpected_cfgs -C debug-assertions=on -C overflow-checks=on \
    -L "dependency=$work/target/release/deps" --extern "parking_lot=$rlib" \
    "$root/tests/architecture_boundaries/main.rs" -o "$output"
