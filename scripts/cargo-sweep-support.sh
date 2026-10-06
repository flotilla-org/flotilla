#!/usr/bin/env bash

# Shared compatibility boundary for cargo-sweep 0.8.0. Source from policy scripts.
prepare_cargo_sweep_target() {
  local target_dir=$1
  local fingerprint_dir

  [[ -d $target_dir ]] || return 0
  # cargo-sweep unconditionally reads build/ and deps/ for profiles with
  # fingerprints, even when Cargo never needed a build script there.
  while IFS= read -r -d '' fingerprint_dir; do
    mkdir -p "${fingerprint_dir%/.fingerprint}/build" "${fingerprint_dir%/.fingerprint}/deps" || return
  done < <(find "$target_dir" -type d -name .fingerprint -prune -print0)
}

checked_cargo_sweep() {
  local target_dir=$1
  local output
  local status=0
  shift

  prepare_cargo_sweep_target "$target_dir" || return
  if output=$("$@" 2>&1); then
    status=0
  else
    status=$?
  fi
  printf '%s\n' "$output"
  # This version logs cleanup errors but exits zero. Match the log level,
  # rather than a path containing the same text in an informational message.
  if grep -Eq '^\[ERROR\]' <<< "$output"; then
    return 1
  fi
  return "$status"
}
