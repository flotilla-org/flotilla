#!/usr/bin/env bash
# Run on an operator host with Docker, enforced memory/swap limits, and a
# public pinned image containing git. No live Docker acceptance runs in CI.
set -euo pipefail
if [[ $# -ne 3 || "$3" != *@sha256:* ]]; then
    echo "Usage: $0 /path/to/repo branch registry/repo@sha256:digest" >&2
    exit 2
fi
cargo run --locked -p flotilla-core --example environment_checkout -- "$1" "$2" "$3"
