#!/usr/bin/env bash
set -euo pipefail

: "${FLEET_SERVER_URL:?FLEET_SERVER_URL is required}"
: "${FLEET_API_URL:?FLEET_API_URL is required}"

if [[ $# -lt 6 || $# -gt 8 ]]; then
  echo "usage: $0 <artifact> <metadata> <repository> <commit-sha> <release-tag> <platform> [wire-generation] [signed]" >&2
  exit 2
fi

artifact=$1
metadata=$2
repository=$3
commit_sha=$4
release_tag=$5
platform=$6
wire_generation=${7:-}
signed=${8:-false}

if [[ ! -f "$artifact" ]]; then
  echo "artifact does not exist: $artifact" >&2
  exit 1
fi
if [[ ! "$commit_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "commit sha must be a full lowercase SHA-1: $commit_sha" >&2
  exit 1
fi
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "repository must be an owner/name pair: $repository" >&2
  exit 1
fi
if [[ "$release_tag" != "fleet-$commit_sha" ]]; then
  echo "release tag must be fleet-<commit-sha>: $release_tag" >&2
  exit 1
fi
if [[ "$signed" != true && "$signed" != false ]]; then
  echo "signed must be true or false" >&2
  exit 1
fi
for url_name in FLEET_SERVER_URL FLEET_API_URL; do
  url=${!url_name}
  if [[ ! "$url" =~ ^https://[^/[:space:]]+(/[^[:space:]]*)?$ ]]; then
    echo "$url_name must be an HTTPS URL without whitespace" >&2
    exit 1
  fi
done

artifact_name=$(basename "$artifact")
metadata_name=$(basename "$metadata")
for name in "$artifact_name" "$metadata_name"; do
  if [[ ! "$name" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "release asset name contains unsupported characters: $name" >&2
    exit 1
  fi
done

if command -v sha256sum >/dev/null 2>&1; then
  sha256=$(sha256sum "$artifact" | awk '{print $1}')
else
  sha256=$(shasum -a 256 "$artifact" | awk '{print $1}')
fi
size_bytes=$(wc -c <"$artifact" | tr -d '[:space:]')
server_url=${FLEET_SERVER_URL%/}
api_url=${FLEET_API_URL%/}
release_web_url="$server_url/$repository/releases/tag/$release_tag"
release_api_url="$api_url/repos/$repository/releases/tags/$release_tag"

jq_args=(
  --arg repository "$repository"
  --arg commit_sha "$commit_sha"
  --arg release_tag "$release_tag"
  --arg release_web_url "$release_web_url"
  --arg release_api_url "$release_api_url"
  --arg platform "$platform"
  --arg artifact "$artifact_name"
  --arg metadata_asset "$metadata_name"
  --arg sha256 "$sha256"
  --argjson size_bytes "$size_bytes"
  --argjson signed "$signed"
)

if [[ -n "$wire_generation" ]]; then
  jq -n "${jq_args[@]}" --arg wire_generation "$wire_generation" '{
    schema_version: 3,
    repository: $repository,
    commit_sha: $commit_sha,
    release_tag: $release_tag,
    release_web_url: $release_web_url,
    release_api_url: $release_api_url,
    wire_generation: $wire_generation,
    platform: $platform,
    artifact: $artifact,
    metadata_asset: $metadata_asset,
    sha256: $sha256,
    size_bytes: $size_bytes,
    signed: $signed
  }' >"$metadata"
else
  jq -n "${jq_args[@]}" '{
    schema_version: 3,
    repository: $repository,
    commit_sha: $commit_sha,
    release_tag: $release_tag,
    release_web_url: $release_web_url,
    release_api_url: $release_api_url,
    platform: $platform,
    artifact: $artifact,
    metadata_asset: $metadata_asset,
    sha256: $sha256,
    size_bytes: $size_bytes,
    signed: $signed
  }' >"$metadata"
fi
