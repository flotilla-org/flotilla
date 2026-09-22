#!/usr/bin/env bash
set -euo pipefail

zig_version=$(awk '
  /^\[zig\]$/ { in_zig = 1; next }
  in_zig && /^\[/ { exit }
  in_zig && $1 == "version" && $2 == "=" {
    gsub(/^"|"$/, "", $3)
    print $3
    found = 1
    exit
  }
  END { if (!found) exit 1 }
' tools/ghostty-toolchain.toml)
test "$(zig version)" = "$zig_version"
