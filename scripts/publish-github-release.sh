#!/usr/bin/env bash
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <asset>..." >&2
  exit 2
fi

release_tag=${GITHUB_RELEASE_TAG:-fleet-$GITHUB_SHA}
if [[ ! "$GITHUB_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "GITHUB_REPOSITORY must be an owner/name pair: $GITHUB_REPOSITORY" >&2
  exit 1
fi
if [[ ! "$GITHUB_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  echo "GITHUB_SHA must be a full lowercase SHA-1: $GITHUB_SHA" >&2
  exit 1
fi
if [[ "$release_tag" != "fleet-$GITHUB_SHA" ]]; then
  echo "release tag must be fleet-<GITHUB_SHA>: $release_tag" >&2
  exit 1
fi

for asset in "$@"; do
  if [[ ! -f "$asset" ]]; then
    echo "asset does not exist: $asset" >&2
    exit 1
  fi
  asset_name=$(basename "$asset")
  if [[ ! "$asset_name" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "release asset name contains unsupported characters: $asset_name" >&2
    exit 1
  fi
done

if gh release view "$release_tag" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  target_commitish=$(gh release view "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --json targetCommitish \
    --jq '.targetCommitish')
  if [[ "$target_commitish" != "$GITHUB_SHA" ]]; then
    echo "release $release_tag targets $target_commitish, not $GITHUB_SHA" >&2
    exit 1
  fi
  download_dir=$(mktemp -d)
  trap 'rm -rf "$download_dir"' EXIT

  for asset in "$@"; do
    asset_name=$(basename "$asset")
    if ! gh release view "$release_tag" \
      --repo "$GITHUB_REPOSITORY" \
      --json assets \
      --jq '.assets[].name' | grep -Fxq "$asset_name"; then
      echo "existing release is missing required asset: $asset_name" >&2
      exit 1
    fi
    gh release download "$release_tag" \
      --repo "$GITHUB_REPOSITORY" \
      --pattern "$asset_name" \
      --dir "$download_dir" \
      --clobber
    if ! cmp -s "$asset" "$download_dir/$asset_name"; then
      echo "refusing to replace immutable release asset: $asset_name" >&2
      exit 1
    fi
    echo "release asset already exists with identical bytes: $asset_name"
  done
else
  gh release create "$release_tag" "$@" \
    --repo "$GITHUB_REPOSITORY" \
    --target "$GITHUB_SHA" \
    --title "Fleet artifacts ${GITHUB_SHA:0:12}" \
    --notes "Immutable fleet artifacts built from ${GITHUB_REPOSITORY}@${GITHUB_SHA} by GitHub Actions." \
    --latest=false
fi
