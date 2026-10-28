#!/bin/sh

set -eu

REPOSITORY="nolabs-ai/nonaut"
RELEASES_URL="https://github.com/$REPOSITORY/releases"
INSTALL_MODE="release"
INSTALL_FLAVOR="standard"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
    ORANGE='\033[38;2;232;115;74m'
    GREEN='\033[38;2;126;198;153m'
    RED='\033[38;2;224;108;117m'
    MUTED='\033[38;2;136;136;136m'
    RESET='\033[0m'
else
    ORANGE=''
    GREEN=''
    RED=''
    MUTED=''
    RESET=''
fi

notice() {
    printf '%b◇%b %s\n' "$ORANGE" "$RESET" "$1"
}

success() {
    printf '%b◆%b %b%s%b\n' "$GREEN" "$RESET" "$GREEN" "$1" "$RESET"
}

fatal() {
    printf '%b◆%b %b%s%b\n' "$RED" "$RESET" "$RED" "$1" "$RESET" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Install Nonaut from a verified release or build the latest main branch.

Usage:
  install.sh
  install.sh --control-plane
  install.sh --from-main
  install.sh --from-main --control-plane

Options:
  --control-plane  Include enrollment and durable audit delivery
  --from-main     Build and install the current main branch with Cargo
  -h, --help      Show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --control-plane) INSTALL_FLAVOR="control-plane" ;;
        --from-main) INSTALL_MODE="main" ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) fatal "Unknown installer option: $1" ;;
    esac
    shift
done

command -v curl >/dev/null 2>&1 || fatal "curl is required to install Nonaut."
command -v tar >/dev/null 2>&1 || fatal "tar is required to install Nonaut."

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-gnu" ;;
    *) fatal "Unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) fatal "Unsupported architecture: $(uname -m)" ;;
esac

if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "x86_64" ]; then
    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        os="unknown-linux-musl"
    fi
fi

if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "aarch64" ]; then
    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        fatal "Linux ARM64 with musl is not available in the current release matrix."
    fi
fi

target="$arch-$os"

if [ -n "${NONAUT_INSTALL_DIR:-}" ]; then
    install_dir=$NONAUT_INSTALL_DIR
else
    [ -n "${HOME:-}" ] || fatal "HOME is not set. Set NONAUT_INSTALL_DIR explicitly."
    install_dir="$HOME/.local/bin"
fi
destination="$install_dir/nonaut"
metadata="$install_dir/.nonaut-install"

temporary=''
staged=''
cleanup() {
    if [ -n "$temporary" ]; then
        rm -rf "$temporary"
    fi
    if [ -n "$staged" ]; then
        rm -f "$staged"
    fi
}
trap cleanup EXIT HUP INT TERM

install_binary() {
    binary=$1
    mkdir -p "$install_dir" \
        || fatal "Could not create $install_dir. Set NONAUT_INSTALL_DIR to another directory."
    staged="$install_dir/.nonaut-install-$$"
    install -m 755 "$binary" "$staged" \
        || fatal "Could not write to $install_dir. Set NONAUT_INSTALL_DIR to another directory."
    mv -f "$staged" "$destination" \
        || fatal "Could not replace $destination."
    staged=''
}

record_install() {
    channel=$1
    installed=$2
    flavor=$3
    staged="$install_dir/.nonaut-metadata-$$"
    printf 'channel=%s\nversion=%s\nflavor=%s\n' "$channel" "$installed" "$flavor" >"$staged" \
        || fatal "Could not write installation metadata to $install_dir."
    mv -f "$staged" "$metadata" \
        || fatal "Could not replace installation metadata in $install_dir."
    staged=''
}

installed_channel=''
installed_version=''
installed_flavor='standard'
if [ -x "$destination" ]; then
    installed_version=$($destination --version 2>/dev/null | awk '{print $2}' || true)
fi
if [ -f "$metadata" ]; then
    installed_channel=$(sed -n 's/^channel=//p' "$metadata" | head -n 1)
    saved_flavor=$(sed -n 's/^flavor=//p' "$metadata" | head -n 1)
    if [ -n "$saved_flavor" ]; then
        installed_flavor=$saved_flavor
    fi
fi

show_path_hint() {
    case ":${PATH:-}:" in
        *:"$install_dir":*) ;;
        *)
            printf '%b  Add %s to PATH to run nonaut.%b\n' "$MUTED" "$install_dir" "$RESET"
            ;;
    esac
}

if [ "$INSTALL_MODE" = "main" ]; then
    command -v cargo >/dev/null 2>&1 \
        || fatal "Cargo is required to build Nonaut from main."
    if [ -x "$destination" ]; then
        notice "Replacing the installed Nonaut binary with the latest main build..."
    else
        notice "Installing the latest Nonaut main build..."
    fi
    notice "Downloading the latest Nonaut source from main..."
    temporary=$(mktemp -d "${TMPDIR:-/tmp}/nonaut.XXXXXX") \
        || fatal "Could not create a temporary directory."
    source_archive="$temporary/nonaut-main.tar.gz"
    curl -fsSL "https://github.com/$REPOSITORY/archive/refs/heads/main.tar.gz" \
        -o "$source_archive" \
        || fatal "Could not download the Nonaut main branch."
    tar -xzf "$source_archive" -C "$temporary" \
        || fatal "Could not extract the Nonaut source archive."
    source_dir="$temporary/nonaut-main"
    [ -f "$source_dir/Cargo.toml" ] \
        || fatal "The source archive did not contain Cargo.toml."
    notice "Building Nonaut from main..."
    if [ "$INSTALL_FLAVOR" = "control-plane" ]; then
        (cd "$source_dir" && cargo build --locked --release --features control-plane) \
            || fatal "The Nonaut control-plane source build failed."
    else
        (cd "$source_dir" && cargo build --locked --release) \
            || fatal "The Nonaut source build failed."
    fi
    binary="$source_dir/target/release/nonaut"
    [ -x "$binary" ] || fatal "The source build did not produce a Nonaut binary."
    version=$($binary --version 2>/dev/null | awk '{print $2}')
    install_binary "$binary"
    record_install "main" "${version:-unknown}" "$INSTALL_FLAVOR"
    success "Installed Nonaut ${version:-from main} to $destination"
    show_path_hint
    exit 0
fi

if [ -n "${NONAUT_VERSION:-}" ]; then
    tag="$NONAUT_VERSION"
    case "$tag" in
        v*) ;;
        *) tag="v$tag" ;;
    esac
else
    notice "Checking the latest Nonaut release..."
    latest_url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$RELEASES_URL/latest") \
        || fatal "Could not resolve the latest Nonaut release."
    tag=${latest_url##*/}
fi

printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' \
    || fatal "Invalid release version: $tag"

version=${tag#v}

if [ -x "$destination" ]; then
    if [ "$installed_channel" = "release" ] && [ "$installed_version" = "$version" ] \
        && [ "$installed_flavor" = "$INSTALL_FLAVOR" ]; then
        success "Nonaut $version is already installed."
        exit 0
    fi
    if [ "$installed_channel" = "main" ]; then
        notice "Replacing the main build with Nonaut release $version..."
    else
        notice "Updating Nonaut ${installed_version:-unknown} to $version..."
    fi
else
    notice "Installing Nonaut $version for $target..."
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/nonaut.XXXXXX") \
    || fatal "Could not create a temporary directory."

archive_prefix="nonaut"
if [ "$INSTALL_FLAVOR" = "control-plane" ]; then
    archive_prefix="nonaut-control-plane"
fi
archive="$archive_prefix-$tag-$target.tar.gz"
base_url="$RELEASES_URL/download/$tag"

curl -fsSL "$base_url/$archive" -o "$temporary/$archive" \
    || fatal "No Nonaut $tag binary is available for $target."
curl -fsSL "$base_url/SHA256SUMS.txt" -o "$temporary/SHA256SUMS.txt" \
    || fatal "Could not download release checksums."

expected=$(awk -v archive="$archive" '$2 == archive { print $1 }' "$temporary/SHA256SUMS.txt")
[ -n "$expected" ] || fatal "The release checksum does not include $archive."

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary/$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$temporary/$archive" | awk '{print $1}')
else
    fatal "sha256sum or shasum is required to verify the download."
fi

[ "$actual" = "$expected" ] || fatal "The downloaded archive failed checksum verification."

tar -xzf "$temporary/$archive" -C "$temporary"
[ -x "$temporary/nonaut" ] || fatal "The release archive did not contain a Nonaut binary."
"$temporary/nonaut" --version >/dev/null 2>&1 \
    || fatal "The downloaded Nonaut binary could not run on this system."

install_binary "$temporary/nonaut"
record_install "release" "$version" "$INSTALL_FLAVOR"

success "Installed Nonaut $version to $destination"
show_path_hint
