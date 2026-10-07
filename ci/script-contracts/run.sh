#!/usr/bin/env bash
# Module selectors are data, never shell code.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"
modules=()
while IFS= read -r module || [[ -n "$module" ]]; do
  [[ "$module" =~ ^[[:space:]]*(#|$) ]] && continue
  if [[ ! "$module" =~ ^[a-zA-Z_][a-zA-Z_0-9]*(\.[a-zA-Z_][a-zA-Z_0-9]*)+$ ]]; then
    echo "invalid script contract selector: $module" >&2
    exit 2
  fi
  modules+=("$module")
done < ci/script-contracts/selectors.txt
python3 -m unittest "${modules[@]}"
