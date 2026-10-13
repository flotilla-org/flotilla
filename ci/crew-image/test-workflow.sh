#!/usr/bin/env bash
# Retained entry point for the existing CI job. Crew images are built by the
# daemon's build service from stdlib/image-layers (ADR 0062); this checks the
# crew-utilities tea wrapper without a live Forgejo server and that the Codex
# harness pin matches its recorded hook contract.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 "$repo_root/ci/crew-image/test-tea-auth.py"
python3 "$repo_root/ci/crew-image/test-harness-pins.py"
echo 'crew image stdlib contracts passed'
