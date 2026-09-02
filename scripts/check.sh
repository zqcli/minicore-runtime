#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

# Main crate gates (stable-only for the whole mainstream anyway).
cargo fmt --all -- --check
cargo test --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features

# Standalone provider evidence harness (its own Rust version contract; source
# is not part of this crate and is never modified by this gate).
cargo fmt --manifest-path provider-gate/Cargo.toml -- --check
cargo test --manifest-path provider-gate/Cargo.toml --all-targets --locked
cargo clippy --manifest-path provider-gate/Cargo.toml --all-targets --locked -- -D warnings

# Docs and v0.4 architecture gates.
bash scripts/check-architecture.sh

# MSRV is deliberately a separate opt-in script (needs a pinned toolchain).
# Run it explicitly: ./scripts/check-msrv.sh

if ! command -v rg >/dev/null 2>&1; then
  echo "rg is required by the quality gate" >&2
  exit 1
fi

if ! repository_files=$(rg --files); then
  echo "rg failed while enumerating repository files" >&2
  exit 1
fi

set +e
archive_files=$(printf '%s\n' "$repository_files" | rg '^docs/archive/')
archive_status=$?
set -e

if ((archive_status > 1)); then
  echo "rg failed while checking archive exclusion" >&2
  exit 1
fi

if ((archive_status == 0)) || [[ -n "$archive_files" ]]; then
  echo "docs/archive must be excluded from default rg results" >&2
  exit 1
fi

git diff --check
git diff --cached --check
git show --check --format= HEAD >/dev/null