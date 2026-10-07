#!/usr/bin/env bash
# Execute selected tests without evaluating shell code from the selector file.
set -euo pipefail
# Regex variables also work with the original Bash 3.2 parser.
job_pattern='^(windows|macos|tender-ssh)$'
scope_pattern='^(all|windows|macos|tender-ssh)$'
comment_pattern='^[[:space:]]*(#|$)'
if [[ $# != 1 || ! "$1" =~ $job_pattern ]]; then
  echo 'usage: run.sh <windows|macos|tender-ssh>' >&2
  exit 2
fi
job=$1
repo_root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
commands=()
while IFS= read -r line || [[ -n "$line" ]]; do
  line=${line%$'\r'}
  [[ "$line" =~ $comment_pattern ]] && continue
  if [[ "$line" != *'|'* ]]; then
    echo "invalid selector: $line" >&2; exit 2
  fi
  scope=${line%%|*}
  args=${line#*|}
  if [[ ! "$scope" =~ $scope_pattern || -z "${args//[[:space:]]/}" || "$args" == *'|'* ]]; then
    echo "invalid selector: $line" >&2; exit 2
  fi
  if [[ "$scope" == "$job" || ( "$scope" == all && "$job" != tender-ssh ) ]]; then
    commands+=("$args")
  fi
done < "$repo_root/ci/platform-tests/selectors.txt"
# Validate the entire file before launching any tests; failures stop the job.
# Bash 3.2 treats an empty array as unset under nounset.
python3 "$repo_root/ci/platform-tests/execute.py" ${commands[@]+"${commands[@]}"}
