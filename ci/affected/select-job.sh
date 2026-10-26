#!/usr/bin/env bash
# Shared PR selection for Test and Clippy; other events retain full coverage.
set -euo pipefail

selection=all
# Non-PR events, malformed stack data and label lookup failures all
# keep full coverage. A stack's top layer covers the whole stack.
affected=false
if [ "$EVENT" = pull_request ] && git rev-parse --verify --quiet HEAD^1 >/dev/null; then
  if [ "$STACK" = null ]; then
    affected=true
  elif jq -e '.position != null and .size != null and .position < .size' <<<"$STACK" >/dev/null; then
    affected=true
  fi
  if ! labels=$(gh pr view "$PR" --repo "$GITHUB_REPOSITORY" --json labels --jq '.labels[].name') \
    || printf '%s\n' "$labels" | grep -qx 'ci:full'; then
    affected=false
  fi
fi
if [ "$affected" = true ]; then
  git -c core.quotePath=false diff --no-renames --name-only HEAD^1 HEAD > "$RUNNER_TEMP/changed-files"
  selection=$(python3 ci/affected/selection.py "$RUNNER_TEMP/changed-files")
fi
echo "Selected: $selection"
echo "selection=$selection" >> "$GITHUB_OUTPUT"
