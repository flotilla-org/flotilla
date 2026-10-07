#!/usr/bin/env bash
# Execute selected tests without evaluating shell code from the selector file.
set -euo pipefail
if [[ $# != 1 || ! "$1" =~ ^(windows|macos|tender-ssh)$ ]]; then
  echo 'usage: run.sh <windows|macos|tender-ssh>' >&2
  exit 2
fi
job=$1
repo_root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
commands=()
while IFS= read -r line || [[ -n "$line" ]]; do
  line=${line%$'\r'}
  [[ "$line" =~ ^[[:space:]]*(#|$) ]] && continue
  if [[ "$line" != *'|'* ]]; then
    echo "invalid selector: $line" >&2; exit 2
  fi
  scope=${line%%|*}
  args=${line#*|}
  if [[ ! "$scope" =~ ^(all|windows|macos|tender-ssh)$ || -z "${args//[[:space:]]/}" || "$args" == *'|'* ]]; then
    echo "invalid selector: $line" >&2; exit 2
  fi
  if [[ "$scope" == "$job" || ( "$scope" == all && "$job" != tender-ssh ) ]]; then
    commands+=("$args")
  fi
done < "$repo_root/ci/platform-tests/selectors.txt"
# Validate the entire file before launching any tests; failures stop the job.
for command in "${commands[@]}"; do
  read -r -a args <<< "$command"
  cargo --config 'profile.dev.package."*".debug=0' test "${args[@]}"
done
