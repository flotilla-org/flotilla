#!/usr/bin/env bash
set -euo pipefail

: "${FORGEJO_TOKEN:?FORGEJO_TOKEN is required}"
: "${FLEET_API_URL:?FLEET_API_URL is required}"
: "${FLEET_REPOSITORY:?FLEET_REPOSITORY is required}"
: "${FLEET_COMMIT_SHA:?FLEET_COMMIT_SHA is required}"

if [[ "$FORGEJO_TOKEN" == *$'\n'* || "$FORGEJO_TOKEN" == *$'\r'* ||
  "$FORGEJO_TOKEN" == *'"'* || "$FORGEJO_TOKEN" == *\\* ]]; then
  echo "FORGEJO_TOKEN contains characters unsafe for curl configuration" >&2
  exit 1
fi

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

release_tag="fleet-$FLEET_COMMIT_SHA"
if [[ ! "$FLEET_API_URL" =~ ^https://[^/[:space:]]+(/[^[:space:]]*)?$ ]]; then
  echo "FLEET_API_URL must be an HTTPS URL without whitespace" >&2
  exit 1
fi
if [[ ! "$FLEET_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "FLEET_REPOSITORY must be an owner/name pair: $FLEET_REPOSITORY" >&2
  exit 1
fi
if [[ ! "$FLEET_COMMIT_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  echo "FLEET_COMMIT_SHA must be a full lowercase SHA-1: $FLEET_COMMIT_SHA" >&2
  exit 1
fi
for expected_asset in "${expected_assets[@]}"; do
  if [[ "$expected_asset" != "$(basename "$expected_asset")" ||
    ! "$expected_asset" =~ ^[A-Za-z0-9._-]+$ ]]; then
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
  if [[ ! "$asset_name" =~ ^[A-Za-z0-9._-]+$ ]]; then
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

work_dir=$(mktemp -d)
auth_config="$work_dir/curl-auth"
chmod 700 "$work_dir"
printf 'header = "Authorization: token %s"\n' "$FORGEJO_TOKEN" >"$auth_config"
chmod 600 "$auth_config"
trap 'rm -rf "$work_dir"' EXIT

api_url=${FLEET_API_URL%/}
if [[ "$api_url" =~ ^(https://[^/]+) ]]; then
  forge_origin=${BASH_REMATCH[1]}
else
  echo "could not determine Forgejo origin from FLEET_API_URL" >&2
  exit 1
fi
repo_api="$api_url/repos/$FLEET_REPOSITORY"
release_by_tag_url="$repo_api/releases/tags/$release_tag"
response_body="$work_dir/response.json"

request() {
  local method=$1
  local url=$2
  local output=$3
  shift 3
  curl --silent --show-error --location \
    --config "$auth_config" \
    --request "$method" \
    --output "$output" \
    --write-out '%{http_code}' \
    "$@" \
    "$url"
}

status_code=$(request GET "$release_by_tag_url" "$response_body")
if [[ "$status_code" == 404 ]]; then
  create_body="$work_dir/create-release.json"
  jq -n \
    --arg tag_name "$release_tag" \
    --arg target_commitish "$FLEET_COMMIT_SHA" \
    --arg name "Fleet artifacts ${FLEET_COMMIT_SHA:0:12}" \
    --arg body "Immutable fleet artifacts built from ${FLEET_REPOSITORY}@${FLEET_COMMIT_SHA} by Forgejo Actions." \
    '{
      tag_name: $tag_name,
      target_commitish: $target_commitish,
      name: $name,
      body: $body,
      draft: true,
      prerelease: false
    }' >"$create_body"
  status_code=$(request POST "$repo_api/releases" "$response_body" \
    --header 'Content-Type: application/json' \
    --data-binary "@$create_body")
  if [[ "$status_code" == 409 ]]; then
    status_code=$(request GET "$release_by_tag_url" "$response_body")
  fi
fi
if [[ "$status_code" != 200 && "$status_code" != 201 ]]; then
  echo "Forgejo release API returned HTTP $status_code" >&2
  cat "$response_body" >&2
  exit 1
fi

release_id=$(jq -er '.id | select(type == "number")' "$response_body")
actual_tag=$(jq -er '.tag_name | select(type == "string")' "$response_body")
target_commitish=$(jq -er '.target_commitish | select(type == "string")' "$response_body")
if ! jq -e '.draft | type == "boolean"' "$response_body" >/dev/null; then
  echo "release API returned a non-boolean draft field" >&2
  exit 1
fi
is_draft=$(jq -r '.draft' "$response_body")
if [[ "$actual_tag" != "$release_tag" ]]; then
  echo "release API returned tag $actual_tag, not $release_tag" >&2
  exit 1
fi
if [[ "$target_commitish" != "$FLEET_COMMIT_SHA" ]]; then
  echo "release $release_tag targets $target_commitish, not $FLEET_COMMIT_SHA" >&2
  exit 1
fi
assets_url="$repo_api/releases/$release_id/assets"
assets_body="$work_dir/assets.json"
download_dir="$work_dir/downloads"
mkdir "$download_dir"

refresh_assets() {
  local assets_status
  assets_status=$(request GET "$assets_url" "$assets_body")
  if [[ "$assets_status" != 200 ]]; then
    echo "Forgejo release assets API returned HTTP $assets_status" >&2
    cat "$assets_body" >&2
    exit 1
  fi
}

refresh_assets
for asset in "${publish_assets[@]}"; do
  asset_name=$(basename "$asset")
  existing_url=$(jq -r --arg name "$asset_name" \
    '.[] | select(.name == $name and .type == "attachment") | .browser_download_url' "$assets_body")
  if [[ -z "$existing_url" ]]; then
    if [[ "$is_draft" != true ]]; then
      echo "published release is missing required asset: $asset_name" >&2
      exit 1
    fi
    upload_body="$work_dir/upload.json"
    upload_status=$(request POST "$assets_url?name=$asset_name" "$upload_body" \
      --form "attachment=@$asset;filename=$asset_name")
    if [[ "$upload_status" != 201 ]]; then
      refresh_assets
      existing_url=$(jq -r --arg name "$asset_name" \
        '.[] | select(.name == $name and .type == "attachment") | .browser_download_url' "$assets_body")
      if [[ -z "$existing_url" ]]; then
        echo "Forgejo release asset upload returned HTTP $upload_status" >&2
        cat "$upload_body" >&2
        exit 1
      fi
    fi
    refresh_assets
    continue
  fi

  downloaded_asset="$download_dir/$asset_name"
  if [[ "$existing_url" != "$forge_origin/"* ]]; then
    echo "refusing to send Forgejo credentials to another origin: $existing_url" >&2
    exit 1
  fi
  download_status=$(request GET "$existing_url" "$downloaded_asset")
  if [[ "$download_status" != 200 ]]; then
    echo "Forgejo release asset download returned HTTP $download_status: $asset_name" >&2
    exit 1
  fi
  if ! cmp -s "$asset" "$downloaded_asset"; then
    echo "refusing to replace immutable release asset: $asset_name" >&2
    exit 1
  fi
  echo "release asset already exists with identical bytes: $asset_name"
done

missing_assets=()
for ((attempt = 1; attempt <= wait_attempts; attempt++)); do
  refresh_assets
  missing_assets=()
  for expected_asset in "${expected_assets[@]}"; do
    if ! jq -e --arg name "$expected_asset" \
      '.[] | select(.name == $name)' "$assets_body" >/dev/null; then
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
  publish_body="$work_dir/publish-release.json"
  printf '{"draft":false}\n' >"$publish_body"
  publish_status=$(request PATCH "$repo_api/releases/$release_id" "$response_body" \
    --header 'Content-Type: application/json' \
    --data-binary "@$publish_body")
  if [[ "$publish_status" != 200 ]]; then
    current_status=$(request GET "$release_by_tag_url" "$response_body")
    if [[ "$current_status" != 200 || "$(jq -r '.draft' "$response_body")" != false ]]; then
      echo "Forgejo release publication returned HTTP $publish_status" >&2
      exit 1
    fi
  fi
fi
