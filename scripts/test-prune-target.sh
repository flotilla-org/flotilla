#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/flotilla-target-cap-test.XXXXXX")
target_dir=$test_root/target
stub_bin=$test_root/bin
stub_log=$test_root/cargo-sweep.log
relative_target_dir=$test_root/relative-target
relative_target_link_name=.flotilla-target-cap-relative-test.$$
relative_target_link=$repo_root/$relative_target_link_name
final_cap_dependency=$target_dir/debug/deps/final-old.rlib

cleanup() {
  rm -f -- "$relative_target_link"
  rm -rf -- "$test_root"
}

fail() {
  echo "target size-cap test failed: $*" >&2
  exit 1
}

trap cleanup EXIT

mkdir -p "$stub_bin" "$target_dir/debug/deps"
for generation in old middle new; do
  generation_dir=$target_dir/debug/incremental/probe/s-$generation
  mkdir -p "$generation_dir"
  dd if=/dev/zero of="$generation_dir/artifact.o" bs=1048576 count=2 >/dev/null 2>&1
done
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

rm -f -- "$CARGO_TARGET_DIR/debug/deps/final-old.rlib"
echo "[INFO] Cleaned 2.00 MiB from \"$CARGO_TARGET_DIR\""
STUB
chmod +x "$stub_bin/cargo-sweep"

run_cap() {
  PATH="$stub_bin:$PATH" \
    CARGO_TARGET_DIR="$target_dir" \
    FLOTILLA_SWEEP_TEST_FAIL="${FLOTILLA_SWEEP_TEST_FAIL:-0}" \
    FLOTILLA_SWEEP_TEST_LOG="$stub_log" \
    FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE=3MiB \
    FLOTILLA_TARGET_MAX_SIZE=100MiB \
    "$repo_root/scripts/prune-target.sh" "$@"
}

assert_invalid_config() {
  local output
  local variable=$1
  local value=$2

  if output=$(env \
    PATH="$stub_bin:$PATH" \
    CARGO_TARGET_DIR="$target_dir" \
    "$variable=$value" \
    "$repo_root/scripts/prune-target.sh" --dry-run 2>&1); then
    fail "$variable accepted invalid value '$value'"
  fi
  grep -Fq "$variable" <<< "$output" || fail "$variable error did not identify the invalid setting"
}

mkdir -p "$relative_target_dir/debug/deps"
canonical_relative_target_dir=$(cd -- "$relative_target_dir" && pwd -P)
ln -s "$relative_target_dir" "$relative_target_link"
(
  cd "$test_root"
  PATH="$stub_bin:$PATH" \
    CARGO_TARGET_DIR="$relative_target_link_name" \
    FLOTILLA_SWEEP_TEST_LOG="$stub_log" \
    FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE=100MiB \
    FLOTILLA_TARGET_MAX_SIZE=100MiB \
    "$repo_root/scripts/prune-target.sh" >/dev/null
)
grep -Fq "target=$canonical_relative_target_dir" "$stub_log" || fail "cargo-sweep did not receive the canonical relative target"

if PATH="$stub_bin:$PATH" CARGO_TARGET_DIR=// "$repo_root/scripts/prune-target.sh" --dry-run >/dev/null 2>&1; then
  fail "root alias was accepted as the target directory"
fi

assert_invalid_config FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE 3GB
assert_invalid_config FLOTILLA_TARGET_MAX_SIZE 20GB

if FLOTILLA_SWEEP_TEST_FAIL=1 run_cap --dry-run >/dev/null 2>&1; then
  fail "preview ignored a cargo-sweep failure"
fi
[[ -z $(find "$test_root" -maxdepth 1 -type d -name '.target.flotilla-target-cap-preview.*' -print -quit) ]] || fail "failed preview clone was not cleaned up"

preview_output=$(run_cap --dry-run)

[[ -d $target_dir/debug/incremental/probe/s-old ]] || fail "preview removed an incremental generation"
[[ -d $target_dir/debug/incremental/probe/s-middle ]] || fail "preview removed an incremental generation"
[[ -f $final_cap_dependency ]] || fail "preview removed the final-cap dependency"
grep -Fq "Would remove 2 oldest incremental generations" <<< "$preview_output" || fail "preview missed the incremental size cap"
grep -Fq "Would remove Cargo artifact families to reach 100MiB (2.0 MiB)" <<< "$preview_output" || fail "preview missed the target size cap"
! grep -Fq -- "--time" "$stub_log" || fail "size-cap backstop invoked age-based cargo-sweep"
[[ -z $(find "$test_root" -maxdepth 1 -type d -name '.target.flotilla-target-cap-preview.*' -print -quit) ]] || fail "preview clone was not cleaned up"

run_cap >/dev/null

[[ ! -e $target_dir/debug/incremental/probe/s-old ]] || fail "apply retained the oldest excess generation"
[[ ! -e $target_dir/debug/incremental/probe/s-middle ]] || fail "apply retained the next excess generation"
[[ -d $target_dir/debug/incremental/probe/s-new ]] || fail "apply removed the newest generation"
[[ ! -e $final_cap_dependency ]] || fail "apply retained the final-cap dependency"
! grep -Fq -- "--time" "$stub_log" || fail "size-cap backstop invoked age-based cargo-sweep"

echo "target size-cap behavior tests passed"
