#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
install_root="${WTM_INSTALL_ROOT:-$HOME/.local}"
if ! command -v cargo >/dev/null 2>&1; then
  printf 'Rust 1.88 or newer is required. Install it from https://rustup.rs.\n' >&2
  exit 1
fi
if ! command -v git >/dev/null 2>&1; then
  printf 'Git 2.36 or newer is required.\n' >&2
  exit 1
fi
git_version="$(git --version | awk '{print $3}')"
if ! printf '%s\n' "$git_version" | awk -F. '{ exit !($1 > 2 || ($1 == 2 && $2 >= 36)) }'; then
  printf 'Git 2.36 or newer is required; found %s.\n' "$git_version" >&2
  exit 1
fi
cargo install --path "$project_dir" --locked --root "$install_root"
printf 'Installed wtm in %s/bin. Add that directory to PATH if needed.\n' "$install_root"
