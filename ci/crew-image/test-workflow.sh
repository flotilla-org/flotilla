#!/usr/bin/env bash
# Retained entry point for the existing CI job; production builds are host-local.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test ! -e "$repo_root/.forgejo/workflows/crew-image.yml"
test ! -e "$repo_root/.flotilla/crew-image-baseline.yaml"
python3 "$repo_root/ci/crew-image/test-tea-auth.py"
python3 "$repo_root/ci/crew-image/test-compose.py"
echo 'crew image composition contract passed'
