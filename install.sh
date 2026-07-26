#!/bin/sh
set -eu

REPOSITORY="openprx/prx"
REQUIRED_RUST_VERSION="1.97.1"
VERSION="latest"
INSTALL_DIR="${PRX_INSTALL_DIR:-${HOME}/.local/bin}"
SOURCE_BUILD="false"

info() {
    printf '==> %s\n' "$*" >&2
}

error() {
    printf 'error: %s\n' "$*" >&2
}

usage() {
    cat <<'EOF'
Install PRX from a verified GitHub Release binary.

Usage:
  install.sh [--version <vX.Y.Z>] [--install-dir <path>] [--source]

Options:
  --version <version>   Install an exact release (default: latest)
  --install-dir <path>  Destination directory (default: ~/.local/bin)
  --source              Build from source with Rust 1.97.1 or newer
  -h, --help            Show this help

The default binary path does not require Rust, Cargo, Git, or sudo.
After installation, configure PRX with:
  prx onboard --interactive
EOF
}

need_value() {
    if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        error "$1 requires a value"
        exit 2
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            need_value "$@"
            VERSION="$2"
            shift 2
            ;;
        --install-dir)
            need_value "$@"
            INSTALL_DIR="$2"
            shift 2
            ;;
        --source)
            SOURCE_BUILD="true"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            error "unknown option: $1"
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$VERSION" = "latest" ]; then
    RELEASE_PATH="latest/download"
else
    normalized_version="${VERSION#v}"
    if ! printf '%s\n' "$normalized_version" |
        grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$'; then
        error "version must be 'latest' or a semantic version such as v0.8.20"
        exit 2
    fi
    VERSION="v$normalized_version"
    RELEASE_PATH="download/$VERSION"
fi

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

version_at_least() {
    awk -v current="$1" -v required="$2" 'BEGIN {
        split(current, c, ".");
        split(required, r, ".");
        for (i = 1; i <= 3; i++) {
            c[i] += 0;
            r[i] += 0;
            if (c[i] > r[i]) exit 0;
            if (c[i] < r[i]) exit 1;
        }
        exit 0;
    }'
}

make_temp_dir() {
    temp_root="${TMPDIR:-/tmp}"
    mktemp -d "${temp_root%/}/prx-install.XXXXXX"
}

install_atomically() {
    source_binary="$1"
    destination="$INSTALL_DIR/prx"
    temporary="$INSTALL_DIR/.prx.tmp.$$"

    mkdir -p "$INSTALL_DIR"
    cp "$source_binary" "$temporary"
    chmod 0755 "$temporary"
    mv -f "$temporary" "$destination"
    printf '%s\n' "$destination"
}

install_from_source() {
    if ! command_exists git || ! command_exists cargo || ! command_exists rustc; then
        error "--source requires git, cargo, and rustc"
        error "install Rust ${REQUIRED_RUST_VERSION} or newer from https://rustup.rs/"
        exit 1
    fi

    current_rust="$(rustc --version | awk '{print $2}')"
    if ! version_at_least "$current_rust" "$REQUIRED_RUST_VERSION"; then
        error "Rust ${REQUIRED_RUST_VERSION} or newer is required (found ${current_rust})"
        exit 1
    fi

    temp_dir="$(make_temp_dir)"
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

    if [ "$VERSION" = "latest" ]; then
        info "Cloning the latest PRX source"
        git clone --depth 1 "https://github.com/${REPOSITORY}.git" "$temp_dir/source"
    else
        info "Cloning PRX ${VERSION}"
        git clone --depth 1 --branch "$VERSION" "https://github.com/${REPOSITORY}.git" "$temp_dir/source"
    fi

    info "Building PRX with Rust ${current_rust}"
    (
        cd "$temp_dir/source"
        cargo build --release --locked --bin prx
    )
    installed="$(install_atomically "$temp_dir/source/target/release/prx")"
    trap - EXIT HUP INT TERM
    rm -rf "$temp_dir"
    printf '%s\n' "$installed"
}

detect_asset_suffix() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            if [ -f /etc/alpine-release ] || (command_exists ldd && ldd --version 2>&1 | grep -qi musl); then
                error "pre-built PRX binaries currently require glibc; use --source on musl/Alpine"
                return 1
            fi
            case "$arch" in
                x86_64|amd64) printf '%s\n' "linux-amd64" ;;
                aarch64|arm64) printf '%s\n' "linux-arm64" ;;
                *)
                    error "unsupported Linux architecture: $arch"
                    return 1
                    ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64|amd64) printf '%s\n' "macos-amd64" ;;
                arm64|aarch64) printf '%s\n' "macos-arm64" ;;
                *)
                    error "unsupported macOS architecture: $arch"
                    return 1
                    ;;
            esac
            ;;
        *)
            error "unsupported operating system: $os"
            error "Windows users should run install.ps1"
            return 1
            ;;
    esac
}

download() {
    url="$1"
    output="$2"
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$output"
}

verify_sha256() {
    archive="$1"
    checksum_file="$2"
    expected="$(awk 'NR == 1 {print tolower($1)}' "$checksum_file")"

    if ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$'; then
        error "release checksum is malformed"
        return 1
    fi

    if command_exists sha256sum; then
        actual="$(sha256sum "$archive" | awk '{print tolower($1)}')"
    elif command_exists shasum; then
        actual="$(shasum -a 256 "$archive" | awk '{print tolower($1)}')"
    else
        error "sha256sum or shasum is required to verify the release"
        return 1
    fi

    if [ "$actual" != "$expected" ]; then
        error "SHA-256 verification failed"
        return 1
    fi
}

install_prebuilt() {
    if ! command_exists curl || ! command_exists tar; then
        error "curl and tar are required for binary installation"
        exit 1
    fi

    suffix="$(detect_asset_suffix)"
    archive_name="prx-${suffix}.tar.gz"
    base_url="https://github.com/${REPOSITORY}/releases/${RELEASE_PATH}"
    temp_dir="$(make_temp_dir)"
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

    info "Downloading ${archive_name}"
    download "${base_url}/${archive_name}" "$temp_dir/$archive_name"
    download "${base_url}/${archive_name}.sha256" "$temp_dir/$archive_name.sha256"
    verify_sha256 "$temp_dir/$archive_name" "$temp_dir/$archive_name.sha256"

    mkdir "$temp_dir/extract"
    tar -xzf "$temp_dir/$archive_name" -C "$temp_dir/extract" prx
    if [ ! -f "$temp_dir/extract/prx" ]; then
        error "release archive does not contain prx"
        exit 1
    fi

    installed="$(install_atomically "$temp_dir/extract/prx")"
    trap - EXIT HUP INT TERM
    rm -rf "$temp_dir"
    printf '%s\n' "$installed"
}

umask 022
if [ "$SOURCE_BUILD" = "true" ]; then
    PRX_BINARY="$(install_from_source)"
else
    PRX_BINARY="$(install_prebuilt)"
fi

info "Installed PRX to ${PRX_BINARY}"
"$PRX_BINARY" --version

case ":${PATH:-}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        printf '\n%s\n' "Add PRX to this shell's PATH:"
        printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        ;;
esac

cat <<'EOF'

Next:
  prx onboard --interactive

Optional daemon service:
  prx service install
  prx service start
EOF
