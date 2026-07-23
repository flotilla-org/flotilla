#!/usr/bin/env bash

set -euo pipefail

readonly default_age_days=30
readonly default_incremental_max_size=10GiB
readonly default_max_size=20GiB

age_days=${FLOTILLA_TARGET_GC_DAYS:-$default_age_days}
incremental_max_size=${FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE:-$default_incremental_max_size}
max_size=${FLOTILLA_TARGET_GC_MAX_SIZE:-$default_max_size}
mode=apply
preview_target_dir=
candidate_file=

usage() {
  echo "Usage: scripts/prune-target.sh [--dry-run]"
  echo
  echo "Prune this checkout's Cargo target by age, then cap its size."
  echo "Environment overrides:"
  echo "  FLOTILLA_TARGET_GC_DAYS                  retention in days (default: $default_age_days)"
  echo "  FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE  incremental ceiling (default: $default_incremental_max_size)"
  echo "  FLOTILLA_TARGET_GC_MAX_SIZE              target ceiling (default: $default_max_size)"
}

case ${1:-} in
  "")
    ;;
  --dry-run)
    mode=preview
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

if (( $# != 0 )); then
  usage >&2
  exit 2
fi

if [[ ! $age_days =~ ^[0-9]+$ ]]; then
  echo "FLOTILLA_TARGET_GC_DAYS must be a non-negative integer; got '$age_days'." >&2
  exit 2
fi

if ! command -v cargo-sweep >/dev/null 2>&1; then
  echo "cargo-sweep is required. Install the tested version with:" >&2
  echo "  cargo install cargo-sweep --version 0.8.0 --locked" >&2
  exit 1
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$repo_root/target"}

if [[ $target_dir != /* ]]; then
  target_dir=$repo_root/$target_dir
fi

if [[ -z $target_dir ]]; then
  echo "Refusing an empty target directory." >&2
  exit 1
fi

if [[ ! -d $target_dir ]]; then
  echo "Cargo target directory does not exist: $target_dir"
  exit 0
fi

target_dir=$(cd -- "$target_dir" && pwd -P)
if [[ $target_dir == / ]]; then
  echo "Refusing unsafe target directory '$target_dir'." >&2
  exit 1
fi
display_target_dir=$target_dir
export CARGO_TARGET_DIR=$target_dir

size_to_kib() {
  local value=$1
  local number

  case $value in
    *GiB)
      number=${value%GiB}
      [[ $number =~ ^[0-9]+$ ]] || return 1
      echo $((number * 1024 * 1024))
      ;;
    *MiB)
      number=${value%MiB}
      [[ $number =~ ^[0-9]+$ ]] || return 1
      echo $((number * 1024))
      ;;
    *)
      return 1
      ;;
  esac
}

incremental_max_kib=$(size_to_kib "$incremental_max_size") || {
  echo "FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE must use whole MiB or GiB units; got '$incremental_max_size'." >&2
  exit 2
}
size_to_kib "$max_size" >/dev/null || {
  echo "FLOTILLA_TARGET_GC_MAX_SIZE must use whole MiB or GiB units; got '$max_size'." >&2
  exit 2
}

format_kib() {
  awk -v kib="$1" 'BEGIN {
    if (kib >= 1024 * 1024) {
      printf "%.2f GiB", kib / 1024 / 1024
    } else {
      printf "%.1f MiB", kib / 1024
    }
  }'
}

incremental_size_kib() {
  find "$target_dir" -type d -name incremental -prune -exec du -sk {} \; 2>/dev/null | awk '{ total += $1 } END { print total + 0 }'
}

size_kib() {
  du -sk "$1" | awk '{ print $1 }'
}

generation_mtime() {
  if [[ $(uname -s) == Darwin ]]; then
    stat -f %m "$1"
  else
    stat -c %Y "$1"
  fi
}

cleanup_temp_files() {
  if [[ -n $candidate_file ]]; then
    rm -f -- "$candidate_file"
  fi
  if [[ -n $preview_target_dir && -d $preview_target_dir ]]; then
    rm -rf -- "$preview_target_dir"
  fi
}

trap cleanup_temp_files EXIT

prepare_preview_target() {
  local target_name
  local target_parent

  if [[ $mode != preview ]]; then
    return
  fi

  target_parent=$(dirname -- "$target_dir")
  target_name=$(basename -- "$target_dir")
  preview_target_dir=$(mktemp -d "$target_parent/.${target_name}.flotilla-target-gc-preview.XXXXXX")
  cp -a -l "$target_dir/." "$preview_target_dir/"
  target_dir=$preview_target_dir
  export CARGO_TARGET_DIR=$target_dir
}

prune_aged_incrementals() {
  local count=0
  local reclaimed_kib=0
  local generation
  local generation_kib

  if [[ ! -d $target_dir ]]; then
    return 0
  fi

  while IFS= read -r generation; do
    generation_kib=$(size_kib "$generation")
    rm -rf -- "$generation"
    count=$((count + 1))
    reclaimed_kib=$((reclaimed_kib + generation_kib))
  done < <(find "$target_dir" -type d -path '*/incremental/*/s-*' -mtime +"$age_days" -prune -print)

  if (( count > 0 )); then
    if [[ $mode == preview ]]; then
      echo "Would remove $count incremental generations older than $age_days days ($(format_kib "$reclaimed_kib"))"
    else
      echo "Removed $count incremental generations older than $age_days days ($(format_kib "$reclaimed_kib"))"
    fi
  fi
}

cap_incrementals() {
  local current_kib
  local generation
  local generation_kib
  local mtime
  local removed_count=0
  local removed_kib=0

  if [[ ! -d $target_dir ]]; then
    return 0
  fi

  current_kib=$(incremental_size_kib)
  if (( current_kib <= incremental_max_kib )); then
    return
  fi

  candidate_file=$(mktemp "${TMPDIR:-/tmp}/flotilla-target-gc.XXXXXX")
  while IFS= read -r generation; do
    mtime=$(generation_mtime "$generation")
    generation_kib=$(size_kib "$generation")
    printf '%s\t%s\t%s\n' "$mtime" "$generation_kib" "$generation" >> "$candidate_file"
  done < <(find "$target_dir" -type d -path '*/incremental/*/s-*' -prune -print)

  while IFS=$'\t' read -r mtime generation_kib generation; do
    if (( current_kib <= incremental_max_kib )); then
      break
    fi
    rm -rf -- "$generation"
    current_kib=$((current_kib - generation_kib))
    removed_count=$((removed_count + 1))
    removed_kib=$((removed_kib + generation_kib))
  done < <(sort -n "$candidate_file")
  rm -f -- "$candidate_file"
  candidate_file=

  if [[ $mode == preview ]]; then
    echo "Would remove $removed_count oldest incremental generations ($(format_kib "$removed_kib")) to reach $incremental_max_size"
  else
    echo "Removed $removed_count oldest incremental generations ($(format_kib "$removed_kib")) to reach $incremental_max_size"
  fi
}

run_sweep() {
  local description=$1
  local after_kib
  local before_kib
  local sweep_output

  shift
  if [[ $mode == apply ]]; then
    cargo sweep "$@"
    return
  fi

  before_kib=$(size_kib "$target_dir")
  if ! sweep_output=$(cargo sweep "$@" 2>&1); then
    printf '%s\n' "$sweep_output" >&2
    return 1
  fi
  after_kib=$(size_kib "$target_dir")

  if (( before_kib > after_kib )); then
    echo "Would remove $description ($(format_kib "$((before_kib - after_kib))"))"
  else
    echo "Would remove no $description"
  fi
}

prune_aged_other_artifacts() {
  run_sweep "Cargo artifact families older than $age_days days" --time "$age_days" "$repo_root"
}

cap_target() {
  run_sweep "Cargo artifact families to reach $max_size" --maxsize "$max_size" "$repo_root"
}

prepare_preview_target

echo "Pruning incremental generations older than $age_days days in $display_target_dir"
prune_aged_incrementals

echo "Pruning other Cargo artifacts older than $age_days days in $repo_root"
prune_aged_other_artifacts

echo "Capping incremental generations at $incremental_max_size in $display_target_dir"
cap_incrementals

echo "Capping Cargo artifacts at $max_size in $repo_root"
cap_target
