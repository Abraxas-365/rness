#!/usr/bin/env bash
# Install from this checkout. No sudo, shell-profile edits, or credential setup.
set -euo pipefail

repo=$(CDPATH= cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin_dir="${HOME:?HOME must be set}/.local/bin"
config_dir="$HOME/.rness"
binary=""
replace_binary=false
experimental_control=false
usage() {
  printf '%s\n' 'Usage: ./install.sh [--bin-dir DIR] [--config-dir DIR] [--binary FILE] [--replace-binary] [--experimental-control]' \
    'Builds the release binary with Cargo unless --binary is supplied.' \
    'Copies the default flavor only when the configuration directory does not exist.' \
    'Existing configuration is never merged, replaced, or deleted.' \
    '--replace-binary explicitly permits replacing an existing rness executable.' \
    '--experimental-control builds the experimental local submission API (off by default).'
}
while (($#)); do
  case "$1" in
    --bin-dir|--config-dir|--binary)
      if (($# < 2)) || [[ -z "$2" ]]; then usage >&2; exit 2; fi
      case "$1" in
        --bin-dir) bin_dir="$2" ;;
        --config-dir) config_dir="$2" ;;
        --binary) binary="$2" ;;
      esac
      shift 2 ;;
    --experimental-control) experimental_control=true; shift ;;
    --replace-binary) replace_binary=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

# Resolve relative destinations before invoking build commands.
[[ "$bin_dir" = /* ]] || bin_dir="$PWD/$bin_dir"
[[ "$config_dir" = /* ]] || config_dir="$PWD/$config_dir"
destination="$bin_dir/rness"
if [[ -e "$destination" || -L "$destination" ]]; then
  if [[ "$replace_binary" != true ]]; then
    printf 'Binary already exists: %s. Use --replace-binary to upgrade; configuration stays untouched.\n' "$destination" >&2
    exit 1
  fi
  if [[ ! -f "$destination" || -L "$destination" ]]; then
    printf 'Refusing to replace non-regular file or symlink: %s\n' "$destination" >&2
    exit 1
  fi
fi
[[ -f "$repo/flavors/default/init.lua" ]] || { printf 'Default flavor is missing.\n' >&2; exit 1; }
if [[ -z "$binary" ]]; then
  command -v cargo >/dev/null || { printf 'Install Rust/Cargo first, or supply --binary FILE.\n' >&2; exit 1; }
  # An explicit target directory avoids guessing Cargo workspace overrides.
  features=(--no-default-features)
  if [[ "$experimental_control" == true ]]; then features+=(--features experimental-control); fi
  cargo build --manifest-path "$repo/Cargo.toml" --locked --release --package rness-cli --target-dir "$repo/target" "${features[@]}"
  binary="$repo/target/release/rness"
fi
[[ -f "$binary" && -x "$binary" ]] || { printf 'Not an executable file: %s\n' "$binary" >&2; exit 1; }
# A prebuilt binary must match the installation opt-in; do not silently enable it.
help_output=$("$binary" --help) || { printf 'Could not inspect binary capabilities (--help failed).\n' >&2; exit 2; }
if [[ "$help_output" == *--control-socket* ]]; then
  if [[ "$experimental_control" != true ]]; then
    printf 'This binary includes experimental control. Pass --experimental-control explicitly.\n' >&2; exit 2
  fi
elif [[ "$experimental_control" == true ]]; then
  printf 'Supplied binary lacks experimental control support.\n' >&2; exit 2
fi
mkdir -p "$bin_dir"
staged=$(mktemp "$bin_dir/.rness-install.XXXXXX")
trap 'rm -f "$staged"' EXIT
install -m 755 "$binary" "$staged"
if [[ "$replace_binary" == true ]]; then
  mv -f "$staged" "$destination"
else
  # Atomic no-clobber publication if another install created the destination.
  ln "$staged" "$destination"
fi
if [[ -e "$config_dir" || -L "$config_dir" ]]; then
  printf 'Preserved existing configuration: %s\n' "$config_dir"
else
  mkdir -p "$(dirname "$config_dir")"
  # mkdir claims a new directory without ever writing into an existing one.
  if mkdir -m 700 "$config_dir"; then
    cp -R "$repo/flavors/default/." "$config_dir/"
    printf 'Installed default flavor: %s\n' "$config_dir"
  else
    printf 'Could not create configuration directory; binary installed but flavor was not copied: %s\n' "$config_dir" >&2
    exit 1
  fi
fi
printf 'Installed binary: %s\n' "$destination"
printf '%s\n' 'No credentials or principal model were selected.' \
  'Configure providers and small.by_provider in your config lua/providers.lua.' \
  'The default small profile includes Anthropic Haiku; other connections need explicit mappings.' \
  'Start with: rness --model <provider>/<model>'
case ":${PATH:-}:" in *":$bin_dir:"*) ;; *) printf 'Add this directory to PATH: %s\n' "$bin_dir" ;; esac
