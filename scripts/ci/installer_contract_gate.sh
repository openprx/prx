#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
installer="$repo_root/install.sh"
release_workflow="$repo_root/.github/workflows/release.yml"
temp_dir="$(mktemp -d -t prx-installer-contract-XXXXXX)"
trap 'rm -rf "$temp_dir"' EXIT

fail() {
    echo "installer contract failure: $*" >&2
    exit 1
}

test -x "$installer" || fail "install.sh must be executable"
sh -n "$installer"
pwsh -NoProfile -Command "& { [void][scriptblock]::Create((Get-Content -Raw '$repo_root/install.ps1')) }" \
    2>/dev/null || {
    if command -v pwsh >/dev/null 2>&1; then
        fail "install.ps1 failed to parse"
    fi
    echo "pwsh unavailable; skipped PowerShell parse check"
}

for suffix in linux-amd64 linux-arm64 macos-amd64 macos-arm64 windows-amd64; do
    grep -q "prx-${suffix}" "$release_workflow" || fail "release workflow is missing prx-${suffix}"
done

fixture_dir="$temp_dir/fixtures"
mock_bin="$temp_dir/mock-bin"
install_dir="$temp_dir/install/bin"
mkdir -p "$fixture_dir" "$mock_bin" "$temp_dir/archive"

cat >"$temp_dir/archive/prx" <<'EOF'
#!/bin/sh
echo "prx 9.9.9-test"
EOF
chmod +x "$temp_dir/archive/prx"
tar -czf "$fixture_dir/prx-linux-amd64.tar.gz" -C "$temp_dir/archive" prx
sha256sum "$fixture_dir/prx-linux-amd64.tar.gz" >"$fixture_dir/prx-linux-amd64.tar.gz.sha256"

cat >"$mock_bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) echo "${PRX_TEST_OS:-Linux}" ;;
    -m) echo "${PRX_TEST_ARCH:-x86_64}" ;;
    *) echo "${PRX_TEST_OS:-Linux}" ;;
esac
EOF

cat >"$mock_bin/ldd" <<'EOF'
#!/bin/sh
echo "${PRX_TEST_LDD:-ldd (GNU libc) 2.39}"
EOF

cat >"$mock_bin/rustc" <<'EOF'
#!/bin/sh
echo "rustc 1.96.0 (test)"
EOF

cat >"$mock_bin/curl" <<'EOF'
#!/bin/sh
set -eu
url=""
output=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            output="$2"
            shift 2
            ;;
        http*)
            url="$1"
            shift
            ;;
        *)
            shift
            ;;
    esac
done
printf '%s\n' "$url" >>"$PRX_TEST_URL_LOG"
cp "$PRX_FIXTURE_DIR/$(basename "$url")" "$output"
EOF
chmod +x "$mock_bin/uname" "$mock_bin/ldd" "$mock_bin/rustc" "$mock_bin/curl"

export PRX_FIXTURE_DIR="$fixture_dir"
export PRX_TEST_URL_LOG="$temp_dir/urls.log"
test_path="$mock_bin:$PATH"

HOME="$temp_dir/home" PATH="$test_path" "$installer" \
    --version v9.9.9 \
    --install-dir "$install_dir" >"$temp_dir/install.out"

test -x "$install_dir/prx" || fail "installer did not create an executable"
grep -q "prx 9.9.9-test" "$temp_dir/install.out" || fail "installed binary was not verified"
grep -q "/releases/download/v9.9.9/prx-linux-amd64.tar.gz" "$temp_dir/urls.log" \
    || fail "exact-version asset URL is incorrect"

printf '%064d  %s\n' 0 "prx-linux-amd64.tar.gz" >"$fixture_dir/prx-linux-amd64.tar.gz.sha256"
if HOME="$temp_dir/home" PATH="$test_path" "$installer" \
    --version v9.9.9 \
    --install-dir "$temp_dir/tampered/bin" >"$temp_dir/tampered.out" 2>&1; then
    fail "installer accepted a bad checksum"
fi
test ! -e "$temp_dir/tampered/bin/prx" || fail "tampered archive was installed"

if HOME="$temp_dir/home" PATH="$test_path" PRX_TEST_ARCH="mips64" "$installer" \
    --version v9.9.9 \
    --install-dir "$temp_dir/unsupported/bin" >"$temp_dir/unsupported.out" 2>&1; then
    fail "installer accepted an unsupported architecture"
fi
grep -q "unsupported Linux architecture" "$temp_dir/unsupported.out" \
    || fail "unsupported-architecture error is not actionable"

if HOME="$temp_dir/home" PATH="$test_path" PRX_TEST_LDD="musl libc" "$installer" \
    --version v9.9.9 \
    --install-dir "$temp_dir/musl/bin" >"$temp_dir/musl.out" 2>&1; then
    fail "installer accepted a glibc binary on musl"
fi
grep -q "require glibc" "$temp_dir/musl.out" \
    || fail "musl error is not actionable"

if HOME="$temp_dir/home" PATH="$test_path" "$installer" \
    --source \
    --install-dir "$temp_dir/source/bin" >"$temp_dir/source.out" 2>&1; then
    fail "source installer accepted Rust older than 1.97.1"
fi
grep -q "Rust 1.97.1 or newer is required" "$temp_dir/source.out" \
    || fail "source Rust version error is not actionable"

echo "Installer contract gate passed."
