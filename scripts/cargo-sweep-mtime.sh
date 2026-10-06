#!/usr/bin/env bash

# Daily per-host Cargo maintenance: sweep artifact families older than the
# three-day threshold, then apply the checkout size caps to the same roots.

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/cargo-sweep-support.sh"
readonly retention_days=3
state_dir=${XDG_STATE_HOME:-"$HOME/.local/state"}/flotilla
log_file=${FLOTILLA_SWEEP_LOG:-"$state_dir/cargo-sweep-mtime.log"}
lock_dir=$state_dir/cargo-sweep-mtime.lock

mkdir -p "$state_dir"
if ! mkdir "$lock_dir" 2>/dev/null; then
  printf '%s mtime-based cargo sweep skipped: another run holds %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$lock_dir" >> "$log_file"
  exit 0
fi
trap 'rmdir "$lock_dir"' EXIT

if [[ -x $HOME/.cargo/bin/cargo-sweep ]]; then
  cargo_sweep=$HOME/.cargo/bin/cargo-sweep
elif command -v cargo-sweep >/dev/null 2>&1; then
  cargo_sweep=$(command -v cargo-sweep)
else
  printf '%s mtime-based cargo sweep failed: cargo-sweep is not installed\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" >> "$log_file"
  exit 1
fi

# prune-target uses the same selected executable, including ~/.cargo/bin fallback.
export PATH="$(dirname -- "$cargo_sweep"):$PATH"

size_kib() {
  if [[ ! -d $1 ]]; then
    echo 0
    return
  fi
  du -sk "$1" 2>/dev/null | awk '{ print $1 }'
}

contains_root() {
  local candidate=$1
  local root

  if (( ${#sweep_roots[@]} == 0 )); then
    return 1
  fi
  for root in "${sweep_roots[@]}"; do
    [[ $root == "$candidate" ]] && return 0
  done
  return 1
}

sweep_roots=()
if [[ -d $HOME/dev ]]; then
  for checkout in "$HOME"/dev/*; do
    [[ -d $checkout/target ]] || continue
    sweep_roots+=("$checkout")
  done
fi

convoy_root=$HOME/dev/flotilla-repos
if [[ -d $convoy_root ]]; then
  while IFS= read -r -d '' target_dir; do
    checkout=${target_dir%/target}
    if [[ -f $checkout/Cargo.toml ]] && ! contains_root "$checkout"; then
      sweep_roots+=("$checkout")
    fi
  done < <(find "$convoy_root" -type d -name target -prune -print0)
fi

{
  started_at=$(date '+%Y-%m-%dT%H:%M:%S%z')
  total_reclaimed_bytes=0
  failed_roots=0
  skipped_roots=0
  echo "$started_at mtime-based cargo sweep started: retention_days=$retention_days roots=${#sweep_roots[@]}"

  if (( ${#sweep_roots[@]} > 0 )); then
    for root in "${sweep_roots[@]}"; do
      if skip_reason=$(cargo_cleanup_skip_reason "$root"); then
        skipped_roots=$((skipped_roots + 1))
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root skipped: $skip_reason"
        continue
      fi
      if ! metadata_error=$(CARGO_TARGET_DIR="$root/target" check_cargo_metadata "$root" 2>&1); then
        skipped_roots=$((skipped_roots + 1))
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root skipped: $metadata_error"
        continue
      fi
      if ! before_kib=$(size_kib "$root/target"); then
        failed_roots=$((failed_roots + 1))
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root failed: could not measure before sweep"
        continue
      fi
      root_failed=0
      if ! CARGO_TARGET_DIR="$root/target" checked_cargo_sweep "$root/target" "$cargo_sweep" sweep --time "$retention_days" "$root"; then
        root_failed=1
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root failed"
      fi
      if ! after_kib=$(size_kib "$root/target"); then
        root_failed=1
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root failed: could not measure after sweep"
      else
        reclaimed_kib=$((before_kib > after_kib ? before_kib - after_kib : 0))
        reclaimed_bytes=$((reclaimed_kib * 1024))
        total_reclaimed_bytes=$((total_reclaimed_bytes + reclaimed_bytes))
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep root=$root reclaimed_bytes=$reclaimed_bytes"
      fi
      if ! before_kib=$(size_kib "$root/target"); then
        root_failed=1
        echo "$(date '+%Y-%m-%dT%H:%M:%S%z') size-cap cargo prune root=$root failed: could not measure before pruning"
      else
        if ! CARGO_TARGET_DIR="$root/target" "$script_dir/prune-target.sh" --root "$root"; then
          root_failed=1
          echo "$(date '+%Y-%m-%dT%H:%M:%S%z') size-cap cargo prune root=$root failed"
        fi
        if ! after_kib=$(size_kib "$root/target"); then
          root_failed=1
          echo "$(date '+%Y-%m-%dT%H:%M:%S%z') size-cap cargo prune root=$root failed: could not measure after pruning"
        else
          reclaimed_bytes=$(((before_kib > after_kib ? before_kib - after_kib : 0) * 1024))
          total_reclaimed_bytes=$((total_reclaimed_bytes + reclaimed_bytes))
          echo "$(date '+%Y-%m-%dT%H:%M:%S%z') size-cap cargo prune root=$root reclaimed_bytes=$reclaimed_bytes"
        fi
      fi
      failed_roots=$((failed_roots + root_failed))
    done
  fi

  echo "$(date '+%Y-%m-%dT%H:%M:%S%z') mtime-based cargo sweep completed: reclaimed_bytes=$total_reclaimed_bytes failed_roots=$failed_roots skipped_roots=$skipped_roots"
  (( failed_roots == 0 ))
} >> "$log_file" 2>&1
