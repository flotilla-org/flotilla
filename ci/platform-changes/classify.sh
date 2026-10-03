#!/usr/bin/env bash
# Decide whether the Windows client and macOS peer-identity CI jobs need to run.
#
# Usage: classify.sh <event> <full-label> <changed-files> <diff>
#   event          GitHub event name (pull_request, merge_group, push, ...)
#   full-label     "true" when the pull request carries the ci:full label
#   changed-files  file listing changed paths, one per line
#   diff           file holding the unified diff of those changes
#
# Prints `windows=<true|false>` and `macos=<true|false>`, suitable for
# appending to $GITHUB_OUTPUT. Pushes to main and ci:full run both jobs; other
# events run a job only when its inputs changed.
set -euo pipefail

event=$1
full_label=$2
changed_files=$3
diff=$4

if [[ "$event" == "push" || "$full_label" == "true" ]]; then
  echo "windows=true"
  echo "macos=true"
  exit 0
fi

# Shared inputs: the toolchain, the lockfile, the workflow and this classifier.
shared='^(Cargo\.(toml|lock)|rust-toolchain\.toml|\.github/workflows/ci\.yml|ci/platform-changes/)'

# The Windows client job builds `flotilla` and tests its client, transport and
# Wheelhouse sink. Its platform seams live in these paths; elsewhere, a change
# reaches Windows only through platform-conditional code, matched in the diff.
windows_paths="${shared}|^(src/|crates/flotilla-client/|crates/flotilla-transport/|crates/flotilla-manifest/src/sink|crates/flotilla-tui/src/(terminal|run|cli))"
windows_content='^[+-].*(cfg\(.*(windows|unix|target_os|target_family)|std::os::(unix|windows))'

# The macOS job runs the daemon's peer-identity tests (server::caller).
macos_paths="${shared}|^(crates/flotilla-daemon/src/server/caller|crates/flotilla-transport/)"
macos_content='^[+-].*(target_os *= *"macos"|target_vendor *= *"apple")'

decide() {
  local paths=$1 content=$2
  if grep -qE "$paths" "$changed_files"; then
    echo true
  elif grep -E '^[+-]' "$diff" | grep -vE '^(\+\+\+|---) ' | grep -qE "$content"; then
    echo true
  else
    echo false
  fi
}

echo "windows=$(decide "$windows_paths" "$windows_content")"
echo "macos=$(decide "$macos_paths" "$macos_content")"
