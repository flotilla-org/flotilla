#!/usr/bin/env bash
set -euo pipefail

bundle_root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$bundle_root/../.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT

publisher="$bundle_root/runtime/publish-forgejo-release.sh"
metadata_writer="$bundle_root/runtime/write-artifact-metadata.sh"
fake_curl="$bundle_root/tests/fake-forgejo-curl"

bash -n "$publisher"
bash -n "$metadata_writer"
bash -n "$bundle_root/runtime/cleat/prepare-ghostty-vt.sh"
bash -n "$bundle_root/runtime/cleat/macos-zig-sdk-shim/xcrun"
bash -n "$fake_curl"

artifact="$test_dir/artifact"
metadata="$test_dir/artifact.json"
printf 'fleet artifact\n' >"$artifact"
commit_sha=0123456789abcdef0123456789abcdef01234567
release_tag=fleet-$commit_sha

env \
  FLEET_SERVER_URL=https://forge.example.test \
  FLEET_API_URL=https://forge.example.test/api/v1 \
  "$metadata_writer" \
  "$artifact" "$metadata" lab/flotilla "$commit_sha" \
  "$release_tag" linux-x86_64 0123456789ab false

if command -v sha256sum >/dev/null 2>&1; then
  expected_sha256=$(sha256sum "$artifact" | awk '{print $1}')
else
  expected_sha256=$(shasum -a 256 "$artifact" | awk '{print $1}')
fi

jq -e \
  --arg commit_sha "$commit_sha" \
  --arg release_tag "$release_tag" \
  --arg sha256 "$expected_sha256" \
  '.schema_version == 3
    and .repository == "lab/flotilla"
    and .commit_sha == $commit_sha
    and .release_tag == $release_tag
    and .release_web_url == ("https://forge.example.test/lab/flotilla/releases/tag/" + $release_tag)
    and .release_api_url == ("https://forge.example.test/api/v1/repos/lab/flotilla/releases/tags/" + $release_tag)
    and .wire_generation == "0123456789ab"
    and .platform == "linux-x86_64"
    and .artifact == "artifact"
    and .metadata_asset == "artifact.json"
    and .sha256 == $sha256
    and .size_bytes == 15
    and .signed == false' \
  "$metadata" >/dev/null

if env \
  FLEET_SERVER_URL=https://forge.example.test \
  FLEET_API_URL=https://forge.example.test/api/v1 \
  "$metadata_writer" \
  "$artifact" "$metadata" lab/flotilla not-a-sha \
  fleet-not-a-sha linux-x86_64 \
  >/dev/null 2>&1; then
  echo "metadata helper accepted an invalid commit SHA" >&2
  exit 1
fi

stub_bin="$test_dir/bin"
mkdir "$stub_bin"
ln -s "$fake_curl" "$stub_bin/curl"
api_log="$test_dir/api.log"
api_state="$test_dir/api-state"

publish() {
  env \
    PATH="$stub_bin:$PATH" \
    FORGEJO_TOKEN=secret \
    FLEET_API_URL=https://download.invalid/api/v1 \
    FLEET_REPOSITORY=lab/flotilla \
    FLEET_COMMIT_SHA="$commit_sha" \
    FAKE_FORGEJO_STATE_DIR="$api_state" \
    FAKE_FORGEJO_LOG="$api_log" \
    "$publisher" \
    --expect artifact -- "$artifact"
}

publish >/dev/null
cmp -s "$artifact" "$api_state/assets/artifact"
grep -Fxq create-release "$api_log"
grep -Fxq upload:artifact "$api_log"
grep -Fxq publish-draft "$api_log"

: >"$api_log"
publish >/dev/null
grep -Fxq download:artifact "$api_log"
if grep -Fq create-release "$api_log"; then
  echo "idempotent publication recreated an existing release" >&2
  exit 1
fi

printf 'different bytes\n' >"$artifact"
if publish >/dev/null 2>&1; then
  echo "publisher accepted conflicting release bytes" >&2
  exit 1
fi

cohort_state="$test_dir/cohort-state"
printf 'first asset\n' >"$test_dir/first"
printf 'second asset\n' >"$test_dir/second"
: >"$api_log"
env \
  PATH="$stub_bin:$PATH" \
  FORGEJO_TOKEN=secret \
  FLEET_API_URL=https://download.invalid/api/v1 \
  FLEET_REPOSITORY=lab/flotilla \
  FLEET_COMMIT_SHA="$commit_sha" \
  FLEET_RELEASE_WAIT_ATTEMPTS=1 \
  FAKE_FORGEJO_STATE_DIR="$cohort_state" \
  FAKE_FORGEJO_LOG="$api_log" \
  "$publisher" \
  --expect first --expect second -- "$test_dir/first" >/dev/null
cmp -s "$test_dir/first" "$cohort_state/assets/first"
grep -Fxq true "$cohort_state/draft"
if grep -Fxq publish-draft "$api_log"; then
  echo "publisher published before every expected cohort arrived" >&2
  exit 1
fi

: >"$api_log"
env \
  PATH="$stub_bin:$PATH" \
  FORGEJO_TOKEN=secret \
  FLEET_API_URL=https://download.invalid/api/v1 \
  FLEET_REPOSITORY=lab/flotilla \
  FLEET_COMMIT_SHA="$commit_sha" \
  FAKE_FORGEJO_STATE_DIR="$cohort_state" \
  FAKE_FORGEJO_LOG="$api_log" \
  "$publisher" \
  --expect first --expect second -- "$test_dir/second" >/dev/null
cmp -s "$test_dir/second" "$cohort_state/assets/second"
grep -Fxq publish-draft "$api_log"
grep -Fxq false "$cohort_state/draft"

rm "$cohort_state/assets/second"
if env \
  PATH="$stub_bin:$PATH" \
  FORGEJO_TOKEN=secret \
  FLEET_API_URL=https://download.invalid/api/v1 \
  FLEET_REPOSITORY=lab/flotilla \
  FLEET_COMMIT_SHA="$commit_sha" \
  FAKE_FORGEJO_STATE_DIR="$cohort_state" \
  FAKE_FORGEJO_LOG="$api_log" \
  "$publisher" \
  --expect first --expect second -- \
  "$test_dir/first" "$test_dir/second" >/dev/null 2>&1; then
  echo "publisher repaired an incomplete published release" >&2
  exit 1
fi

printf 'race candidate\n' >"$test_dir/race"
conflict_race_state="$test_dir/conflict-race-state"
if env \
  PATH="$stub_bin:$PATH" \
  FORGEJO_TOKEN=secret \
  FLEET_API_URL=https://download.invalid/api/v1 \
  FLEET_REPOSITORY=lab/flotilla \
  FLEET_COMMIT_SHA="$commit_sha" \
  FAKE_FORGEJO_STATE_DIR="$conflict_race_state" \
  FAKE_FORGEJO_LOG="$api_log" \
  FAKE_FORGEJO_UPLOAD_RACE_MODE=conflict \
  "$publisher" \
  --expect race -- "$test_dir/race" >/dev/null 2>&1; then
  echo "publisher accepted conflicting bytes from a racing uploader" >&2
  exit 1
fi

identical_race_state="$test_dir/identical-race-state"
env \
  PATH="$stub_bin:$PATH" \
  FORGEJO_TOKEN=secret \
  FLEET_API_URL=https://download.invalid/api/v1 \
  FLEET_REPOSITORY=lab/flotilla \
  FLEET_COMMIT_SHA="$commit_sha" \
  FAKE_FORGEJO_STATE_DIR="$identical_race_state" \
  FAKE_FORGEJO_LOG="$api_log" \
  FAKE_FORGEJO_UPLOAD_RACE_MODE=identical \
  "$publisher" \
  --expect race -- "$test_dir/race" >/dev/null
grep -Fxq false "$identical_race_state/draft"

workflow_count=$(find "$bundle_root/workflows" -type f -name '*.yml' | wc -l | tr -d '[:space:]')
if [[ "$workflow_count" != 7 ]]; then
  echo "expected seven inert workflow templates, found $workflow_count" >&2
  exit 1
fi

forbidden_workflow_pattern='forgejo\.lab|udder|comte|feta|GH_TOKEN|github\.token|upload-artifact|download-artifact|publish-github'
# shellcheck disable=SC2016 # Intentional literal workflow expression fixture.
for forbidden_fixture in 'https://forgejo.lab.example/repo' '${{ github.token }}'; do
  if ! grep -Eqi "$forbidden_workflow_pattern" <<<"$forbidden_fixture"; then
    echo "forbidden workflow pattern did not catch: $forbidden_fixture" >&2
    exit 1
  fi
done

while IFS= read -r workflow; do
  if [[ "$(grep -c 'Publishing adapter:' "$workflow")" != 1 ]]; then
    echo "workflow must contain exactly one publishing adapter marker: $workflow" >&2
    exit 1
  fi
  if [[ "$(grep -c 'FORGEJO_TOKEN:' "$workflow")" != 1 ]]; then
    echo "workflow must expose the Forgejo token exactly once: $workflow" >&2
    exit 1
  fi
  if ! awk '
    /Publishing adapter:/ { after_adapter = 1; next }
    after_adapter && /^[[:space:]]+- name:/ { names_after_adapter++ }
    END { exit names_after_adapter == 1 ? 0 : 1 }
  ' "$workflow"; then
    echo "publishing adapter must be the final workflow step: $workflow" >&2
    exit 1
  fi
  while IFS= read -r action_reference; do
    if [[ ! "$action_reference" =~ ^https:// ]]; then
      echo "action reference is not fully qualified in $workflow: $action_reference" >&2
      exit 1
    fi
  done < <(sed -n 's/^[[:space:]]*uses:[[:space:]]*//p' "$workflow")
  if grep -Eqi "$forbidden_workflow_pattern" "$workflow"; then
    echo "workflow contains a forbidden provider/private value: $workflow" >&2
    exit 1
  fi
done < <(find "$bundle_root/workflows" -type f -name '*.yml' | sort)

if find "$repo_root/.github/workflows" -maxdepth 1 -type f -name 'publish-fleet-*.yml' | grep -q .; then
  echo "public GitHub release workflow is active" >&2
  exit 1
fi

echo "fork action contracts passed"
