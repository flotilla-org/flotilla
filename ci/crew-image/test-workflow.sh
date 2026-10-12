#!/usr/bin/env bash
# Retained entry point for the existing CI job; production builds are host-local.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 "$repo_root/ci/crew-image/test-tea-auth.py"
python3 "$repo_root/ci/crew-image/test-compose.py"
echo 'crew image composition contract passed'
