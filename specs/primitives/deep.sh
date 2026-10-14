#!/usr/bin/env bash
# The provider is an executable implementing start/run/collect/stop. See README.
set -euo pipefail
provider=$(realpath -- "${1:?usage: deep.sh PROVIDER [OUTPUT_DIRECTORY]}")
out=$(realpath -m -- "${2:-$(dirname "$0")/build/deep-results}")
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
scratch=$(mktemp -d)
allocation="quint-3220-${scratch##*.}"
mkdir -p "$out"
run_reached=false
cleanup() {
  result=$?
  trap - EXIT
  if "$run_reached"; then
    timeout --kill-after=10s "${DEEP_CLEANUP_SECONDS:-120}s" "$provider" collect "$allocation" "$out" || { echo 'deep result collection failed' >&2; result=1; }
  fi
  timeout --kill-after=10s "${DEEP_CLEANUP_SECONDS:-120}s" "$provider" stop "$allocation" || { echo 'deep allocation teardown failed' >&2; result=1; }
  if [ -f "$scratch/source.tgz" ]; then rm -- "$scratch/source.tgz"; fi
  rm -rf -- "$scratch"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Only source and lockfiles travel; no checkout caches, credentials, or generated outputs.
tar --exclude=target --exclude=build --exclude=_apalache-out --exclude=__pycache__ -czf "$scratch/source.tgz" -C "$repo" \
  rust-toolchain.toml rustfmt.toml prototypes/3108-3209 specs/primitives
timeout --kill-after=10s "${DEEP_START_SECONDS:-600}s" "$provider" start "$allocation"
run_reached=true
timeout --kill-after=10s "${DEEP_RUN_SECONDS:-900}s" "$provider" run "$allocation" "$scratch/source.tgz"
