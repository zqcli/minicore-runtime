#!/usr/bin/env bash
# MSRV gate for the main minicore-runtime crate only.
#
# The pinned toolchain's real rustc/cargo are resolved through rustup and the
# rustc release is verified against MSRV_TOOLCHAIN, so a Homebrew/PATH rustc
# or a rustc wrapper can never fake a green run. The provider-gate package is
# deliberately NOT exercised here: it is a standalone stable-only evidence
# harness whose own manifest declares Rust 1.88, and Rig is rejected from the
# production baseline rather than hidden behind an MSRV exclusion.
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

rustc_path=$("$rustup_bin" which rustc --toolchain "$toolchain")
cargo_path=$("$rustup_bin" which cargo --toolchain "$toolchain")

# Parse the numeric release from `rustc --version` output, e.g.
# "rustc 1.85.0 (4d91de4e4 2025-02-17)" -> 1.85.0.
release=$("$rustc_path" --version)
rustc_version=${release#rustc }
rustc_version=${rustc_version%% *}

# Normalize the expected version to major.minor.patch for numeric comparison.
if [[ "$toolchain" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  expected="$toolchain"
elif [[ "$toolchain" =~ ^([0-9]+)\.([0-9]+)$ ]]; then
  expected="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.0"
else
  echo "MSRV_TOOLCHAIN must be a numeric Rust version, got: $toolchain" >&2
  exit 1
fi

if [[ "$rustc_version" != "$expected" ]]; then
  echo "MSRV toolchain mismatch: $rustc_path reports $rustc_version, expected $expected (MSRV_TOOLCHAIN=$toolchain)" >&2
  exit 1
fi

# Force the pinned compiler and drop any wrapper that could substitute one.
unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
export RUSTC="$rustc_path"

# Isolated target dir: never reuse an ambient CARGO_TARGET_DIR.
target_dir=${MSRV_TARGET_DIR:-"$root/target/msrv-$toolchain"}
export CARGO_TARGET_DIR="$target_dir"
mkdir -p "$target_dir"

echo "MSRV $toolchain: $rustc_path ($release), target dir: $target_dir"

"$cargo_path" check --all-targets --all-features --locked
"$cargo_path" test --all-targets --locked
