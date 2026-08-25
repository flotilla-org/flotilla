#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
workflow="$repo_root/.forgejo/workflows/fleet-candidates.yml"
builder="$repo_root/ci/fleet-candidates/build-candidate.sh"
setup="$repo_root/ci/fleet-candidates/setup-linux-toolchain.sh"

bash -n "$builder" "$setup"

grep -Fq 'workflow_dispatch:' "$workflow"
if grep -Eq '^[[:space:]]+(push|pull_request):' "$workflow"; then
  echo 'fleet candidate workflow must be manual-only' >&2
  exit 1
fi
grep -Fq 'runs-on: debian-12' "$workflow"
grep -Fq 'runs-on: darwin-aarch64' "$workflow"
# The literals below are workflow syntax and shell source, not expressions for
# this contract test to expand.
# shellcheck disable=SC2016
grep -Fq 'FLEET_ORCHESTRATION_SHA: ${{ forgejo.sha }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'FLEET_MATTPOCOCK_SKILLS_SHA: ${{ inputs.mattpocock_skills_sha }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'FLEET_RJW_SKILLS_SHA: ${{ inputs.rjw_skills_sha }}' "$workflow"
test "$(grep -Fc 'type: string' "$workflow")" -eq 4
# shellcheck disable=SC2016
test "$(grep -Fc 'git -C orchestration fetch --depth=1 origin "$FLEET_ORCHESTRATION_SHA"' "$workflow")" -eq 2
if grep -F 'git -C orchestration fetch' "$workflow" | grep -Fq 'FLEET_FLOTILLA_SHA'; then
  echo 'orchestration checkout must not use the selected Flotilla source SHA' >&2
  exit 1
fi
grep -Fq 'retention-days: 7' "$workflow"
grep -Fq '"schema_version": 4' "$builder"
grep -Fq '"paths": ["plugins/rjw-sdlc/skills"]' "$builder"
grep -Fq '"name": "cleat"' "$builder"
grep -Fq '"revision": os.environ["FLEET_CLEAT_SHA"]' "$builder"
grep -Fq '"name": "flotilla"' "$builder"
grep -Fq '"revision": os.environ["FLEET_FLOTILLA_SHA"]' "$builder"
if grep -Fq '"required_skills"' "$builder"; then
  echo 'skill manifest production must carry no universal required-skill list' >&2
  exit 1
fi
grep -Fq 'actions/cache/restore@6f8efc29b200d32929f49075959781ed54ec270c' "$workflow"
grep -Fq 'actions/cache/save@6f8efc29b200d32929f49075959781ed54ec270c' "$workflow"
test "$(grep -Fc 'actions/upload-artifact@a8a3f3ad30e3422c9c7b888a15615d19a852ae32' "$workflow")" -eq 2
if grep -Fq 'repository: ${{ forgejo.repository_owner }}/' "$workflow"; then
  echo 'fleet candidate workflow cannot use its repository-scoped token for private sibling repositories' >&2
  exit 1
fi

# shellcheck disable=SC1090,SC1091
source "$builder"
valid_sha=0123456789abcdef0123456789abcdef01234567
test "$(require_sha TEST_SHA "$valid_sha")" = "$valid_sha"
if require_sha TEST_SHA main >/dev/null 2>&1; then
  echo 'accepted a floating source ref' >&2
  exit 1
fi

skill_test_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-skill-paths.XXXXXX")"
trap 'rm -rf "$skill_test_root"' EXIT
skill_repo="$skill_test_root/source"
git init --quiet "$skill_repo"
git -C "$skill_repo" config user.name test
git -C "$skill_repo" config user.email test@example.invalid
mkdir -p "$skill_repo/skills/example"
printf '# Example\n' >"$skill_repo/skills/example/SKILL.md"
git -C "$skill_repo" add .
git -C "$skill_repo" commit --quiet -m fixture
skill_pin="$(git -C "$skill_repo" rev-parse HEAD)"
skill_manifest="$skill_test_root/sources.json"
python3 - "$skill_manifest" "$skill_repo" "$skill_pin" <<'PY'
import json
import sys

manifest, repository, revision = sys.argv[1:]
sources = []
for name in ("mattpocock-skills", "rjw-skills", "cleat", "flotilla"):
    source = {"name": name, "repository": repository, "revision": revision}
    if name == "mattpocock-skills":
        source["repository"] = "https://github.com/flotilla-org/mattpocock-skills.git"
    if name == "rjw-skills":
        source["paths"] = ["missing/skills"]
    sources.append(source)
with open(manifest, "w") as output:
    json.dump({"schema_version": 4, "sources": sources}, output)
PY
git_config="$skill_test_root/gitconfig"
git config --file "$git_config" "url.file://$skill_repo.insteadOf" https://github.com/flotilla-org/mattpocock-skills.git
if GIT_CONFIG_GLOBAL="$git_config" python3 "$repo_root/ci/fleet-candidates/generation_validation.py" \
    skill-sources "$skill_manifest" >"$skill_test_root/stdout" 2>"$skill_test_root/stderr"; then
  echo 'accepted a declared skill path absent at its pinned revision' >&2
  exit 1
fi
expected="skill source rjw-skills declared path missing/skills is missing at pinned revision $skill_pin"
grep -Fq "$expected" "$skill_test_root/stderr"
if grep -Fq 'remote: Enumerating objects' "$skill_test_root/stderr"; then
  echo 'skill path validation leaked git fetch chatter' >&2
  exit 1
fi

fake_flotilla="$(mktemp "${TMPDIR:-/tmp}/fake-flotilla.XXXXXX")"
printf '#!/bin/sh\nprintf "flotilla 0.1.0 (wire=test, proto=20)\\n"\n' >"$fake_flotilla"
chmod 0755 "$fake_flotilla"
test "$(read_protocol_version "$fake_flotilla")" = 20
printf '#!/bin/sh\nprintf "flotilla 0.1.0 (wire=test)\\n"\n' >"$fake_flotilla"
if read_protocol_version "$fake_flotilla" >/dev/null 2>&1; then
  echo 'accepted a binary that did not report its protocol version' >&2
  exit 1
fi
rm -f "$fake_flotilla"

echo 'fleet candidate workflow contract passed'
"$repo_root/ci/fleet-candidates/test-generation-validation.sh"
