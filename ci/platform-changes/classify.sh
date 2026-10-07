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
# appending to $GITHUB_OUTPUT. Pull requests and merge groups run a job only
# when its inputs changed; every other event, and ci:full, runs both.
set -euo pipefail

event=$1
full_label=$2
changed_files=$3
diff=$4

# Only pull requests and merge groups carry a diff base; every other event
# (push to main, workflow_dispatch, schedule, ...) runs both jobs.
if [[ ( "$event" != "pull_request" && "$event" != "merge_group" ) || "$full_label" == "true" ]]; then
  echo "windows=true"
  echo "macos=true"
  exit 0
fi

# Shared inputs: the toolchain, the lockfile, the workflow and this classifier.
# Crate manifests count too: a new unix-only dependency breaks Windows without
# touching any Rust source.
shared='^(Cargo\.(toml|lock)|crates/[^/]+/Cargo\.toml|rust-toolchain\.toml|\.github/workflows/ci\.yml|ci/(platform-changes|platform-tests|toolchain)/)'

# The Windows client job builds and runs `flotilla` and tests its client,
# transport and Wheelhouse sink. Its platform seams live in these paths, plus
# the CLI definitions whose unoptimised stack frames must fit Windows' 1 MiB
# main thread (#2588). Elsewhere, a change reaches Windows only through
# platform-conditional code, matched in the diff.
windows_paths="${shared}|^(src/|crates/flotilla-client/|crates/flotilla-commands/|crates/flotilla-transport/|crates/flotilla-manifest/src/sink|crates/flotilla-tui/src/(terminal|run|cli|pm_connect))"
windows_content='^[+-].*(cfg(_attr|!)?\(.*(windows|unix|target_os|target_family|target_vendor)|std::os::(unix|windows)|libc::|nix::|Unix(Stream|Listener|Datagram)|::unix::|pre_exec|setsid)'

# The macOS job runs the daemon's peer-identity tests (server::caller).
macos_paths="${shared}|^(crates/flotilla-daemon/src/server/caller|crates/flotilla-transport/)"
macos_content='^[+-].*(target_os *= *"macos"|target_vendor *= *"apple")'

decide() {
  local paths=$1 content=$2
  if grep -qE "$paths" "$changed_files"; then
    echo true
  # The content patterns anchor on added or removed lines. The final grep
  # reads its whole input rather than using -q: an early exit would SIGPIPE
  # the writer and, under pipefail, turn a match into a miss on large diffs.
  elif grep -vE '^(\+\+\+|---) ' "$diff" | grep -E "$content" >/dev/null; then
    echo true
  else
    echo false
  fi
}

echo "windows=$(decide "$windows_paths" "$windows_content")"
echo "macos=$(decide "$macos_paths" "$macos_content")"
