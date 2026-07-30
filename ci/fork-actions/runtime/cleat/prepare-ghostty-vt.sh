#!/usr/bin/env bash
set -euo pipefail

toolchain_file=tools/ghostty-toolchain.toml
if ! grep -Fq -- '-Demit-xcframework=false' "$toolchain_file"; then
  patched_file=$(mktemp)
  trap 'rm -f "$patched_file"' EXIT
  awk '
    /^build_step = / {
      sub("-Demit-lib-vt=true ", "-Demit-lib-vt=true -Demit-xcframework=false ")
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
fi

grep -Fq -- '-Demit-xcframework=false' "$toolchain_file"
./tools/prepare-ghostty-vt.sh
test -f .tools/ghostty-install/lib/libghostty-vt.a
find .tools/ghostty-install/lib -maxdepth 1 \
  \( -name 'libghostty-vt.so' -o -name 'libghostty-vt.dylib' \) \
  -delete
