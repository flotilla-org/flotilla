#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT

artifact="$test_dir/artifact"
metadata="$test_dir/artifact.json"
printf 'fleet artifact\n' >"$artifact"
commit_sha=0123456789abcdef0123456789abcdef01234567
release_tag=fleet-$commit_sha

"$repo_root/scripts/write-artifact-metadata.sh" \
  "$artifact" "$metadata" flotilla-org/flotilla "$commit_sha" \
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
  '.schema_version == 2
    and .repository == "flotilla-org/flotilla"
    and .commit_sha == $commit_sha
    and .release_tag == $release_tag
    and .wire_generation == "0123456789ab"
    and .platform == "linux-x86_64"
    and .artifact == "artifact"
    and .artifact_url == ("https://github.com/flotilla-org/flotilla/releases/download/" + $release_tag + "/artifact")
    and .metadata_asset == "artifact.json"
    and .metadata_url == ("https://github.com/flotilla-org/flotilla/releases/download/" + $release_tag + "/artifact.json")
    and .sha256 == $sha256
    and .size_bytes == 15
    and .signed == false' \
  "$metadata" >/dev/null

if "$repo_root/scripts/write-artifact-metadata.sh" \
  "$artifact" "$metadata" flotilla-org/flotilla not-a-sha \
  fleet-not-a-sha linux-x86_64 \
  >/dev/null 2>&1; then
  echo "metadata helper accepted an invalid commit SHA" >&2
  exit 1
fi

stub_bin="$test_dir/bin"
mkdir "$stub_bin"
ln -s "$repo_root/scripts/test-support/fake-github-gh" "$stub_bin/gh"
gh_log="$test_dir/gh.log"
gh_state="$test_dir/gh-state"

publish() {
  env \
    PATH="$stub_bin:$PATH" \
    GH_TOKEN=secret \
    GITHUB_REPOSITORY=flotilla-org/flotilla \
    GITHUB_SHA="$commit_sha" \
    FAKE_GH_STATE_DIR="$gh_state" \
    FAKE_GH_LOG="$gh_log" \
    "$repo_root/scripts/publish-github-release.sh" "$artifact"
}

publish >/dev/null
cmp -s "$artifact" "$gh_state/assets/artifact"
grep -Fxq create:artifact "$gh_log"

: >"$gh_log"
publish >/dev/null
grep -Fxq download:artifact "$gh_log"
if grep -Fq create: "$gh_log"; then
  echo "idempotent publication recreated an existing release" >&2
  exit 1
fi

printf 'different bytes\n' >"$artifact"
if publish >/dev/null 2>&1; then
  echo "publisher accepted conflicting release bytes" >&2
  exit 1
fi

if env \
  PATH="$stub_bin:$PATH" \
  GH_TOKEN=secret \
  GITHUB_REPOSITORY=flotilla-org/flotilla \
  GITHUB_SHA="$commit_sha" \
  GITHUB_RELEASE_TAG=invalid-tag \
  FAKE_GH_STATE_DIR="$gh_state" \
  FAKE_GH_LOG="$gh_log" \
  "$repo_root/scripts/publish-github-release.sh" "$artifact" \
  >/dev/null 2>&1; then
  echo "publisher accepted an invalid release tag" >&2
  exit 1
fi
