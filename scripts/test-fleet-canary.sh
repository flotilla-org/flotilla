#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/bin" "$root/fleet/releases/test-generation/bin"
# These stand in for the SSH and hostname subprocess boundaries. The remote
# verifies the bootstrap transfer, rather than accepting an arbitrary command.
cat >"$root/bin/hostname" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${CANARY_TEST_HOST:-consumer}"
EOF
cat >"$root/bin/ssh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == *'BatchMode=yes -T feta '* && "$*" == *'--canary test-generation'* ]]
mkdir -p "$CANARY_TEST_ROOT/bootstrap"
tar -xf - -C "$CANARY_TEST_ROOT/bootstrap"
test -f "$CANARY_TEST_ROOT/bootstrap/install.sh"
test -f "$CANARY_TEST_ROOT/bootstrap/generation_validation.py"
printf 'canary\n' >>"$CANARY_TEST_ROOT/order"
exit "${CANARY_TEST_FAILURE:-0}"
EOF
chmod +x "$root/bin/"*
export PATH="$root/bin:$PATH" CANARY_TEST_ROOT="$root" FLEET_INSTALL_ROOT="$root/fleet"
export FLEET_GENERATION_VALIDATOR="$repo_root/ci/fleet-candidates/generation_validation.py"
unset FLEET_INSTALL_SKIP_CANARY

# An incoming activation must gate before validation, service mutation or link
# switching, even when the bootstrap installer did not know about the canary.
activation='source "$1/scripts/fleet-install"
PLATFORM=linux-x86_64-gnu2.36
verify_installed_release() { :; }
current_generation() { :; }
ensure_launchers_and_path() { printf "launchers\n" >>"$CANARY_TEST_ROOT/order"; }
prepare_linux_daemon_service() { printf "service\n" >>"$CANARY_TEST_ROOT/order"; }
prepare_darwin_daemon_service() { :; }
switch_to() { printf "switch\n" >>"$CANARY_TEST_ROOT/order"; }
restart_linux_daemon_service() { :; }
restart_darwin_daemon_service() { :; }
await_confirmation_watchdog() { :; }
prune_after_confirmation() { :; }
activate_staged_generation test-generation "" token'
touch "$root/fleet/releases/test-generation/.generation.json"
cat >"$root/fleet/releases/test-generation/bin/flotilla" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$root/fleet/releases/test-generation/bin/flotilla"
bash -c "$activation" bash "$repo_root" >"$root/success.log" 2>&1
[[ "$(head -1 "$root/order")" == canary ]]
[[ "$(tail -1 "$root/order")" == switch ]]

# Canary refusal cannot reach any active-generation or daemon-service mutation.
: >"$root/order"
if CANARY_TEST_FAILURE=1 bash -c "$activation" bash "$repo_root" >"$root/refusal.log" 2>&1; then
  echo 'failed canary allowed activation' >&2; exit 1
fi
[[ "$(cat "$root/order")" == canary ]]
grep -Fq 'no generation switched' "$root/refusal.log"

# An explicit emergency bypass is visible and survives incoming-installer exec.
: >"$root/order"
FLEET_INSTALL_SKIP_CANARY=1 bash -c "$activation" bash "$repo_root" >"$root/skip.log" 2>&1
grep -Fq 'WARNING canary explicitly skipped' "$root/skip.log"
! grep -Fxq canary "$root/order"

# The feta staging path exercises its own unpacked generation and never activates.
cat >"$root/fleet/releases/test-generation/fleet-canary.py" <<'PY'
import os
from pathlib import Path
import sys
assert sys.argv[1].endswith('/releases/test-generation')
Path(os.environ['CANARY_TEST_ROOT'], 'local-probe').write_text('ran')
PY
CANARY_TEST_HOST=feta bash -c 'source "$1/scripts/fleet-install"; CANARY_ONLY=1; finish_staging test-generation "" token' bash "$repo_root"
test -f "$root/local-probe"
test ! -L "$root/fleet/current"

# Stage-only is refused on other hosts, including before package downloads.
if bash "$repo_root/scripts/fleet-install" --canary test-generation >"$root/wrong-host.log" 2>&1; then
  echo 'non-feta stage-only canary accepted' >&2; exit 1
fi
grep -Fq -- '--canary must run on feta' "$root/wrong-host.log"
python3 "$repo_root/scripts/test-fleet-canary.py"
# Build the actual CLI and daemon: admission must never be covered only by fakes.
cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin flotilla --bin flotillad \
  --message-format=json >"$root/cargo-artifacts.jsonl"
# Cargo reports the actual executables, including target/profile subdirectories.
python3 - "$repo_root" "$root/cargo-artifacts.jsonl" <<'PYTHON'
import json
from pathlib import Path
import subprocess
import sys

artifacts = {}
for line in Path(sys.argv[2]).read_text().splitlines():
    message = json.loads(line)
    if message.get('reason') == 'compiler-artifact' and message.get('executable'):
        artifacts[message['target']['name']] = message['executable']
for name in ('flotilla', 'flotillad'):
    if name not in artifacts:
        raise SystemExit(f'missing cargo artifact: {name}')
subprocess.run(['python3', str(Path(sys.argv[1]) / 'scripts/test-fleet-canary-real.py'),
                artifacts['flotilla'], artifacts['flotillad']], check=True)
PYTHON
echo 'fleet canary contract passed'
