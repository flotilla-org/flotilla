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
python3 "$repo_root/ci/crew-image/test-tea-auth.py"

if grep -Eq ':(latest|"latest"|'"'"'latest'"'"')[[:space:]]*$' "$workflow"; then
  echo 'crew image workflow must never tag latest — the tag is the deployment contract' >&2
  exit 1
fi

# The pushed tag must be date/sha-derived, not a hand-maintained sequence
# number that would need a registry round-trip to compute, and must also
# fold in the dispatch inputs so two same-day, same-commit dispatches with
# different versions can never collide on one tag.
grep -Fq "date -u +%Y-%m-%d" "$workflow"
grep -Fq 'FORGEJO_SHA' "$workflow"
grep -Fq 'input_hash' "$workflow"
grep -Fq 'sha256sum' "$workflow"

# Dispatch inputs are free-form workflow_dispatch strings. They must be
# passed through env: and referenced as "$VAR" in run: scripts, never
# interpolated as "${{ inputs.* }}" directly into --build-arg text — the
# latter is a script-injection hole on a runner that also holds the
# just-logged-in registry credential.
if grep -Ee '--build-arg [A-Z_]+="\$\{\{ *inputs\.' "$workflow"; then
  echo 'dispatch inputs must not be interpolated directly into --build-arg text; pass them through env: and reference as "$VAR"' >&2
  exit 1
fi
for var in CODEX_VERSION CLAUDE_CODE_VERSION TEA_VERSION CLEAT_REF ZIG_VERSION; do
  grep -Fq "${var}: \${{ inputs." "$workflow"
done

# The registry credential must not outlive the job on this host-execution
# runner: a docker logout cleanup step must run even if the build fails.
grep -Fq 'docker logout forgejo.lab.flotilla.work' "$workflow"
logout_line="$(grep -Fn 'docker logout forgejo.lab.flotilla.work' "$workflow" | head -n1 | cut -d: -f1)"
sed -n "$((logout_line - 5)),${logout_line}p" "$workflow" | grep -Fq 'if: always()'

echo 'crew image workflow contract passed'
