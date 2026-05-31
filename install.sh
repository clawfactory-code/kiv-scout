#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Install kiv-scout from this checkout.

Usage:
  ./install.sh [--dir DIR] [--update-shell | --no-shell]

Options:
  --dir DIR        Install into DIR instead of auto-selecting a user bin dir.
  --update-shell  Add the install dir to your shell startup file if needed.
  --no-shell      Do not prompt or update shell startup files.
  -h, --help      Show this help.
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

path_contains() {
  case ":${PATH:-}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

display_path() {
  case "$1" in
    "$HOME"/*) printf '$HOME/%s' "${1#"$HOME"/}" ;;
    "$HOME") printf '$HOME' ;;
    *) printf '%s' "$1" ;;
  esac
}

choose_install_dir() {
  for dir in "$HOME/.local/bin" "$HOME/bin" "$HOME/.cargo/bin"; do
    if path_contains "$dir"; then
      printf '%s\n' "$dir"
      return
    fi
  done
  printf '%s\n' "$HOME/.local/bin"
}

shell_startup_file() {
  case "$(basename "${SHELL:-}")" in
    zsh) printf '%s\n' "$HOME/.zshrc" ;;
    bash) printf '%s\n' "$HOME/.bashrc" ;;
    *) printf '%s\n' "$HOME/.profile" ;;
  esac
}

append_path_to_shell() {
  local dir="$1"
  local rc_file
  local dir_expr
  local line
  rc_file="$(shell_startup_file)"
  dir_expr="$(display_path "$dir")"
  line="export PATH=\"$dir_expr:\$PATH\""

  mkdir -p "$(dirname "$rc_file")"
  touch "$rc_file"

  if grep -Fq "$line" "$rc_file"; then
    printf 'PATH already configured in %s\n' "$rc_file"
    return
  fi

  {
    printf '\n# Added by Kiv Scout installer\n'
    printf '%s\n' "$line"
  } >>"$rc_file"

  printf 'Added %s to PATH in %s\n' "$dir_expr" "$rc_file"
}

install_dir="${KIV_SCOUT_INSTALL_DIR:-}"
shell_mode="auto"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir)
      [ "$#" -ge 2 ] || die "--dir requires a path"
      install_dir="$2"
      shift 2
      ;;
    --update-shell)
      shell_mode="yes"
      shift
      ;;
    --no-shell)
      shell_mode="no"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

command -v cargo >/dev/null 2>&1 || die "cargo is required. Install Rust from https://rustup.rs/ first."

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${install_dir:-$(choose_install_dir)}"
mkdir -p "$install_dir"

printf 'Building kiv-scout...\n'
cargo build --release --locked --manifest-path "$script_dir/Cargo.toml"

install -m 0755 "$script_dir/target/release/kiv-scout" "$install_dir/kiv-scout"
printf 'Installed kiv-scout to %s\n' "$install_dir/kiv-scout"

if ! path_contains "$install_dir"; then
  dir_expr="$(display_path "$install_dir")"
  if [ "$shell_mode" = "yes" ] || [ "${KIV_SCOUT_INSTALL_UPDATE_SHELL:-}" = "1" ]; then
    append_path_to_shell "$install_dir"
  elif [ "$shell_mode" = "auto" ] && [ -t 0 ] && [ -t 1 ]; then
    printf '%s is not on PATH. Add it to your shell startup file? [y/N] ' "$dir_expr"
    read -r reply
    case "$reply" in
      y | Y | yes | YES) append_path_to_shell "$install_dir" ;;
    esac
  fi

  if ! path_contains "$install_dir"; then
    printf 'For this shell, run:\n'
    printf '  export PATH="%s:$PATH"\n' "$dir_expr"
  fi
fi

if command -v kiv-scout >/dev/null 2>&1; then
  resolved="$(command -v kiv-scout)"
  if [ "$resolved" != "$install_dir/kiv-scout" ]; then
    printf 'Note: current PATH resolves kiv-scout to %s\n' "$resolved"
  fi
  kiv-scout --version
else
  printf 'Open a new shell, then run:\n'
  printf '  kiv-scout --version\n'
fi
