#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixtures="$root/fixtures"
tools=(
  "$root/lab-fleet-promote"
  "$root/lab-fleet-finalize-darwin"
  "$root/lab-darwin-sign"
)
for tool in "${tools[@]}"; do
  python3 "$tool" --validate-fixture "$fixtures/valid.json"
  for fixture in bad-pin bad-skill-path traversing-skill-path unexpected-payload source-set-mismatch v3-bundle-violation; do
    if python3 "$tool" --validate-fixture "$fixtures/$fixture.json" >/dev/null 2>&1; then
      echo "$(basename "$tool") accepted invalid fixture $fixture" >&2
      exit 1
    fi
  done
done

FLEET_GENERATION_VALIDATOR="$root/generation_validation.py" "$root/../../scripts/fleet-install" __validate_fixture "$fixtures/valid.json"
for fixture in bad-pin bad-skill-path traversing-skill-path unexpected-payload source-set-mismatch v3-bundle-violation; do
  if FLEET_GENERATION_VALIDATOR="$root/generation_validation.py" "$root/../../scripts/fleet-install" __validate_fixture "$fixtures/$fixture.json" >/dev/null 2>&1; then
    echo "fleet-install accepted invalid fixture $fixture" >&2
    exit 1
  fi
done

install_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-validator-layout.XXXXXX")"
trap 'rm -rf "$install_root"' EXIT
cp "$root/../../scripts/fleet-install" "$root/generation_validation.py" "$install_root/"
PATH="$(dirname "$(command -v python3)"):$PATH" "$install_root/fleet-install" __validate_fixture "$fixtures/valid.json"

echo "generation validator parity passed"
