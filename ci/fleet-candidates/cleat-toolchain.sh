#!/usr/bin/env bash

read_cleat_toolchain_value() {
  local toolchain_file="$1"
  local section="$2"
  local key="$3"

  awk -v section="$section" -v key="$key" '
    $0 == "[" section "]" { in_section = 1; next }
    in_section && /^\[/ { exit }
    in_section && $1 == key && $2 == "=" {
      value = $3
      sub(/^"/, "", value)
      sub(/"$/, "", value)
      print value
      found = 1
      exit
    }
    END { if (!found) exit 1 }
  ' "$toolchain_file"
}
