#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

toolchain=${MSRV_TOOLCHAIN:-1.85.0}

rustup_bin=$(command -v rustup || true)
if [[ -z "$rustup_bin" && -x "$HOME/.cargo/bin/rustup" ]]; then
  rustup_bin="$HOME/.cargo/bin/rustup"
fi
if [[ -z "$rustup_bin" ]]; then
  echo "rustup is required to verify MSRV $toolchain" >&2
  exit 1
fi

if ! "$rustup_bin" toolchain list | awk '{print $1}' | grep -Eq "^${toolchain}(-|$)"; then
  echo "Rust $toolchain is not installed; run: rustup toolchain install $toolchain --profile minimal" >&2
  exit 1
fi

"$rustup_bin" run "$toolchain" cargo check --all-targets --all-features --locked
"$rustup_bin" run "$toolchain" cargo test --all-targets --locked
