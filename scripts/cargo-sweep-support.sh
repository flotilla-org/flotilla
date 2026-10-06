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

# Checkout provisioning reserves convoy-* ancestors for convoy-owned roots.
# Treat that lifecycle-maintained directory name as an ownership marker. Keep
# terminal and orphaned convoy caches too: teardown/GC owns their removal, so
# daemon availability and a crew starting during this run cannot weaken safety.
cargo_cleanup_skip_reason() {
  local root
  if ! root=$(cd -- "$1" 2>/dev/null && pwd -P); then
    echo "could not resolve checkout root; refusing cleanup"
    return 0
  fi
  while [[ $root != / ]]; do
    if [[ ${root##*/} == convoy-* ]]; then
      echo "convoy checkout (teardown/GC owns cleanup)"
      return 0
    fi
    root=${root%/*}
    [[ -n $root ]] || root=/
  done
  return 1
}

# Validate before any incremental removal or cargo-sweep compatibility writes.
# Preserve Cargo's diagnostic so the scheduled job can explain a skipped root.
check_cargo_metadata() {
  local root=$1
  local output
  if ! output=$(cargo metadata --format-version 1 --no-deps --locked --manifest-path "$root/Cargo.toml" 2>&1); then
    output=${output//$'\n'/ }
    printf 'cargo metadata failed: %s\n' "$output" >&2
    return 1
  fi
}
