#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
workflow="$repo_root/.forgejo/workflows/crew-image.yml"
dockerfile="$repo_root/.flotilla/Dockerfile.crew"

grep -Fq 'workflow_dispatch:' "$workflow"
if grep -Eq '^[[:space:]]+(push|pull_request):' "$workflow"; then
  echo 'crew image workflow must be manual-only' >&2
  exit 1
fi

dockerfile_arg() {
  sed -n "s/^ARG ${1}=\(.*\)\$/\1/p" "$dockerfile" | head -n1
}

workflow_input_default() {
  grep -A5 "^      ${1}:\$" "$workflow" | grep 'default:' | sed -E "s/.*default: '([^']*)'.*/\1/"
}

# The workflow's dispatch-input defaults are the deployment's "current pins" —
# keep them equal to the Dockerfile's own ARG defaults so this test fails the
# moment they drift, rather than relying on someone noticing by eye.
for pair in \
  'codex_version:CODEX_VERSION' \
  'claude_code_version:CLAUDE_CODE_VERSION' \
  'tea_version:TEA_VERSION' \
  'cleat_ref:CLEAT_REF' \
  'zig_version:ZIG_VERSION'; do
  input="${pair%%:*}"
  arg="${pair##*:}"
  workflow_value="$(workflow_input_default "$input")"
  dockerfile_value="$(dockerfile_arg "$arg")"
  test -n "$workflow_value"
  test -n "$dockerfile_value"
  if [ "$workflow_value" != "$dockerfile_value" ]; then
    echo "crew image workflow input '$input' default ($workflow_value) must match Dockerfile.crew ARG $arg ($dockerfile_value)" >&2
    exit 1
  fi
done

grep -Fq 'runs-on: crew-image-builder' "$workflow"
grep -Fqe '--platform linux/amd64,linux/arm64' "$workflow"
grep -Fq '.flotilla/Dockerfile.crew' "$workflow"
grep -Fqe '--push \' "$workflow"
grep -Fq 'CREW_IMAGE_REPOSITORY: forgejo.lab.flotilla.work/image-builder/flotilla-crew' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'secrets.IMAGE_BUILDER_TOKEN' "$workflow"
grep -Fq 'claude --version' "$workflow"
grep -Fq 'codex --version' "$workflow"
grep -Fq 'tea --version' "$workflow"

if grep -Eq ':(latest|"latest"|'"'"'latest'"'"')[[:space:]]*$' "$workflow"; then
  echo 'crew image workflow must never tag latest — the tag is the deployment contract' >&2
  exit 1
fi

# The pushed tag must be date/sha-derived, not a hand-maintained sequence
# number that would need a registry round-trip to compute.
grep -Fq "date -u +%Y-%m-%d" "$workflow"
grep -Fq 'FORGEJO_SHA' "$workflow"

echo 'crew image workflow contract passed'
