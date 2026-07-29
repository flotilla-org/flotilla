#!/usr/bin/env bash
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"

expected_assets=()
while [[ $# -gt 0 && "$1" == --expect ]]; do
  if [[ $# -lt 2 ]]; then
    echo "--expect requires an asset name" >&2
    exit 2
  fi
  expected_assets+=("$2")
  shift 2
done
if [[ ${1:-} != -- ]]; then
  echo "usage: $0 --expect <asset-name>... -- <asset>..." >&2
  exit 2
fi
shift
if [[ ${#expected_assets[@]} -eq 0 || $# -eq 0 ]]; then
  echo "usage: $0 --expect <asset-name>... -- <asset>..." >&2
  exit 2
fi
publish_assets=("$@")
wait_attempts=${FLEET_RELEASE_WAIT_ATTEMPTS:-31}
if [[ ! "$wait_attempts" =~ ^[1-9][0-9]*$ ]]; then
  echo "FLEET_RELEASE_WAIT_ATTEMPTS must be a positive integer" >&2
  exit 1
fi

release_tag="fleet-$GITHUB_SHA"
if [[ ! "$GITHUB_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "GITHUB_REPOSITORY must be an owner/name pair: $GITHUB_REPOSITORY" >&2
  exit 1
fi
if [[ ! "$GITHUB_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  echo "GITHUB_SHA must be a full lowercase SHA-1: $GITHUB_SHA" >&2
  exit 1
fi
for expected_asset in "${expected_assets[@]}"; do
  if [[ "$expected_asset" != "$(basename "$expected_asset")" ||
    ! "$expected_asset" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "expected release asset name contains unsupported characters: $expected_asset" >&2
    exit 1
  fi
done
for asset in "${publish_assets[@]}"; do
  if [[ ! -f "$asset" ]]; then
    echo "asset does not exist: $asset" >&2
    exit 1
  fi
  asset_name=$(basename "$asset")
  if [[ ! "$asset_name" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "release asset name contains unsupported characters: $asset_name" >&2
    exit 1
  fi
  is_expected=false
  for expected_asset in "${expected_assets[@]}"; do
    if [[ "$asset_name" == "$expected_asset" ]]; then
      is_expected=true
      break
    fi
  done
  if [[ "$is_expected" != true ]]; then
    echo "local asset is absent from the expected release manifest: $asset_name" >&2
    exit 1
  fi
done

if ! gh release view "$release_tag" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  if ! gh release create "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --target "$GITHUB_SHA" \
    --title "Fleet artifacts ${GITHUB_SHA:0:12}" \
    --notes "Immutable fleet artifacts built from ${GITHUB_REPOSITORY}@${GITHUB_SHA} by GitHub Actions." \
    --draft \
    --latest=false; then
    gh release view "$release_tag" --repo "$GITHUB_REPOSITORY" >/dev/null
  fi
fi

target_commitish=$(gh release view "$release_tag" \
  --repo "$GITHUB_REPOSITORY" \
  --json targetCommitish \
  --jq '.targetCommitish')
if [[ "$target_commitish" != "$GITHUB_SHA" ]]; then
  echo "release $release_tag targets $target_commitish, not $GITHUB_SHA" >&2
  exit 1
fi
is_draft=$(gh release view "$release_tag" \
  --repo "$GITHUB_REPOSITORY" \
  --json isDraft \
  --jq '.isDraft')
download_dir=$(mktemp -d)
trap 'rm -rf "$download_dir"' EXIT

for asset in "${publish_assets[@]}"; do
  asset_name=$(basename "$asset")
  if ! gh release view "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --json assets \
    --jq '.assets[].name' | grep -Fxq "$asset_name"; then
    if [[ "$is_draft" != true ]]; then
      echo "published release is missing required asset: $asset_name" >&2
      exit 1
    fi
    gh release upload "$release_tag" "$asset" --repo "$GITHUB_REPOSITORY"
    continue
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

for ((attempt = 1; attempt <= wait_attempts; attempt++)); do
  missing_assets=()
  for expected_asset in "${expected_assets[@]}"; do
    if ! gh release view "$release_tag" \
      --repo "$GITHUB_REPOSITORY" \
      --json assets \
      --jq '.assets[].name' | grep -Fxq "$expected_asset"; then
      missing_assets+=("$expected_asset")
    fi
  done
  if [[ ${#missing_assets[@]} -eq 0 || "$is_draft" != true ]]; then
    break
  fi
  if [[ $attempt -lt $wait_attempts ]]; then
    sleep 2
  fi
done
if [[ ${#missing_assets[@]} -gt 0 ]]; then
  if [[ "$is_draft" != true ]]; then
    printf 'published release is missing required asset: %s\n' "${missing_assets[@]}" >&2
    exit 1
  fi
  printf 'draft release is waiting for asset: %s\n' "${missing_assets[@]}"
  exit 0
fi

if [[ "$is_draft" == true ]]; then
  if ! gh release edit "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --draft=false \
    --latest=false; then
    current_draft=$(gh release view "$release_tag" \
      --repo "$GITHUB_REPOSITORY" \
      --json isDraft \
      --jq '.isDraft')
    [[ "$current_draft" == false ]]
  fi
fi
