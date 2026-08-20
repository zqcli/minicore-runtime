#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --manifest-path provider-gate/Cargo.toml -- --check
cargo test --manifest-path provider-gate/Cargo.toml --all-targets --locked
cargo clippy --manifest-path provider-gate/Cargo.toml --all-targets --locked -- -D warnings
python3 scripts/check_docs.py

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
git show --check --format= HEAD
