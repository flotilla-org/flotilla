#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 5 || $# -gt 7 ]]; then
  echo "usage: $0 <artifact> <metadata> <repository> <commit-sha> <platform> [wire-generation] [signed]" >&2
  exit 2
fi

artifact=$1
metadata=$2
repository=$3
commit_sha=$4
platform=$5
wire_generation=${6:-}
signed=${7:-false}

if [[ ! -f "$artifact" ]]; then
  echo "artifact does not exist: $artifact" >&2
  exit 1
fi
if [[ ! "$commit_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "commit sha must be a full lowercase SHA-1: $commit_sha" >&2
  exit 1
fi
if [[ "$signed" != true && "$signed" != false ]]; then
  echo "signed must be true or false" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256=$(sha256sum "$artifact" | awk '{print $1}')
else
  sha256=$(shasum -a 256 "$artifact" | awk '{print $1}')
fi
size_bytes=$(wc -c <"$artifact" | tr -d '[:space:]')

jq_args=(
  --arg repository "$repository"
  --arg commit_sha "$commit_sha"
  --arg platform "$platform"
  --arg artifact "$(basename "$artifact")"
  --arg sha256 "$sha256"
  --argjson size_bytes "$size_bytes"
  --argjson signed "$signed"
)

if [[ -n "$wire_generation" ]]; then
  jq -n "${jq_args[@]}" --arg wire_generation "$wire_generation" '{
    schema_version: 1,
    repository: $repository,
    commit_sha: $commit_sha,
    wire_generation: $wire_generation,
    platform: $platform,
    artifact: $artifact,
    sha256: $sha256,
    size_bytes: $size_bytes,
    signed: $signed
  }' >"$metadata"
else
  jq -n "${jq_args[@]}" '{
    schema_version: 1,
    repository: $repository,
    commit_sha: $commit_sha,
    platform: $platform,
    artifact: $artifact,
    sha256: $sha256,
    size_bytes: $size_bytes,
    signed: $signed
  }' >"$metadata"
fi
