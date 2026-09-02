#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

python3 scripts/check_docs.py --self-test
python3 scripts/check_docs.py
python3 scripts/check_v04_architecture.py --self-test
python3 scripts/check_v04_architecture.py