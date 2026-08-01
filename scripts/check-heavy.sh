#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

MINICORE_HEAVY_TESTS=1 cargo test --test heavy_boundaries --features heavy-tests --locked
python3 docs/fixtures/wire-v1/verify.py
