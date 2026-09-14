#!/bin/sh
# Bootstrap Rness from a GitHub source archive. Run with: curl ... | sh
set -eu

repository=${RNESS_REPOSITORY:-Abraxas-365/rness}
ref=${RNESS_REF:-main}

usage() {
  cat <<'EOF'
Usage: bootstrap.sh [install.sh options]

Downloads the selected Rness source archive and runs its source installer.

Environment:
  RNESS_REPOSITORY=OWNER/REPO  GitHub repository (default: Abraxas-365/rness)
  RNESS_REF=REF               Branch, tag, or commit without slashes (default: main)
  RNESS_INSTALL_RUST=1        Install Rust with rustup when Cargo is unavailable

All remaining arguments are passed to install.sh, for example:
  --replace-binary
  --experimental-control     Build opt-in experimental local submission support
  --bin-dir DIR
  --config-dir DIR
  --binary FILE
EOF
}

case "$repository" in
  *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._/-]*|/*|*//*|*/|"")
    printf 'Invalid RNESS_REPOSITORY: %s\n' "$repository" >&2
    exit 2
    ;;
esac
case "$ref" in
  *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-]*|"")
    printf 'Invalid RNESS_REF: %s\n' "$ref" >&2
    exit 2
    ;;
esac

for arg in "$@"; do
  case "$arg" in
    -h|--help) usage; exit 0 ;;
  esac
done

command -v curl >/dev/null 2>&1 || {
  printf 'curl is required to download Rness.\n' >&2
  exit 1
}
command -v tar >/dev/null 2>&1 || {
  printf 'tar is required to unpack Rness.\n' >&2
  exit 1
}
command -v bash >/dev/null 2>&1 || {
  printf 'Bash is required by the source installer.\n' >&2
  exit 1
}

has_binary=false
previous=""
for arg in "$@"; do
  if [ "$previous" = "--binary" ]; then has_binary=true; fi
  previous=$arg
done
if [ "$has_binary" = false ] && ! command -v cargo >/dev/null 2>&1; then
  if [ "${RNESS_INSTALL_RUST:-}" = "1" ]; then
    printf '%s\n' 'Cargo was not found; installing Rust with rustup.'
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
  else
    cat >&2 <<'EOF'
Cargo is required to build Rness from source.
Install Rust first (https://rustup.rs), or rerun with RNESS_INSTALL_RUST=1.
A native C/C++ build toolchain is also required for embedded Lua.
EOF
    exit 1
  fi
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/rness-install.XXXXXX") || exit 1
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
archive="$temporary/rness.tar.gz"
url="https://codeload.github.com/$repository/tar.gz/$ref"

printf 'Downloading Rness (%s at %s)...\n' "$repository" "$ref"
curl --proto '=https' --tlsv1.2 -sSfL "$url" -o "$archive"
tar -xzf "$archive" -C "$temporary"

checkout=""
for candidate in "$temporary"/*; do
  if [ -d "$candidate" ] && [ -f "$candidate/install.sh" ] && [ -d "$candidate/flavors/default" ]; then
    if [ -n "$checkout" ]; then
      printf 'Downloaded archive contains multiple Rness source checkouts.\n' >&2
      exit 1
    fi
    checkout=$candidate
  fi
done
if [ -z "$checkout" ]; then
  printf 'Downloaded archive does not contain a valid Rness source checkout.\n' >&2
  exit 1
fi

bash "$checkout/install.sh" "$@"
