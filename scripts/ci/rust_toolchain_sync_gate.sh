#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

toolchain_version="$(awk -F'"' '/^channel = / { print $2; exit }' rust-toolchain.toml)"
cargo_version="$(awk -F'"' '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^rust-version = / { print $2; exit }
' Cargo.toml)"
docker_version="$(sed -nE 's/^FROM rust:([0-9.]+)-slim@.*/\1/p' Dockerfile | head -n 1)"

test -n "$toolchain_version"
test "$cargo_version" = "$toolchain_version"
test "$docker_version" = "$toolchain_version"

mapfile -t workflow_versions < <(
    grep -rhoE 'toolchain: [0-9]+\.[0-9]+\.[0-9]+' .github/workflows \
        | awk '{print $2}' \
        | sort -u
)
test "${#workflow_versions[@]}" -eq 1
test "${workflow_versions[0]}" = "$toolchain_version"

default_reusable_version="$(
    awk -F'"' '
        /default: "[0-9]+\.[0-9]+\.[0-9]+"/ { print $2; exit }
    ' .github/workflows/test-rust-build.yml
)"
test "$default_reusable_version" = "$toolchain_version"

echo "Rust toolchain contract is synchronized at ${toolchain_version}."
