#!/usr/bin/env bash
# The lab mirror of Zig package archives (Forgejo generic package
# `lab/zig-packages`, one version per declared package hash). Cleat's
# prepare-ghostty-vt.sh reads it through ZIG_PACKAGE_MIRROR; see the README.

FLEET_ZIG_PACKAGE_MIRROR_URL='https://forgejo.lab.flotilla.work/api/packages/lab/generic/zig-packages'

# Write a curl header file carrying a Forgejo token, readable only by us.
write_zig_package_mirror_header() {
  local token="$1" destination="$2"
  [[ -n "$token" && ! "$token" =~ [[:space:]] ]] || { echo 'Zig package mirror token is empty or malformed' >&2; return 1; }
  (umask 077 && printf 'Authorization: token %s\n' "$token" >"$destination")
}
