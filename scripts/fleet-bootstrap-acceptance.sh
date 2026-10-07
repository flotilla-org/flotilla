#!/usr/bin/env bash
set -euo pipefail

# Operator-only acceptance on feta, using the synced bootstrap/validator pair.
# Stage and probe a finalized generation without changing the active generation.
if [[ $# -ne 2 ]]; then
  echo 'usage: fleet-bootstrap-acceptance.sh <fleet-install-path> <generation>' >&2
  exit 2
fi
if [[ "$(hostname -s)" != feta ]]; then
  echo 'fleet-bootstrap-acceptance: run this script on feta' >&2
  exit 1
fi
bootstrap="$1"
bootstrap_dir="$(cd "$(dirname "$bootstrap")" && pwd)"
validator="$bootstrap_dir/generation_validation.py"
[[ -r "$validator" ]] || {
  echo "fleet-bootstrap-acceptance: sync generation_validation.py beside $bootstrap" >&2
  exit 1
}
# Override an inherited emergency bypass and select the adjacent reviewed module.
FLEET_INSTALL_SKIP_CANARY=0 FLEET_GENERATION_VALIDATOR="$validator" \
  bash "$bootstrap" --canary "$2"
