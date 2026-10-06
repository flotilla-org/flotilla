#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/flotilla-mtime-sweep-test.XXXXXX")
test_home=$test_root/home
stub_log=$test_root/invocations.log
sweep_log=$test_root/sweep.log
empty_home=$test_root/empty-home
empty_log=$test_root/empty-sweep.log

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

# Real regression coverage needs the pinned subprocess; provision it in the test
# directory on fresh CI hosts rather than requiring a scheduler or global install.
if ! command -v cargo-sweep >/dev/null 2>&1; then
  cargo install cargo-sweep --version 0.8.0 --locked --root "$test_root/tools"
  export PATH="$test_root/tools/bin:$PATH"
fi

mkdir -p "$test_home/.cargo/bin" "$test_home/dev/desk/target" "$test_home/dev/no-target" "$test_home/dev/flotilla-repos/convoy/target"
: > "$test_home/dev/flotilla-repos/convoy/Cargo.toml"
# Recently used incremental generations must be capped after the age sweep.
for generation in old middle new; do
  generation_dir=$test_home/dev/desk/target/debug/incremental/probe/s-$generation
  mkdir -p "$generation_dir"
  dd if=/dev/zero of="$generation_dir/artifact.o" bs=1048576 count=2 >/dev/null 2>&1
done
touch -t 202001010000 "$test_home/dev/desk/target/debug/incremental/probe/s-old"
dd if=/dev/zero of="$test_home/dev/desk/target/stale" bs=1048576 count=1 >/dev/null 2>&1
dd if=/dev/zero of="$test_home/dev/flotilla-repos/convoy/target/stale" bs=1048576 count=1 >/dev/null 2>&1

cat > "$test_home/.cargo/bin/cargo-sweep" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$FLOTILLA_SWEEP_TEST_INVOCATIONS"
# Process boundary: age sweeping removes stale files; size sweeping is a no-op.
[[ $CARGO_TARGET_DIR == "${@: -1}/target" ]]
if [[ ${FLOTILLA_SWEEP_TEST_LOG_ERROR:-0} == 1 ]]; then
  echo '[ERROR] Failed to clean target: simulated cleanup failure'
  exit 0
fi
if [[ ${FLOTILLA_SWEEP_TEST_FAIL:-0} == 1 && $2 == --maxsize ]]; then
  exit 42
fi
if [[ $2 == --time ]]; then
  find "${@: -1}" -type f -name stale -delete
fi
STUB
chmod +x "$test_home/.cargo/bin/cargo-sweep"

HOME="$test_home" XDG_STATE_HOME="$test_home/.local/state" \
  FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE=3MiB \
  FLOTILLA_TARGET_MAX_SIZE=5MiB \
  FLOTILLA_SWEEP_LOG="$sweep_log" \
  FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  "$repo_root/scripts/cargo-sweep-mtime.sh"

grep -Fxq "sweep --time 3 $test_home/dev/desk" "$stub_log"
grep -Fxq "sweep --time 3 $test_home/dev/flotilla-repos/convoy" "$stub_log"
[[ $(wc -l < "$stub_log") == 4 ]]
! grep -Fq "$test_home/dev/no-target" "$stub_log"
# Both stages report per-root reclaimed bytes; size caps apply to the same roots.
grep -Fxq "sweep --maxsize 5MiB $test_home/dev/desk" "$stub_log"
grep -Fxq "sweep --maxsize 5MiB $test_home/dev/flotilla-repos/convoy" "$stub_log"
[[ ! -e $test_home/dev/desk/target/debug/incremental/probe/s-old ]]
[[ $(du -sk "$test_home/dev/desk/target" | awk '{print $1}') -le 5120 ]]
grep -Fq "mtime-based cargo sweep root=$test_home/dev/desk reclaimed_bytes=1048576" "$sweep_log"
grep -Eq "size-cap cargo prune root=.*desk reclaimed_bytes=[1-9][0-9]*" "$sweep_log"

mkdir -p "$empty_home/.cargo/bin"
cp "$test_home/.cargo/bin/cargo-sweep" "$empty_home/.cargo/bin/"
HOME="$empty_home" XDG_STATE_HOME="$empty_home/.local/state" \
  FLOTILLA_SWEEP_LOG="$empty_log" \
  FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  "$repo_root/scripts/cargo-sweep-mtime.sh"

[[ $(wc -l < "$stub_log") == 4 ]]
grep -Fq "mtime-based cargo sweep started: retention_days=3 roots=0" "$empty_log"
grep -Fq "mtime-based cargo sweep completed: reclaimed_bytes=0 failed_roots=0" "$empty_log"

# Untracked files can leave targets above cap; the existing log must warn.
dd if=/dev/zero of="$test_home/dev/desk/target/untracked" bs=1048576 count=6 >/dev/null 2>&1
HOME="$test_home" XDG_STATE_HOME="$test_home/.local/state" \
  FLOTILLA_SWEEP_LOG="$sweep_log" FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  FLOTILLA_TARGET_MAX_SIZE=5MiB "$repo_root/scripts/cargo-sweep-mtime.sh"
grep -Fq "WARNING: target remains over cap: target=$test_home/dev/desk/target" "$sweep_log"

# One failed cap makes the job fail, but does not prevent capping later roots.
if HOME="$test_home" XDG_STATE_HOME="$test_home/.local/state" \
  FLOTILLA_SWEEP_LOG="$sweep_log" FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  FLOTILLA_SWEEP_TEST_FAIL=1 "$repo_root/scripts/cargo-sweep-mtime.sh"; then
  echo "scheduled pruning ignored a failed cap" >&2
  exit 1
fi
grep -Fq "size-cap cargo prune root=$test_home/dev/flotilla-repos/convoy failed" "$sweep_log"
grep -Eq 'completed: reclaimed_bytes=[0-9]+ failed_roots=2' "$sweep_log"

# cargo-sweep's zero-exit error diagnostics must fail both scheduled stages.
error_log=$test_root/error.log
if HOME="$test_home" XDG_STATE_HOME="$test_home/.local/state" \
  FLOTILLA_SWEEP_LOG="$error_log" FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  FLOTILLA_SWEEP_TEST_LOG_ERROR=1 "$repo_root/scripts/cargo-sweep-mtime.sh"; then
  echo "scheduled sweep accepted an error diagnostic with exit zero" >&2
  exit 1
fi
grep -Fq "mtime-based cargo sweep root=$test_home/dev/desk failed" "$error_log"
grep -Fq "size-cap cargo prune root=$test_home/dev/desk failed" "$error_log"

# Installer glue: both platform branches deploy the helper beside the runner.
# Stubs stand in for OS scheduler commands; execute the installed runner separately.
stub_bin=$test_root/scheduler-bin
mkdir -p "$stub_bin"
cat > "$stub_bin/uname" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$FLOTILLA_TEST_OS"
STUB
for scheduler in systemctl launchctl; do
  printf '#!/usr/bin/env bash\nexit 0\n' > "$stub_bin/$scheduler"
done
chmod +x "$stub_bin/"*
for platform in Linux Darwin; do
  HOME="$test_home" PATH="$stub_bin:$PATH" FLOTILLA_TEST_OS="$platform" \
    XDG_CONFIG_HOME="$test_home/.config" "$repo_root/scripts/install-cargo-sweep-schedule.sh" >/dev/null
  cmp "$repo_root/scripts/cargo-sweep-support.sh" "$test_home/.local/libexec/flotilla/cargo-sweep-support.sh"
  cmp "$repo_root/scripts/prune-target.sh" "$test_home/.local/libexec/flotilla/prune-target.sh"
  cmp "$repo_root/scripts/cargo-sweep-mtime.sh" "$test_home/.local/libexec/flotilla/cargo-sweep-mtime.sh"
done

# The deployed entry point uses real cargo-sweep and caps a recently used target.
# A fresh incremental fixture crosses both ceilings without age-eligible artifacts.
real_sweep=$(command -v cargo-sweep)
test_rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
test_cargo_home=${CARGO_HOME:-$HOME/.cargo}
real_home=$test_root/real-home
real_repo=$real_home/dev/desk
mkdir -p "$real_repo/target/debug/.fingerprint/probe-0123456789abcdef"
: > "$real_repo/target/debug/.fingerprint/probe-0123456789abcdef/lib-probe"
mkdir -p "$real_home/.cargo/bin" "$real_repo/src" "$real_home/.local/libexec/flotilla"
ln -s "$real_sweep" "$real_home/.cargo/bin/cargo-sweep"
cp "$test_home/.local/libexec/flotilla/"*.sh "$real_home/.local/libexec/flotilla/"
printf '[package]\nname="scheduled-cap"\nversion="0.1.0"\nedition="2021"\n' > "$real_repo/Cargo.toml"
: > "$real_repo/src/lib.rs"
for generation in old middle new; do
  generation_dir=$real_repo/target/debug/incremental/probe/s-$generation
  mkdir -p "$generation_dir"
  dd if=/dev/zero of="$generation_dir/artifact.o" bs=1048576 count=2 >/dev/null 2>&1
done
[[ $(du -sk "$real_repo/target" | awk '{print $1}') -gt 5120 ]]
HOME="$real_home" RUSTUP_HOME="$test_rustup_home" CARGO_HOME="$test_cargo_home" \
  XDG_STATE_HOME="$real_home/.local/state" FLOTILLA_SWEEP_LOG="$test_root/real.log" \
  FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE=3MiB FLOTILLA_TARGET_MAX_SIZE=5MiB \
  "$real_home/.local/libexec/flotilla/cargo-sweep-mtime.sh"
[[ $(du -sk "$real_repo/target" | awk '{print $1}') -le 5120 ]]
grep -Fq "mtime-based cargo sweep root=$real_repo reclaimed_bytes=0" "$test_root/real.log"
grep -Eq "size-cap cargo prune root=.*desk reclaimed_bytes=[1-9][0-9]*" "$test_root/real.log"
! grep -Fq 'Failed to clean' "$test_root/real.log"

echo "mtime-based cargo sweep behavior tests passed"
