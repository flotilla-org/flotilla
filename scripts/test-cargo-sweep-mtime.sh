#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/flotilla-mtime-sweep-test.XXXXXX")
test_home=$test_root/home
stub_log=$test_root/invocations.log
sweep_log=$test_root/sweep.log

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

mkdir -p "$test_home/.cargo/bin" "$test_home/dev/desk/target" "$test_home/dev/no-target" "$test_home/dev/flotilla-repos/convoy/target"
: > "$test_home/dev/flotilla-repos/convoy/Cargo.toml"
dd if=/dev/zero of="$test_home/dev/desk/target/stale" bs=1048576 count=1 >/dev/null 2>&1
dd if=/dev/zero of="$test_home/dev/flotilla-repos/convoy/target/stale" bs=1048576 count=1 >/dev/null 2>&1

cat > "$test_home/.cargo/bin/cargo-sweep" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$FLOTILLA_SWEEP_TEST_INVOCATIONS"
find "${@: -1}" -type f -name stale -delete
STUB
chmod +x "$test_home/.cargo/bin/cargo-sweep"

HOME="$test_home" \
  FLOTILLA_SWEEP_LOG="$sweep_log" \
  FLOTILLA_SWEEP_TEST_INVOCATIONS="$stub_log" \
  "$repo_root/scripts/cargo-sweep-mtime.sh"

grep -Fxq "sweep --time 3 $test_home/dev/desk" "$stub_log"
grep -Fxq "sweep --time 3 $test_home/dev/flotilla-repos/convoy" "$stub_log"
[[ $(wc -l < "$stub_log") == 2 ]]
! grep -Fq "$test_home/dev/no-target" "$stub_log"
grep -Fq "mtime-based cargo sweep completed: reclaimed_bytes=2097152 failed_roots=0" "$sweep_log"

echo "mtime-based cargo sweep behavior tests passed"
