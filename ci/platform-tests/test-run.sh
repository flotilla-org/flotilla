#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
python3 -m unittest discover -s "$repo_root/ci/platform-tests" -p test_run.py
