#!/usr/bin/env bash
# Populate the lab mirror of the Zig package archives a Cleat revision builds
# with, so fleet candidate builds need no third-party host for them.
#
#   ci/fleet-candidates/mirror-zig-packages.sh <cleat-sha> <token-file>
#
# Run once by an operator for each new Cleat or Ghostty pin. The token file
# holds a lab Forgejo token with write:package on the `lab` organisation.
# Cleat's prepare script runs with an empty Zig package cache and the mirror
# enabled, so every package is fetched (from the mirror where it already is,
# upstream otherwise) and re-hashed by `zig fetch` against the hash the
# build.zig.zon files declare. Only archives that passed that check are
# uploaded; ones the mirror already holds are left as they are.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/cleat-toolchain.sh"
source "$(dirname "${BASH_SOURCE[0]}")/zig-package-mirror.sh"

[[ $# -eq 2 ]] || { echo "usage: $0 <cleat-sha> <token-file>" >&2; exit 2; }
cleat_sha="$(require_sha cleat-sha "$1")"
token_file="$2"
[[ -f "$token_file" && -r "$token_file" ]] || { echo "token file is not readable: $token_file" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
header="$work/authorization.header"
write_zig_package_mirror_header "$(<"$token_file")" "$header"

cleat_root="$work/cleat"
git init -q "$cleat_root"
git -C "$cleat_root" remote add origin https://github.com/flotilla-org/cleat.git
git -C "$cleat_root" fetch -q --depth=1 origin "$cleat_sha"
git -C "$cleat_root" checkout -q --detach FETCH_HEAD
grep -q 'ZIG_PACKAGE_ARCHIVE_DIR' "$cleat_root/tools/prepare-ghostty-vt.sh" \
  || { echo "cleat $cleat_sha predates the Zig package mirror (no ZIG_PACKAGE_ARCHIVE_DIR)" >&2; exit 1; }

archives="$work/archives"
mkdir -p "$archives" "$work/zig-cache"
ZIG_GLOBAL_CACHE_DIR="$work/zig-cache" \
  ZIG_PACKAGE_MIRROR="$FLEET_ZIG_PACKAGE_MIRROR_URL" \
  ZIG_PACKAGE_MIRROR_AUTH_HEADER_FILE="$header" \
  ZIG_PACKAGE_ARCHIVE_DIR="$archives" \
  "$cleat_root/tools/prepare-ghostty-vt.sh" --fetch-only

uploaded=0 present=0
for archive in "$archives"/*/*; do
  [[ -f "$archive" ]] || continue
  hash="$(basename "$(dirname "$archive")")"
  url="${FLEET_ZIG_PACKAGE_MIRROR_URL%/}/$hash/$(basename "$archive")"
  status="$(curl --silent --show-error --retry 3 --header "@$header" --upload-file "$archive" \
    --output "$work/response" --write-out '%{http_code}' "$url")"
  case "$status" in
    201) echo "uploaded $hash" >&2; uploaded=$((uploaded + 1)) ;;
    409) present=$((present + 1)) ;;
    *) printf 'upload of %s failed (HTTP %s): %s\n' "$hash" "$status" "$(cat "$work/response")" >&2; exit 1 ;;
  esac
done
printf 'Zig package mirror for cleat %s: %d uploaded, %d already present\n' "$cleat_sha" "$uploaded" "$present" >&2
