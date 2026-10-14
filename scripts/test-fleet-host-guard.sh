#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="${FLEET_INSTALL_UNDER_TEST:-$repo_root/scripts/fleet-install}"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT
fail() { echo "fleet host guard failed: $*" >&2; exit 1; }

# Process boundary: source the real watchdog loop, injecting health, clock and
# sleep. No daemon, wall-clock wait, service manager or live host is involved.
for scenario in pending rotated stale absent bounded healthy; do
  SCENARIO="$scenario" ROOT="$root" INSTALLER="$installer" \
    FLEET_GENERATION_VALIDATOR="$repo_root/ci/fleet-candidates/generation_validation.py" \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=2 FLEET_INSTALL_MAX_WAIT_SECONDS=5 \
    XDG_STATE_HOME="$root/state" bash <<'BASH'
set -euo pipefail
source "$INSTALLER"
WORK_DIR="$ROOT/work"
mkdir -p "$WORK_DIR" "$DAEMON_STATE_DIR/log"
MAX_WAIT_SECONDS=5
FLEET_INSTALL_STARTUP_SINCE=100
case "$SCENARIO" in
  pending|rotated|bounded|healthy) timestamp='1970-01-01T00:01:41Z' ;;
  *) timestamp='1970-01-01T00:01:39Z' ;;
esac
printf '{"timestamp":"%s","fields":{"message":"daemon startup phase remains pending"}}\n' "$timestamp" >"$DAEMON_STATE_DIR/log/flotillad.jsonl"
[[ "$SCENARIO" != absent ]] || rm "$DAEMON_STATE_DIR/log/flotillad.jsonl"
current_generation() { echo candidate; }
probe_daemon_health() {
  [[ "$SCENARIO" == healthy ]] || { [[ ( "$SCENARIO" == pending || "$SCENARIO" == rotated ) && $SECONDS -ge 4 ]]; }
}
sleep() {
  SECONDS=$((SECONDS + 1))
  # Once observed, rotation cannot erase this attempt's startup evidence.
  if [[ "$SCENARIO" == rotated ]]; then : >"$DAEMON_STATE_DIR/log/flotillad.jsonl"; fi
}
# Unsetting removes Bash's wall-clock semantics; sleep advances this clock.
unset SECONDS
SECONDS=0
if wait_for_daemon_generation candidate; then
  [[ "$SCENARIO" == pending || "$SCENARIO" == rotated || "$SCENARIO" == healthy ]] || exit 1
else
  [[ "$SCENARIO" != pending && "$SCENARIO" != rotated && "$SCENARIO" != healthy ]] || exit 1
fi
case "$SCENARIO" in
  pending|rotated) [[ $SECONDS == 4 ]] ;;
  healthy) [[ $SECONDS == 0 ]] ;;
  bounded) [[ $SECONDS == 5 ]] ;;
  *) [[ $SECONDS == 2 ]] ;;
esac
BASH
done

# A floor above the host's capacity refuses before downloads, links, locks or
# service changes, including first installation into a missing state directory.
if HOME="$root/home" XDG_STATE_HOME="$root/state" \
  "$installer" --min-free-bytes 999999999999999 --max-wait-seconds 600 latest >"$root/refusal" 2>&1; then
  fail 'low-space host accepted'
fi
grep -Fq 'below free-space floor' "$root/refusal" || fail 'missing actionable space refusal'
test ! -e "$root/home/.local/opt/flotilla-fleet" || fail 'refusal created fleet state'

# Option values survive the export boundary used by incoming activation and
# detached confirmation; a smaller maximum is accepted as an explicit cap.
ROOT="$root" INSTALLER="$installer" bash <<'BASH'
set -euo pipefail
unset FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS FLEET_INSTALL_MAX_WAIT_SECONDS FLEET_INSTALL_MIN_FREE_BYTES
source "$INSTALLER"
[[ "$CONFIRM_TIMEOUT_SECONDS" == 90 && "$MAX_WAIT_SECONDS" == 600 && "$MIN_FREE_BYTES" == 5368709120 ]]
host_platform() { echo linux-x86_64-gnu2.36; }
install_generation() {
  bash -c '[[ "$FLEET_INSTALL_MIN_FREE_BYTES" == 100 && "$FLEET_INSTALL_MAX_WAIT_SECONDS" == 5 ]]'
}
main --min-free-bytes 000100 --max-wait-seconds 5 candidate
BASH
echo 'fleet host guard passed'
