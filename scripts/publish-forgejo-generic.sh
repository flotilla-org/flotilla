#!/usr/bin/env bash
set -euo pipefail

: "${FORGEJO_SERVER_URL:?FORGEJO_SERVER_URL is required}"
: "${FORGEJO_PACKAGE_OWNER:?FORGEJO_PACKAGE_OWNER is required}"
: "${FORGEJO_PACKAGE_NAME:?FORGEJO_PACKAGE_NAME is required}"
: "${FORGEJO_PACKAGE_VERSION:?FORGEJO_PACKAGE_VERSION is required}"
: "${FORGEJO_PACKAGE_TOKEN:?FORGEJO_PACKAGE_TOKEN is required}"

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <artifact>..." >&2
  exit 2
fi

for value in "$FORGEJO_PACKAGE_OWNER" "$FORGEJO_PACKAGE_NAME" "$FORGEJO_PACKAGE_VERSION"; do
  if [[ ! "$value" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "invalid Forgejo package path component: $value" >&2
    exit 1
  fi
done

server_url=${FORGEJO_SERVER_URL%/}
server_authority=${server_url#*://}
server_authority=${server_authority%%/*}
server_host=${server_authority%%:*}

umask 077
auth_file=$(mktemp)
remote_file=$(mktemp)
trap 'rm -f "$auth_file" "$remote_file"' EXIT
printf 'machine %s login %s password %s\n' "$server_host" "$FORGEJO_PACKAGE_OWNER" "$FORGEJO_PACKAGE_TOKEN" >"$auth_file"

for artifact in "$@"; do
  if [[ ! -f "$artifact" ]]; then
    echo "artifact does not exist: $artifact" >&2
    exit 1
  fi

  file_name=$(basename "$artifact")
  if [[ ! "$file_name" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "invalid Forgejo package file name: $file_name" >&2
    exit 1
  fi

  artifact_url="${server_url}/api/packages/${FORGEJO_PACKAGE_OWNER}/generic/${FORGEJO_PACKAGE_NAME}/${FORGEJO_PACKAGE_VERSION}/${file_name}"
  status=$(
    curl --silent --show-error --location \
      --netrc-file "$auth_file" \
      --output "$remote_file" \
      --write-out '%{http_code}' \
      "$artifact_url"
  )

  case "$status" in
    200)
      if ! cmp -s "$artifact" "$remote_file"; then
        echo "refusing to replace immutable package file: $artifact_url" >&2
        exit 1
      fi
      echo "already published: $file_name"
      ;;
    404)
      curl --silent --show-error --fail-with-body \
        --netrc-file "$auth_file" \
        --upload-file "$artifact" \
        "$artifact_url"
      echo "published: $file_name"
      ;;
    *)
      echo "unexpected Forgejo response $status while checking $artifact_url" >&2
      if [[ -s "$remote_file" ]]; then
        sed -n '1,20p' "$remote_file" >&2
      fi
      exit 1
      ;;
  esac
done
