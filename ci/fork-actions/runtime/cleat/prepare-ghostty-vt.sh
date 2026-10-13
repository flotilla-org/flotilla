#!/usr/bin/env bash
set -euo pipefail

toolchain_file=tools/ghostty-toolchain.toml

# Add a Ghostty build flag to the pinned build step unless it is already there.
ensure_build_flag() {
  local flag=$1
  if grep -Fq -- "$flag" "$toolchain_file"; then
    return
  fi
  local patched_file
  patched_file=$(mktemp)
  trap 'rm -f "$patched_file"' EXIT
  awk -v flag="$flag" '
    /^build_step = / {
      sub("-Demit-lib-vt=true ", "-Demit-lib-vt=true " flag " ")
      patched = 1
    }
    { print }
    END {
      if (!patched) {
        exit 1
      }
    }
  ' "$toolchain_file" >"$patched_file"
  mv "$patched_file" "$toolchain_file"
  trap - EXIT
  grep -Fq -- "$flag" "$toolchain_file"
}

ensure_build_flag -Demit-xcframework=false
# Ghostty's Linux build otherwise targets the build machine's CPU, so a cached
# or published binary can die with SIGILL on an older runner or host.
ensure_build_flag -Dcpu=baseline

./tools/prepare-ghostty-vt.sh
test -f .tools/ghostty-install/lib/libghostty-vt.a
find .tools/ghostty-install/lib -maxdepth 1 \
  \( -name 'libghostty-vt.so' -o -name 'libghostty-vt.dylib' \) \
  -delete
