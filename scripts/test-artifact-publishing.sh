#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT

artifact="$test_dir/artifact"
metadata="$test_dir/artifact.json"
printf 'fleet artifact\n' >"$artifact"
commit_sha=0123456789abcdef0123456789abcdef01234567

"$repo_root/scripts/write-artifact-metadata.sh" \
  "$artifact" "$metadata" flotilla-org/flotilla "$commit_sha" \
  linux-x86_64 0123456789ab false

if command -v sha256sum >/dev/null 2>&1; then
  expected_sha256=$(sha256sum "$artifact" | awk '{print $1}')
else
  expected_sha256=$(shasum -a 256 "$artifact" | awk '{print $1}')
fi

jq -e \
  --arg commit_sha "$commit_sha" \
  --arg sha256 "$expected_sha256" \
  '.schema_version == 1
    and .repository == "flotilla-org/flotilla"
    and .commit_sha == $commit_sha
    and .wire_generation == "0123456789ab"
    and .platform == "linux-x86_64"
    and .artifact == "artifact"
    and .sha256 == $sha256
    and .size_bytes == 15
    and .signed == false' \
  "$metadata" >/dev/null

if "$repo_root/scripts/write-artifact-metadata.sh" \
  "$artifact" "$metadata" flotilla-org/flotilla not-a-sha linux-x86_64 \
  >/dev/null 2>&1; then
  echo "metadata helper accepted an invalid commit SHA" >&2
  exit 1
fi

stub_bin="$test_dir/bin"
mkdir "$stub_bin"
ln -s "$repo_root/scripts/test-support/fake-forgejo-curl" "$stub_bin/curl"
curl_log="$test_dir/curl.log"
uploaded="$test_dir/uploaded"

publish() {
  env \
    PATH="$stub_bin:$PATH" \
    FORGEJO_SERVER_URL=https://forgejo.example \
    FORGEJO_PACKAGE_OWNER=robert \
    FORGEJO_PACKAGE_NAME=flotilla \
    FORGEJO_PACKAGE_VERSION="$commit_sha-0123456789ab" \
    FORGEJO_PACKAGE_TOKEN=secret \
    FAKE_CURL_STATUS="$1" \
    FAKE_CURL_LOG="$curl_log" \
    FAKE_CURL_REMOTE="${2:-$artifact}" \
    FAKE_CURL_UPLOADED="$uploaded" \
    "$repo_root/scripts/publish-forgejo-generic.sh" "$artifact"
}

publish 404 >/dev/null
cmp -s "$artifact" "$uploaded"
grep -Fxq upload "$curl_log"

: >"$curl_log"
publish 200 "$artifact" >/dev/null
grep -Fxq get:200 "$curl_log"
if grep -Fxq upload "$curl_log"; then
  echo "idempotent publication uploaded an existing artifact" >&2
  exit 1
fi

printf 'different bytes\n' >"$test_dir/different"
if publish 200 "$test_dir/different" >/dev/null 2>&1; then
  echo "publisher accepted conflicting immutable bytes" >&2
  exit 1
fi

if env \
  FORGEJO_SERVER_URL=https://forgejo.example \
  FORGEJO_PACKAGE_OWNER=robert \
  FORGEJO_PACKAGE_NAME=flotilla \
  FORGEJO_PACKAGE_VERSION=invalid/version \
  FORGEJO_PACKAGE_TOKEN=secret \
  "$repo_root/scripts/publish-forgejo-generic.sh" "$artifact" \
  >/dev/null 2>&1; then
  echo "publisher accepted an invalid package version" >&2
  exit 1
fi
