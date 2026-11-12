#!/usr/bin/env bash
# Claim QA from the controller checkout, then build the pinned source on QA x86_64.
# After a confirmed musl failure, PRX_BENCH_MUSL_FAILURE_REASON resumes the GNU fallback.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
[[ -z $(git status --porcelain) ]] || { echo 'clean committed source required' >&2; exit 2; }
sha=$(git rev-parse HEAD)
toolchain=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)
[[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
res=${PRX_RESOURCE_SCRIPT:-/Users/xx/work/code/ramflux/ops/orchestration/tools/res.sh}
resume_gnu=0
if [[ -n ${PRX_BENCH_MUSL_FAILURE_REASON:-} ]]; then
    resume_gnu=1
    printf 'Resuming GNU fallback after musl failure: %s\n' "$PRX_BENCH_MUSL_FAILURE_REASON"
fi
claim=$(timeout 60 "$res" claim qa 1 prx bench-musl)
trap 'timeout 60 "$res" release "$claim"' EXIT
timeout 60 ssh -o ConnectTimeout=10 qa "mkdir -p /opt/ops/prx-lanes/prx-bench/bench-source/$sha"
git archive HEAD | timeout 600 ssh -o ConnectTimeout=10 qa "tar -xf - -C /opt/ops/prx-lanes/prx-bench/bench-source/$sha"
timeout 4200 ssh -o ConnectTimeout=10 qa bash -s -- "$sha" "$toolchain" "$resume_gnu" <<'REMOTE'
set -euo pipefail
sha=$1 toolchain=$2 resume_gnu=$3
[[ $(uname -m) == x86_64 ]] || { echo 'QA x86_64 host required' >&2; exit 2; }
slot=/opt/ops/prx-lanes/prx-bench
mkdir -p "$slot/tmp" /opt/ops/bench/artifacts
build() {
    local image=$1 target=$2 packages=$3
    timeout 1800 podman run --rm --network host \
      -v "$slot/bench-source/$sha:/source:ro" -v "$slot:/build" -w /build \
      -e TOOLCHAIN="$toolchain" -e BUILD_TARGET="$target" -e PACKAGES="$packages" \
      "docker.io/library/rust:1-$image" sh -ec '
        if command -v apk >/dev/null; then
            apk add $PACKAGES
        else
            sed -i "s|http://deb.debian.org|https://deb.debian.org|g" /etc/apt/sources.list
            # Bullseye security packages have moved to the Debian archive.
            sed -i "s|https://deb.debian.org/debian-security|https://archive.debian.org/debian-security|g" /etc/apt/sources.list
            apt-get -o Acquire::Retries=3 -o Acquire::https::No-Cache=true -o Acquire::Check-Valid-Until=false update
            apt-get -o Acquire::Retries=3 -o Acquire::https::No-Cache=true install -y --no-install-recommends $PACKAGES
        fi
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
if [[ $resume_gnu -eq 0 ]] && build alpine "$target" 'build-base cmake perl clang linux-headers'; then
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
