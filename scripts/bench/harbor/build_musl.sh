#!/usr/bin/env bash
# Claim QA from the controller checkout, then build the pinned source on QA x86_64.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
[[ -z $(git status --porcelain) ]] || { echo 'clean committed source required' >&2; exit 2; }
sha=$(git rev-parse HEAD)
toolchain=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)
[[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
res=${PRX_RESOURCE_SCRIPT:-/Users/xx/work/code/ramflux/ops/orchestration/tools/res.sh}
claim=$("$res" claim qa 1 prx bench-musl)
trap '"$res" release "$claim"' EXIT
ssh -o ConnectTimeout=10 qa "mkdir -p /opt/ops/prx-lanes/prx-bench/bench-source/$sha"
git archive HEAD | ssh -o ConnectTimeout=10 qa "tar -xf - -C /opt/ops/prx-lanes/prx-bench/bench-source/$sha"
ssh -o ConnectTimeout=10 qa bash -s -- "$sha" "$toolchain" <<'REMOTE'
set -euo pipefail
sha=$1 toolchain=$2
[[ $(uname -m) == x86_64 ]] || { echo 'QA x86_64 host required' >&2; exit 2; }
slot=/opt/ops/prx-lanes/prx-bench
mkdir -p "$slot/tmp" /opt/ops/bench/artifacts
build() {
    local image=$1 target=$2 packages=$3
    timeout 1800 podman run --rm --network host \
      -v "$slot/bench-source/$sha:/source:ro" -v "$slot:/build" -w /build \
      -e TOOLCHAIN="$toolchain" -e BUILD_TARGET="$target" -e PACKAGES="$packages" \
      "docker.io/library/rust:1-$image" sh -ec '
        if command -v apk >/dev/null; then apk add $PACKAGES; else apt-get update && apt-get install -y $PACKAGES; fi
        rm -rf /build/container-source
        mkdir /build/container-source
        cp -a /source/. /build/container-source/
        cd /build/container-source
        rustup toolchain install "$TOOLCHAIN" --profile minimal
        rustup target add --toolchain "$TOOLCHAIN" "$BUILD_TARGET"
        export CARGO_TARGET_DIR=/build/target TMPDIR=/build/tmp CARGO_BUILD_JOBS=4
        cargo +"$TOOLCHAIN" build --locked --profile ci --bin prx --target "$BUILD_TARGET"
      '
}
target=x86_64-unknown-linux-musl
if build alpine "$target" 'build-base cmake perl clang linux-headers'; then
    binary="$slot/target/$target/ci/prx"
    file "$binary" | grep -Eq 'statically linked|static-pie linked'
    if ldd "$binary"; then echo 'musl artifact unexpectedly dynamic' >&2; exit 1; fi
else
    echo 'musl build failed; falling back to bullseye glibc 2.31' >&2
    target=x86_64-unknown-linux-gnu
    build bullseye "$target" 'build-essential cmake perl clang pkg-config'
    binary="$slot/target/$target/ci/prx"
fi
artifact=/opt/ops/bench/artifacts/prx-$sha
install -m 0755 "$binary" "$artifact"
sha256sum "$artifact" > "$artifact.sha256"
printf 'artifact=%s target=%s\n' "$artifact" "$target"
REMOTE
