#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/flotilla-target-gc-test.XXXXXX")
target_dir=$test_root/target
stub_bin=$test_root/bin
stub_log=$test_root/cargo-sweep.log
relative_log=$test_root/cargo-sweep-relative.log
relative_target_dir=$test_root/relative-target
relative_target_link_name=.flotilla-target-gc-relative-test.$$
relative_target_link=$repo_root/$relative_target_link_name
stale_dependency=$target_dir/debug/deps/stale.rlib
final_cap_dependency=$target_dir/debug/deps/final-old.rlib

cleanup() {
  rm -f -- "$relative_target_link"
  rm -rf -- "$test_root"
}

fail() {
  echo "target GC test failed: $*" >&2
  exit 1
}

trap cleanup EXIT

mkdir -p "$stub_bin" "$target_dir/debug/deps"
for generation in old middle new; do
  generation_dir=$target_dir/debug/incremental/probe/s-$generation
  mkdir -p "$generation_dir"
  dd if=/dev/zero of="$generation_dir/artifact.o" bs=1048576 count=2 >/dev/null 2>&1
done
dd if=/dev/zero of="$stale_dependency" bs=1048576 count=2 >/dev/null 2>&1
dd if=/dev/zero of="$final_cap_dependency" bs=1048576 count=2 >/dev/null 2>&1

touch -t 202001010000 "$target_dir/debug/incremental/probe/s-old"
touch "$target_dir/debug/incremental/probe/s-middle"
touch "$target_dir/debug/incremental/probe/s-new"

cat > "$stub_bin/cargo-sweep" <<'STUB'
#!/usr/bin/env bash

set -euo pipefail

printf '%s\n' "$*" >> "$FLOTILLA_SWEEP_TEST_LOG"
printf 'target=%s\n' "$CARGO_TARGET_DIR" >> "$FLOTILLA_SWEEP_TEST_LOG"

if [[ ${FLOTILLA_SWEEP_TEST_FAIL:-0} == 1 ]]; then
  exit 42
fi

if [[ " $* " == *" --time "* ]]; then
  rm -f -- "$CARGO_TARGET_DIR/debug/deps/stale.rlib"
  echo "selected stale.rlib" >> "$FLOTILLA_SWEEP_TEST_LOG"
  echo "[INFO] Cleaned 2.00 MiB from \"$CARGO_TARGET_DIR\""
else
  rm -f -- "$CARGO_TARGET_DIR/debug/deps/final-old.rlib"
  echo "selected final-old.rlib" >> "$FLOTILLA_SWEEP_TEST_LOG"
  echo "[INFO] Cleaned 2.00 MiB from \"$CARGO_TARGET_DIR\""
fi
STUB
chmod +x "$stub_bin/cargo-sweep"

run_gc() {
  PATH="$stub_bin:$PATH" \
    CARGO_TARGET_DIR="$target_dir" \
    FLOTILLA_SWEEP_TEST_FAIL="${FLOTILLA_SWEEP_TEST_FAIL:-0}" \
    FLOTILLA_SWEEP_TEST_LOG="$stub_log" \
    FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE=3MiB \
    FLOTILLA_TARGET_GC_MAX_SIZE=100MiB \
    "$repo_root/scripts/prune-target.sh" "$@"
}

mkdir -p "$relative_target_dir/debug/deps"
canonical_relative_target_dir=$(cd -- "$relative_target_dir" && pwd -P)
ln -s "$relative_target_dir" "$relative_target_link"
(
  cd "$test_root"
  PATH="$stub_bin:$PATH" \
    CARGO_TARGET_DIR="$relative_target_link_name" \
    FLOTILLA_SWEEP_TEST_LOG="$relative_log" \
    FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE=100MiB \
    FLOTILLA_TARGET_GC_MAX_SIZE=100MiB \
    "$repo_root/scripts/prune-target.sh" >/dev/null
)
grep -Fq "target=$canonical_relative_target_dir" "$relative_log" || fail "cargo-sweep did not receive the canonical relative target"

if PATH="$stub_bin:$PATH" CARGO_TARGET_DIR=// "$repo_root/scripts/prune-target.sh" --dry-run >/dev/null 2>&1; then
  fail "root alias was accepted as the target directory"
fi

if FLOTILLA_SWEEP_TEST_FAIL=1 run_gc --dry-run >/dev/null 2>&1; then
  fail "preview ignored a cargo-sweep failure"
fi
[[ -z $(find "$test_root" -maxdepth 1 -type d -name '.target.flotilla-target-gc-preview.*' -print -quit) ]] || fail "failed preview clone was not cleaned up"

preview_output=$(run_gc --dry-run)

[[ -d $target_dir/debug/incremental/probe/s-old ]] || fail "preview removed the aged generation"
[[ -d $target_dir/debug/incremental/probe/s-middle ]] || fail "preview removed the size-cap generation"
[[ -f $stale_dependency ]] || fail "preview removed the stale dependency"
[[ -f $final_cap_dependency ]] || fail "preview removed the final-cap dependency"
grep -Fq "Would remove 1 incremental generations older" <<< "$preview_output" || fail "preview missed the aged generation"
grep -Fq "Would remove 1 oldest incremental generations" <<< "$preview_output" || fail "preview double-counted the aged generation"
grep -Fq "Would remove Cargo artifact families older than 30 days (2.0 MiB)" <<< "$preview_output" || fail "preview missed the stale dependency"
grep -Fq "Would remove Cargo artifact families to reach 100MiB (2.0 MiB)" <<< "$preview_output" || fail "preview missed the final-cap dependency"
[[ $(grep -Fc "selected stale.rlib" "$stub_log") == 1 ]] || fail "preview did not select the aged Cargo family once"
[[ $(grep -Fc "selected final-old.rlib" "$stub_log") == 1 ]] || fail "preview did not select the final Cargo family once"
! grep -Fq -- "--dry-run" "$stub_log" || fail "preview did not simulate the preceding removals"
[[ -z $(find "$test_root" -maxdepth 1 -type d -name '.target.flotilla-target-gc-preview.*' -print -quit) ]] || fail "preview clone was not cleaned up"

run_gc >/dev/null

[[ ! -e $target_dir/debug/incremental/probe/s-old ]] || fail "apply retained the aged generation"
[[ ! -e $target_dir/debug/incremental/probe/s-middle ]] || fail "apply retained the oldest excess generation"
[[ -d $target_dir/debug/incremental/probe/s-new ]] || fail "apply removed the newest generation"
[[ ! -e $stale_dependency ]] || fail "apply retained the stale dependency"
[[ ! -e $final_cap_dependency ]] || fail "apply retained the final-cap dependency"
[[ $(grep -Fc "selected stale.rlib" "$stub_log") == 2 ]] || fail "preview and apply selected different aged Cargo families"
[[ $(grep -Fc "selected final-old.rlib" "$stub_log") == 2 ]] || fail "preview and apply selected different final Cargo families"

echo "target GC behavior tests passed"
