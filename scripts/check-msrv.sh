#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

toolchain=${MSRV_TOOLCHAIN:-1.85.0}

if ! command -v rustup >/dev/null 2>&1; then
  echo "rustup is required to verify MSRV $toolchain" >&2
  exit 1
fi

if ! rustup toolchain list | awk '{print $1}' | grep -Eq "^${toolchain}(-|$)"; then
  echo "Rust $toolchain is not installed; run: rustup toolchain install $toolchain --profile minimal" >&2
  exit 1
fi

rustup run "$toolchain" cargo check --all-targets --all-features --locked
rustup run "$toolchain" cargo test --all-targets --locked
