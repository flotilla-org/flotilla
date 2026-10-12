#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
root="$(mktemp -d)"
trap 'rm -rf -- "$root"' EXIT
source "$repo_root/scripts/fleet-install"
WORK_DIR="$root"
export VALIDATION_TEST_ROOT="$root"
# This executable stands in for the candidate binary's process boundary.
cat >"$root/candidate" <<'CANDIDATE'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == --socket && "$3" == resource && "$4" == validate && "$5" == --from-daemon ]]
count=$(cat "$VALIDATION_TEST_ROOT/count" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" >"$VALIDATION_TEST_ROOT/count"
case "$VALIDATION_TEST_MODE" in
  recover) [[ "$count" -gt 2 ]] && exit 0; echo 'daemon kind discovery failed: 500 Internal Server Error' >&2 ;;
  busy) echo 'resource store busy: read deadline exceeded' >&2 ;;
  invalid) echo 'stored resource decode refused' >&2 ;;
  fault) echo 'unrelated server failure: 500 Internal Server Error' >&2 ;;
esac
exit 1
CANDIDATE
chmod +x "$root/candidate"
# Backoff is injected at the shell boundary; assert delays without wall time.
sleep() { echo "$1" >>"$root/delays"; }
export VALIDATION_TEST_MODE=recover
validate_live_store "$root/candidate" >"$root/output" 2>&1
[[ "$(cat "$root/count")" == 3 ]]
[[ "$(cat "$root/delays")" == $'1\n2' ]]
rm "$root/count" "$root/delays"
export VALIDATION_TEST_MODE=busy
if validate_live_store "$root/candidate" >"$root/output" 2>&1; then exit 1; fi
[[ "$(cat "$root/count")" == 4 ]]
[[ "$(cat "$root/delays")" == $'1\n2\n4' ]]
rm "$root/count" "$root/delays"
export VALIDATION_TEST_MODE=invalid
if validate_live_store "$root/candidate" >"$root/output" 2>&1; then exit 1; fi
[[ "$(cat "$root/count")" == 1 ]]
[[ ! -e "$root/delays" ]]
rm "$root/count"
export VALIDATION_TEST_MODE=fault
if validate_live_store "$root/candidate" >"$root/output" 2>&1; then exit 1; fi
[[ "$(cat "$root/count")" == 1 ]]
[[ ! -e "$root/delays" ]]
echo 'fleet validation retry contract passed'
