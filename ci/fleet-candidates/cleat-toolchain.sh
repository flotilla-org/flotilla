#!/usr/bin/env bash

require_sha() {
  local name="$1"
  local value="$2"
  if [[ ${#value} -ne 40 || "$value" == *[!0-9a-fA-F]* ]]; then
    printf '%s must be an exact 40-character hexadecimal commit: %s\n' "$name" "$value" >&2
    return 1
  fi
  printf '%s' "$value" | tr 'A-F' 'a-f'
}

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
