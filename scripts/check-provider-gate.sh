#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

# Standalone provider evidence, outside the runtime gate.
cargo fmt --manifest-path provider-gate/Cargo.toml -- --check
cargo test --manifest-path provider-gate/Cargo.toml --all-targets --locked
cargo clippy --manifest-path provider-gate/Cargo.toml --all-targets --locked -- -D warnings
