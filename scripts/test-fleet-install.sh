#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_root/scripts/fleet-install"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-install-test.XXXXXX")"
cleanup() {
  chmod -R u+w "$test_root" 2>/dev/null || true
  rm -rf "$test_root"
}
trap cleanup EXIT

fail() {
  echo "fleet-install contract failed: $*" >&2
  exit 1
}

file_sha256() {
  python3 - "$1" <<'PY'
import hashlib
import sys

print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
}

file_mode() {
  python3 - "$1" <<'PY'
import os
import stat
import sys

print(format(stat.S_IMODE(os.stat(sys.argv[1]).st_mode), "o"))
PY
}

link_generation() {
  basename "$(readlink "$1")"
}

generation_one="20260815T210000Z-r1-f111111111111-caaaaaaaaaaaa"
generation_two="20260815T220000Z-r2-f222222222222-cbbbbbbbbbbbb"
generation_incomplete="20260815T225000Z-r9-f999999999999-cffffffffffff"
fixture_root="$test_root/packages"
fake_bin="$test_root/fake-bin"
mkdir -p "$test_root/home/.config/flotilla" "$fixture_root" "$fake_bin"
printf 'test-token\n' >"$test_root/home/.config/flotilla/fleet-reader-token"
chmod 0600 "$test_root/home/.config/flotilla/fleet-reader-token"

make_generation() {
  local generation="$1"
  local protocol="$2"
  local platform="${3:-linux-x86_64-gnu2.36}"
  local corrupt_inner="${4:-no}"
  local directory="$fixture_root/$generation"
  local bundle="$test_root/bundle-$generation/fleet-candidate-linux-x86_64-gnu2.36"
  mkdir -p "$directory" "$bundle/bin" "$bundle/lib"
  for name in flotilla flotillad cleat; do
    printf '#!/usr/bin/env bash\nif [[ "${1:-}" == daemon && "${2:-}" == stop ]]; then echo "daemon stop requested"; exit "${STOP_FAIL:-0}"; fi\nif [[ "%s" == flotilla && "${1:-}" == --json && "${2:-}" == fleet ]]; then\n  [[ "${FLEET_HEALTH_FAIL_FOR:-}" != "%s" ]] || exit 1\n  printf '\''{"kind":"fleet_health","hosts":[{"host":"test","is_local":true,"daemon_generation":"%s"}],"dispatch_queue":{"entries":[]}}\\n'\''\n  exit 0\nfi\nif [[ "%s" == flotilla && "${1:-}" == --json && "${2:-}" == host && "${3:-}" == list ]]; then\n  [[ -n "${FLEET_HOST_LIST_JSON:-}" ]] || exit 1\n  printf "%%s\\n" "$FLEET_HOST_LIST_JSON"\n  exit 0\nfi\nprintf "%s from %s\\n"\n' "$name" "$generation" "$generation" "$name" "$name" "$generation" >"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty\n' >"$bundle/lib/libghostty-vt.so.0"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$bundle/install.sh"
  chmod 0755 "$bundle/install.sh"
TEST_BUNDLE="$bundle" TEST_PLATFORM="$platform" TEST_PROTOCOL="$protocol" TEST_CORRUPT_INNER="$corrupt_inner" python3 - <<'PY'
import hashlib
import json
import os
import re
from pathlib import Path

bundle = Path(os.environ["TEST_BUNDLE"])
identity = re.fullmatch(r".+-f([0-9a-f]{12})-c([0-9a-f]{12})", bundle.parent.name.removeprefix("bundle-"))
sources = {"flotilla": identity.group(1) + "1" * 28, "cleat": identity.group(2) + "a" * 28}
files = []
for path in sorted(bundle.rglob("*")):
    if path.is_file():
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if os.environ["TEST_CORRUPT_INNER"] == "yes" and path.name == "cleat":
            digest = "0" * 64
        files.append({"path": str(path.relative_to(bundle)), "sha256": digest, "size_bytes": path.stat().st_size})
(bundle / "manifest.json").write_text(json.dumps({
    "schema_version": 1,
    "kind": "unsigned-fleet-candidate",
    "platform": os.environ["TEST_PLATFORM"],
    "sources": sources,
    "peer_protocol_version": int(os.environ["TEST_PROTOCOL"]),
    "signed": False,
    "files": files,
}, sort_keys=True) + "\n")
PY
  local artifact="fleet-candidate-linux-x86_64-gnu2.36.tar.gz"
  COPYFILE_DISABLE=1 tar -C "$(dirname "$bundle")" -czf "$directory/$artifact" "$(basename "$bundle")"
  local digest
  digest="$(file_sha256 "$directory/$artifact")"
  local size
  size="$(python3 - "$directory/$artifact" <<'PY'
import os
import sys

print(os.path.getsize(sys.argv[1]))
PY
)"
  TEST_GENERATION="$generation" TEST_PROTOCOL="$protocol" TEST_PLATFORM="$platform" TEST_ARTIFACT="$artifact" TEST_DIGEST="$digest" TEST_SIZE="$size" TEST_DIRECTORY="$directory" python3 - <<'PY'
import json
import os
import re
from pathlib import Path

identity = re.fullmatch(r".+-f([0-9a-f]{12})-c([0-9a-f]{12})", os.environ["TEST_GENERATION"])
sources = {"flotilla": identity.group(1) + "1" * 28, "cleat": identity.group(2) + "a" * 28}
manifest = {
    "schema_version": 1,
    "kind": "internal-promoted-fleet-generation",
    "generation": os.environ["TEST_GENERATION"],
    "sources": sources,
    "peer_protocol_version": int(os.environ["TEST_PROTOCOL"]),
    "platforms": {
        os.environ["TEST_PLATFORM"]: {
            "artifact": os.environ["TEST_ARTIFACT"],
            "sha256": os.environ["TEST_DIGEST"],
            "size_bytes": int(os.environ["TEST_SIZE"]),
            "signed": False,
            "state": "installable-internal",
        }
    },
}
(Path(os.environ["TEST_DIRECTORY"]) / "generation.json").write_text(json.dumps(manifest, sort_keys=True) + "\n")
PY
}

add_darwin_derivative() {
  local generation="$1"
  local source_generation="$2"
  local protocol="$3"
  local directory="$fixture_root/$generation"
  local bundle="$test_root/darwin-$generation/fleet-signed-darwin-aarch64"
  mkdir -p "$bundle/bin" "$bundle/lib"
  for name in flotilla flotillad cleat; do
    printf '#!/usr/bin/env bash\nif [[ "%s" == flotilla && "${1:-}" == --json && "${2:-}" == fleet ]]; then\n  [[ "${FLEET_HEALTH_FAIL_FOR:-}" != "%s" ]] || exit 1\n  printf '\''{"kind":"fleet_health","hosts":[{"host":"test","is_local":true,"daemon_generation":"%s"}],"dispatch_queue":{"entries":[]}}\\n'\''\n  exit 0\nfi\nprintf "%s signed for %s\\n"\n' "$name" "$generation" "$generation" "$name" "$generation" >"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty signed\n' >"$bundle/lib/libghostty-vt.dylib"
  chmod 0755 "$bundle/lib/libghostty-vt.dylib"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$bundle/install.sh"
  chmod 0755 "$bundle/install.sh"
  TEST_BUNDLE="$bundle" TEST_GENERATION="$generation" TEST_SOURCE_GENERATION="$source_generation" TEST_PROTOCOL="$protocol" python3 - <<'PY'
import hashlib
import json
import os
from pathlib import Path

bundle = Path(os.environ["TEST_BUNDLE"])
sources = {
    "flotilla": os.environ["TEST_GENERATION"].split("-f", 1)[1].split("-c", 1)[0] + "1" * 28,
    "cleat": os.environ["TEST_GENERATION"].rsplit("-c", 1)[1] + "a" * 28,
}
files = [
    {
        "path": str(path.relative_to(bundle)),
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "size_bytes": path.stat().st_size,
    }
    for path in sorted(bundle.rglob("*"))
    if path.is_file()
]
signing = {
    "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
    "team_id": "973L4GV58R",
    "certificate_sha256": "d" * 64,
    "entitlements_sha256": "e" * 64,
    "options": ["runtime", "timestamp=none"],
}
(bundle / "manifest.json").write_text(json.dumps({
    "schema_version": 1,
    "kind": "signed-fleet-derivative",
    "platform": "darwin-aarch64",
    "sources": sources,
    "build_profile": "release",
    "peer_protocol_version": int(os.environ["TEST_PROTOCOL"]),
    "signed": True,
    "source_generation": os.environ["TEST_SOURCE_GENERATION"],
    "source_artifact_sha256": "c" * 64,
    "signing": signing,
    "files": files,
}, sort_keys=True) + "\n")
PY
  local artifact="fleet-signed-darwin-aarch64.tar.gz"
  COPYFILE_DISABLE=1 tar -C "$(dirname "$bundle")" -czf "$directory/$artifact" "$(basename "$bundle")"
  local digest size
  digest="$(file_sha256 "$directory/$artifact")"
  size="$(python3 - "$directory/$artifact" <<'PY'
import os
import sys

print(os.path.getsize(sys.argv[1]))
PY
)"
  TEST_GENERATION="$generation" TEST_SOURCE_GENERATION="$source_generation" TEST_ARTIFACT="$artifact" \
    TEST_DIGEST="$digest" TEST_SIZE="$size" TEST_DIRECTORY="$directory" python3 - <<'PY'
import json
import os
from pathlib import Path

path = Path(os.environ["TEST_DIRECTORY"]) / "generation.json"
manifest = json.loads(path.read_text())
signing = {
    "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
    "team_id": "973L4GV58R",
    "certificate_sha256": "d" * 64,
    "entitlements_sha256": "e" * 64,
    "options": ["runtime", "timestamp=none"],
}
manifest["source_generation"] = os.environ["TEST_SOURCE_GENERATION"]
manifest["central_signing"] = {
    "derivative_package": "lab-signing/flotilla-fleet-darwin-signed",
    "derivative_version": os.environ["TEST_SOURCE_GENERATION"],
    "attestation": "darwin-signing-attestation.json",
    "attestation_sha256": "f" * 64,
    "cms": "darwin-signing-attestation.cms",
    "cms_sha256": "1" * 64,
    "certificate": "darwin-signing-certificate.pem",
    "certificate_sha256": signing["certificate_sha256"],
    "signing": signing,
}
manifest["platforms"]["darwin-aarch64"] = {
    "artifact": os.environ["TEST_ARTIFACT"],
    "sha256": os.environ["TEST_DIGEST"],
    "size_bytes": int(os.environ["TEST_SIZE"]),
    "signed": True,
    "state": "installable-internal",
    "source_artifact": "fleet-candidate-darwin-aarch64.tar.gz",
    "source_artifact_sha256": "c" * 64,
    "signing": signing,
}
path.write_text(json.dumps(manifest, sort_keys=True) + "\n")
PY
}

make_generation "$generation_one" 20
make_generation "$generation_two" 21
source_generation_two="20260815T215500Z-r2-f222222222222-cbbbbbbbbbbbb"
source_generation_one="20260815T205500Z-r1-f111111111111-caaaaaaaaaaaa"
add_darwin_derivative "$generation_one" "$source_generation_one" 20
add_darwin_derivative "$generation_two" "$source_generation_two" 21

cat >"$fixture_root/packages-page-1.json" <<JSON
[
  {"type":"generic","name":"flotilla-fleet","version":"$generation_one","created_at":"2026-08-15T21:00:00Z"},
  {"type":"generic","name":"another-package","version":"ignored","created_at":"2026-08-15T23:00:00Z"},
  {"type":"generic","name":"flotilla-fleet","version":"$generation_two","created_at":"2026-08-15T22:00:00Z"},
  {"type":"generic","name":"flotilla-fleet","version":"$generation_incomplete","created_at":"2026-08-15T22:50:00Z"}
]
JSON

cat >"$fake_bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
destination=""
url=""
write_out=""
while (($#)); do
  case "$1" in
    --output) destination="$2"; shift 2 ;;
    --header) shift 2 ;;
    --write-out) write_out="$2"; shift 2 ;;
    --fail|--silent|--show-error|--location) shift ;;
    *) url="$1"; shift ;;
  esac
done
[[ -n "$destination" && -n "$url" ]]
if [[ "$url" == *'/api/v1/packages/'* ]]; then
  page="${url##*page=}"
  source="$FIXTURE_ROOT/packages-page-$page.json"
  if [[ ! -f "$source" ]]; then printf '[]\n' >"$destination"; else cp "$source" "$destination"; fi
  exit 0
fi
generation="$(basename "$(dirname "$url")")"
file="$(basename "$url")"
source="$FIXTURE_ROOT/$generation/$file"
if [[ "${FAIL_MANIFEST_FOR:-}" == "$generation" && "$file" == generation.json ]]; then
  [[ -z "$write_out" ]] || printf '503'
  exit 0
fi
if [[ ! -f "$source" ]]; then
  [[ -z "$write_out" ]] || printf '404'
  exit 0
fi
if [[ "${FAIL_ARTIFACT_FOR:-}" == "$generation" && "$file" == *.tar.gz ]]; then
  printf 'interrupted' >"$destination"
  exit 56
fi
cp "$source" "$destination"
[[ -z "$write_out" ]] || printf '200'
SH
chmod 0755 "$fake_bin/curl"

cat >"$fake_bin/pgrep" <<'SH'
#!/usr/bin/env bash
[[ "${DAEMON_RUNNING:-0}" == 1 ]]
SH
chmod 0755 "$fake_bin/pgrep"

cat >"$fake_bin/zsh" <<'SH'
#!/bin/sh
set -eu
[ "$#" -eq 3 ] || exit 98
[ "$1" = -l ] && [ "$2" = -c ] || exit 98
case "$3" in
  'command -v flotilla'|'command -v flotillad'|'command -v cleat') ;;
  *) exit 98 ;;
esac
case ":${PATH:-}:" in
  *":$HOME/.local/bin:"*) exit 97 ;;
esac
PATH="$LOGIN_SHELL_PATH"
export PATH
exec /bin/sh -c "$3"
SH
chmod 0755 "$fake_bin/zsh"

cat >"$fake_bin/systemctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$SYSTEMCTL_LOG"
SH
chmod 0755 "$fake_bin/systemctl"

cat >"$fake_bin/loginctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$LOGINCTL_LOG"
state="$LOGINCTL_LOG.linger"
case "${1:-}" in
  # A redundant re-enable is denied, as polkit does outside a login session.
  enable-linger) if [[ -e "$state" ]]; then exit 1; else touch "$state"; fi ;;
  show-user) if [[ -e "$state" ]]; then printf 'yes\n'; else printf 'no\n'; fi ;;
esac
SH
chmod 0755 "$fake_bin/loginctl"

cat >"$fake_bin/launchctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
current='<none>'
if [[ -L "$FLEET_INSTALL_ROOT/current" ]]; then
  current="$(readlink "$FLEET_INSTALL_ROOT/current")"
fi
printf '%s|%s\n' "$*" "$current" >>"$LAUNCHCTL_LOG"
if [[ "$1" == print-disabled ]]; then
  printf 'disabled services = {\n    "work.flotilla.flotillad" => %s\n}\n' "${LAUNCHD_AGENT_DISABLED:-false}"
fi
SH
chmod 0755 "$fake_bin/launchctl"

cat >"$fake_bin/codesign" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$CODESIGN_LOG"
path="${!#}"
if [[ " $* " == *' --verify '* ]]; then
  [[ -z "${FAIL_CODESIGN_FOR:-}" || "$path" != *"$FAIL_CODESIGN_FOR"* ]] || exit 1
elif [[ " $* " == *' -d '* ]]; then
  printf 'Authority=Apple Development: Robert Wittams (DYYMCPD885)\n' >&2
  printf 'TeamIdentifier=973L4GV58R\n' >&2
  printf 'CDHash=0123456789abcdef\n' >&2
elif [[ " $* " == *' --entitlements '* ]]; then
  printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict/></plist>'
else
  exit 2
fi
SH
chmod 0755 "$fake_bin/codesign"

run_installer() {
  : "${SYSTEMCTL_LOG:=$test_root/systemctl.log}"
  : "${LOGINCTL_LOG:=$test_root/loginctl.log}"
  HOME="$test_root/home" \
    XDG_CONFIG_HOME="$test_root/home/.config" \
    PATH="$test_root/home/.local/bin:$fake_bin:$PATH" \
    SHELL="$fake_bin/zsh" \
    LOGIN_SHELL_PATH="$test_root/home/.local/bin:$fake_bin:/usr/bin:/bin" \
    FIXTURE_ROOT="$fixture_root" \
    SYSTEMCTL_LOG="$SYSTEMCTL_LOG" \
    LOGINCTL_LOG="$LOGINCTL_LOG" \
    FLEET_INSTALL_UNAME_S=Linux \
    FLEET_INSTALL_UNAME_M=x86_64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS="${FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS:-30}" \
    "$installer" "$@"
}

run_darwin_installer() {
  local home="$1"
  shift
  : "${LAUNCHCTL_LOG:=$test_root/launchctl.log}"
  HOME="$home" \
    XDG_CONFIG_HOME="$home/.config" \
    XDG_STATE_HOME="$home/.local/state" \
    PATH="$home/.local/bin:$fake_bin:$PATH" \
    SHELL="$fake_bin/zsh" \
    LOGIN_SHELL_PATH="$home/.local/bin:$fake_bin:/usr/bin:/bin" \
    FIXTURE_ROOT="$fixture_root" \
    CODESIGN_LOG="$test_root/codesign.log" \
    LAUNCHCTL_LOG="$LAUNCHCTL_LOG" \
    LAUNCHD_AGENT_DISABLED="${LAUNCHD_AGENT_DISABLED:-false}" \
    FAIL_CODESIGN_FOR="${FAIL_CODESIGN_FOR:-}" \
    FLEET_INSTALL_ROOT="$home/.local/opt/flotilla-fleet" \
    FLEET_INSTALL_TESTING=1 \
    FLEET_INSTALL_TEST_CODESIGN="$fake_bin/codesign" \
    FLEET_INSTALL_UNAME_S=Darwin \
    FLEET_INSTALL_UNAME_M=arm64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS="${FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS:-30}" \
    "$installer" "$@"
}

status_home="$test_root/status-home"
mkdir -p "$status_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$status_home/.config/flotilla/fleet-reader-token"
HOME="$status_home" PATH="$status_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" status >"$test_root/fresh-status.out"
test ! -e "$status_home/.local/opt/flotilla-fleet" || fail 'status mutated the install root'
grep -Fq 'fleet:   unavailable (daemon not running)' "$test_root/fresh-status.out" \
  || fail 'status did not degrade gracefully without a daemon'

login_path_home="$test_root/login-path-home"
mkdir -p "$login_path_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$login_path_home/.config/flotilla/fleet-reader-token"
if HOME="$login_path_home" PATH="$login_path_home/.local/bin:$fake_bin:$PATH" SHELL="$fake_bin/zsh" \
  LOGIN_SHELL_PATH="$fake_bin:/usr/bin:/bin" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/login-path.out" 2>&1; then
  fail 'inline PATH masked a missing login-shell PATH entry'
fi
grep -Fq 'not reachable through the login shell' "$test_root/login-path.out" \
  || fail 'login-shell PATH failure was unclear'
grep -Fq '~/.zshenv' "$test_root/login-path.out" || fail 'zsh PATH failure omitted ~/.zshenv'

run_installer "$generation_one" >"$test_root/install-one.out"
grep -Fq "generation $generation_one confirmed healthy" "$test_root/install-one.out" \
  || fail 'healthy Linux install was not confirmed'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'exact generation was not selected'
test -x "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/bin/flotilla" || fail 'candidate binaries were not staged'
test ! -w "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json" || fail 'selected generation is writable'
test "$(file_mode "$test_root/home/.local/bin")" = 755 || fail 'credential umask leaked into the launcher directory'
for name in flotilla flotillad cleat; do
  grep -Fq '# managed by fleet-install' "$test_root/home/.local/bin/$name" || fail "missing stable $name launcher"
done
unit="$test_root/home/.config/systemd/user/flotillad.service"
test -f "$unit" || fail 'systemd user unit was not installed'
grep -Fxq 'ExecStart="%h/.local/opt/flotilla-fleet/current/bin/flotillad"' "$unit" \
  || fail 'systemd user unit does not use the stable flotillad path'
grep -Fxq 'Environment="PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin"' "$unit" \
  || fail 'systemd user unit does not expose the fleet binary PATH'
grep -Fxq 'Restart=on-failure' "$unit" || fail 'systemd user unit does not restart after failures'
grep -Fxq -- '--user daemon-reload' "$test_root/systemctl.log" || fail 'systemd user manager was not reloaded'
grep -Fxq -- '--user enable flotillad.service' "$test_root/systemctl.log" || fail 'systemd user unit was not enabled'
grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log" || fail 'systemd user unit was not started'
grep -Eq '^enable-linger .+$' "$test_root/loginctl.log" || fail 'systemd lingering was not enabled'

status="$(run_installer status 2>"$test_root/status.err")"
grep -Fq "current: $generation_one (peer protocol 20)" <<<"$status" || fail 'status omitted current manifest protocol'
grep -Fq "latest:  $generation_two (peer protocol 21)" <<<"$status" || fail 'status selected the wrong promoted generation'
grep -Fq 'fleet:   unavailable (daemon not running)' <<<"$status" || fail 'status did not report unavailable fleet spread'
grep -Fq 'warning: peer protocol changes from 20 to 21' <<<"$status" || fail 'status omitted wire-bump warning'
grep -Fq "skipping incomplete linux-x86_64-gnu2.36 generation $generation_incomplete" "$test_root/status.err" \
  || fail 'latest did not explain why it skipped an incomplete generation'

fleet_health='{"kind":"fleet_health","hosts":[{"host":"feta","is_local":true,"daemon_generation":"111111111111"},{"host":"kiwi","is_local":false,"daemon_generation":"222222222222"},{"host":"mango","is_local":false,"daemon_generation":"111111111111"},{"host":"pear","is_local":false}],"dispatch_queue":{"entries":[]}}'
fleet_status="$(DAEMON_RUNNING=1 FLEET_HOST_LIST_JSON="$fleet_health" run_installer status 2>"$test_root/fleet-status.err")"
grep -Fq 'fleet wire generations:' <<<"$fleet_status" || fail 'status omitted reachable fleet spread'
grep -Fq '  111111111111: feta (local), mango' <<<"$fleet_status" || fail 'status did not group hosts on the current wire generation'
grep -Fq '  222222222222: kiwi' <<<"$fleet_status" || fail 'status omitted a pending wire generation'
grep -Fq '  unknown: pear' <<<"$fleet_status" || fail 'status omitted a host with unknown generation'
unreachable_status="$(DAEMON_RUNNING=1 run_installer status 2>"$test_root/unreachable-status.err")"
grep -Fq 'fleet:   unavailable (daemon query failed)' <<<"$unreachable_status" \
  || fail 'status did not degrade gracefully when the daemon query failed'

for bad_payload in 'not json at all' '{"kind":"host_list","hosts":[]}' '{"kind":"fleet_health","hosts":"nope"}' '{"kind":"fleet_health","hosts":[{"is_local":true}]}'; do
  invalid_status="$(DAEMON_RUNNING=1 FLEET_HOST_LIST_JSON="$bad_payload" run_installer status 2>"$test_root/invalid-status.err")"
  grep -Fq 'fleet:   unavailable (invalid daemon response)' <<<"$invalid_status" \
    || fail "status did not degrade gracefully on invalid daemon payload: $bad_payload"
  grep -Fq "current: $generation_one" <<<"$invalid_status" \
    || fail 'invalid daemon payload disturbed local status reporting'
done

if run_installer "$generation_incomplete" >"$test_root/incomplete-exact.out" 2>&1; then
  fail 'an explicitly requested incomplete generation was accepted'
fi
grep -Fq "generation $generation_incomplete is not complete for linux-x86_64-gnu2.36" \
  "$test_root/incomplete-exact.out" || fail 'incomplete exact-generation error was unclear'

if FAIL_MANIFEST_FOR="$generation_two" run_installer status >"$test_root/latest-fetch.out" 2>&1; then
  fail 'latest silently downgraded after a promoted-generation fetch failure'
fi
grep -Fq "generation manifest request returned HTTP 503" "$test_root/latest-fetch.out" \
  || fail 'latest hid the promoted-generation fetch failure'
if grep -Fq "could not verify advertised generation $generation_two" "$test_root/latest-fetch.out"; then
  fail 'fatal generation fetch was incorrectly treated as a skippable generation'
fi

before_manifest="$(file_sha256 "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
printf '# managed by fleet-install\nstale unit\n' >"$unit"
: >"$test_root/systemctl.log"
run_installer "$generation_one" >/dev/null
after_manifest="$(file_sha256 "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
[[ "$before_manifest" == "$after_manifest" ]] || fail 'exact-generation reinstall mutated the release'
grep -Fxq 'ExecStart="%h/.local/opt/flotilla-fleet/current/bin/flotillad"' "$unit" \
  || fail 'exact-generation reinstall did not refresh the systemd user unit'
test "$(grep -Fxc -- '--user daemon-reload' "$test_root/systemctl.log")" = 1 \
  || fail 'unit refresh did not reload the systemd user manager exactly once'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 1 \
  || fail 'unit refresh did not restart flotillad exactly once'
test "$(grep -Ec '^enable-linger ' "$test_root/loginctl.log")" = 1 \
  || fail 'reinstall with linger already enabled must not re-attempt enable-linger'

ln -s "$$-active-test-owner" "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
if run_installer "$generation_one" >"$test_root/locked.out" 2>&1; then
  fail 'concurrent mutation lock was ignored'
fi
rm "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
grep -Fq 'another fleet-install mutation is already running' "$test_root/locked.out" || fail 'concurrent mutation error was unclear'

ln -s '999999999-stale-test-owner' "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
run_installer "$generation_one" >"$test_root/stale-lock.out"
test ! -L "$test_root/home/.local/opt/flotilla-fleet/.install.lock" || fail 'stale install lock was not reclaimed'

if FAIL_ARTIFACT_FOR="$generation_two" run_installer "$generation_two" >"$test_root/interrupted.out" 2>&1; then
  fail 'interrupted download was accepted'
fi
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'interrupted download switched current'
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_two" || fail 'interrupted download published a release'

if DAEMON_RUNNING=1 STOP_FAIL=1 run_installer "$generation_two" >"$test_root/daemon.out" 2>&1; then
  fail 'daemon stop refusal was ignored'
fi
grep -Fq 'flotilla daemon stop' "$test_root/daemon.out" || fail 'daemon refusal omitted recovery instruction'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'failed daemon preflight switched current'

: >"$test_root/systemctl.log"
if FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=0 \
  run_installer "$generation_two" >"$test_root/health-rollback.out" 2>&1; then
  fail 'unhealthy Linux generation was accepted'
fi
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'unhealthy Linux generation did not roll current back'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_two" \
  || fail 'unhealthy Linux generation was not retained as previous'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 2 \
  || fail 'Linux health failure did not restart the candidate and restored daemon'
grep -Fq "generation $generation_two failed health confirmation; rolled back to $generation_one" \
  "$test_root/health-rollback.out" || fail 'Linux automatic rollback was not reported loudly'

: >"$test_root/systemctl.log"
FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=2 \
  run_installer "$generation_two" >"$test_root/interrupted-confirmation.out" 2>&1 &
interrupted_installer_pid=$!
for _ in {1..50}; do
  if [[ "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" == "$generation_two" ]] \
    && grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log"; then
    break
  fi
  sleep 0.1
done
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" \
  || fail 'interrupted-confirmation test never selected the candidate'
grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log" \
  || fail 'interrupted-confirmation test did not start the candidate daemon'
kill "$interrupted_installer_pid"
wait "$interrupted_installer_pid" 2>/dev/null || true
for _ in {1..50}; do
  [[ "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" == "$generation_one" ]] && break
  sleep 0.1
done
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'detached health watchdog did not roll back after the installer exited'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 2 \
  || fail 'detached Linux watchdog did not restart the candidate and restored daemon'

DAEMON_RUNNING=1 STOP_FAIL=0 run_installer latest >"$test_root/latest.out"
grep -Fq "generation $generation_two confirmed healthy" "$test_root/latest.out" \
  || fail 'healthy Linux upgrade was not confirmed'
grep -Fq "$generation_one -> $generation_two" "$test_root/latest.out" || fail 'latest did not print the exact transition'
grep -Fq 'daemon stop requested' "$test_root/latest.out" || fail 'running daemon was not stopped before switching'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" || fail 'latest did not select newest promoted generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_one" || fail 'switch did not record previous generation'

run_installer rollback >"$test_root/rollback.out"
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'rollback did not restore previous generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_two" || fail 'rollback did not retain displaced generation'

custom_root="$test_root/custom-root"
custom_bin="$test_root/custom-bin"
HOME="$test_root/home" XDG_CONFIG_HOME="$test_root/home/.config" PATH="$custom_bin:$fake_bin:$PATH" \
  SHELL="$fake_bin/zsh" LOGIN_SHELL_PATH="$custom_bin:$fake_bin:/usr/bin:/bin" \
  FIXTURE_ROOT="$fixture_root" SYSTEMCTL_LOG="$test_root/systemctl.log" LOGINCTL_LOG="$test_root/loginctl.log" \
  FLEET_INSTALL_ROOT="$custom_root" FLEET_INSTALL_BIN_DIR="$custom_bin" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/custom-paths.out"
grep -Fxq "ExecStart=\"$custom_root/current/bin/flotillad\"" "$unit" \
  || fail 'systemd user unit ignored the configured fleet root'
grep -Fxq "Environment=\"PATH=$custom_bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin\"" "$unit" \
  || fail 'systemd user unit ignored the configured fleet binary directory'

darwin_home="$test_root/darwin-home"
mkdir -p "$darwin_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_home/.config/flotilla/fleet-reader-token"
run_darwin_installer "$darwin_home" "$generation_one" >"$test_root/darwin-install-one.out"
: >"$test_root/launchctl.log"
if FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=0 \
  run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-health-rollback.out" 2>&1; then
  fail 'unhealthy Darwin generation was accepted'
fi
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'unhealthy Darwin generation did not roll current back'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin health check did not start the candidate generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_one$" "$test_root/launchctl.log" \
  || fail 'Darwin health failure did not restart the restored generation'
grep -Fq "generation $generation_two failed health confirmation; rolled back to $generation_one" \
  "$test_root/darwin-health-rollback.out" || fail 'Darwin automatic rollback was not reported loudly'
: >"$test_root/codesign.log"
: >"$test_root/launchctl.log"
run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-install.out"
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_two" \
  || fail 'signed Darwin generation was not selected'
test -f "$darwin_home/.local/opt/flotilla-fleet/releases/$generation_two/lib/libghostty-vt.dylib" \
  || fail 'signed Darwin dynamic library was not installed'
test "$(grep -c -- '--verify' "$test_root/codesign.log")" = 4 \
  || fail 'Darwin install did not strictly verify every Mach-O payload'
test "$(grep -c -- '--entitlements' "$test_root/codesign.log")" = 4 \
  || fail 'Darwin install did not verify every Mach-O entitlement set'
launch_agent="$darwin_home/Library/LaunchAgents/work.flotilla.flotillad.plist"
python3 - "$launch_agent" "$darwin_home" <<'PY' || fail 'Darwin launchd agent content is incorrect'
import plistlib
import sys

with open(sys.argv[1], "rb") as source:
    agent = plistlib.load(source)
home = sys.argv[2]
assert agent["Label"] == "work.flotilla.flotillad"
assert agent["ProgramArguments"] == [
    f"{home}/.local/opt/flotilla-fleet/current/bin/flotillad",
    "--config-dir",
    f"{home}/.config/flotilla",
    "--state-dir",
    f"{home}/.local/state/flotilla",
    "--socket",
    f"{home}/.config/flotilla/run/flotilla.sock",
]
assert agent["EnvironmentVariables"]["PATH"].split(":")[0] == f"{home}/.local/bin"
assert "/usr/sbin" in agent["EnvironmentVariables"]["PATH"].split(":")
assert "/sbin" in agent["EnvironmentVariables"]["PATH"].split(":")
assert agent["StandardErrorPath"] == f"{home}/Library/Logs/flotilla/flotillad.stderr.log"
assert agent["StandardOutPath"] == f"{home}/Library/Logs/flotilla/flotillad.stdout.log"
assert agent["RunAtLoad"] is True
assert agent["KeepAlive"] == {"SuccessfulExit": False}
PY
test -d "$darwin_home/Library/Logs/flotilla" \
  || fail 'Darwin install did not create the launchd log directory'
grep -Eq "^bootout gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_one$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not unload the old agent before the generation flip'
grep -Eq "^enable gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not enable the agent after selecting the generation'
grep -Eq "^bootstrap gui/[0-9]+ $launch_agent\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not bootstrap the selected generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not restart through launchd after the generation flip'

: >"$test_root/launchctl.log"
run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-reinstall.out"
test "$(grep -Ec '^bootout gui/[0-9]+/work\.flotilla\.flotillad\|' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not unload the agent exactly once'
test "$(grep -Ec '^bootstrap gui/[0-9]+ ' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not bootstrap the refreshed agent exactly once'
test "$(grep -Ec '^kickstart -k gui/[0-9]+/work\.flotilla\.flotillad\|' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not restart the agent exactly once'

: >"$test_root/launchctl.log"
LAUNCHD_AGENT_DISABLED=true run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-dev-mode-install.out"
grep -Fq 'preserving flotillad dev mode' "$test_root/darwin-dev-mode-install.out" \
  || fail 'Darwin install did not report preserved dev mode'
if grep -Eq '^(enable|bootstrap|kickstart) ' "$test_root/launchctl.log"; then
  fail 'Darwin install restarted the fleet agent while dev mode was active'
fi

darwin_previous_target="releases/$generation_one"
: >"$test_root/launchctl.log"
run_darwin_installer "$darwin_home" rollback >"$test_root/darwin-rollback.out"
grep -Eq "^bootstrap gui/[0-9]+ $launch_agent\\|$darwin_previous_target$" "$test_root/launchctl.log" \
  || fail 'Darwin rollback did not bootstrap after selecting the previous generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|$darwin_previous_target$" "$test_root/launchctl.log" \
  || fail 'Darwin rollback did not restart the selected generation through launchd'

darwin_fail_home="$test_root/darwin-fail-home"
mkdir -p "$darwin_fail_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_fail_home/.config/flotilla/fleet-reader-token"
if FAIL_CODESIGN_FOR=bin/cleat run_darwin_installer "$darwin_fail_home" "$generation_two" \
  >"$test_root/darwin-signature-failure.out" 2>&1; then
  fail 'Darwin signature failure was accepted'
fi
grep -Fq 'strict signature verification failed: bin/cleat' "$test_root/darwin-signature-failure.out" \
  || fail 'Darwin signature failure was unclear'
test ! -L "$darwin_fail_home/.local/opt/flotilla-fleet/current" \
  || fail 'Darwin signature failure selected a generation'
test ! -e "$darwin_fail_home/.local/opt/flotilla-fleet/releases/$generation_two" \
  || fail 'Darwin signature failure published a release'

cp "$fixture_root/$generation_two/generation.json" "$test_root/generation-two-good.json"
python3 - "$fixture_root/$generation_two/generation.json" <<'PY'
import json
import sys

path = sys.argv[1]
manifest = json.load(open(path))
manifest["central_signing"]["signing"]["team_id"] = "ATTACKERTEAM"
open(path, "w").write(json.dumps(manifest) + "\n")
PY
darwin_team_home="$test_root/darwin-team-home"
mkdir -p "$darwin_team_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_team_home/.config/flotilla/fleet-reader-token"
if run_darwin_installer "$darwin_team_home" "$generation_two" >"$test_root/darwin-team.out" 2>&1; then
  fail 'Darwin artifact from an untrusted Apple team was accepted'
fi
grep -Fq 'trusted Apple team 973L4GV58R' "$test_root/darwin-team.out" \
  || fail 'Darwin team rejection was unclear'
mv "$test_root/generation-two-good.json" "$fixture_root/$generation_two/generation.json"

bad_digest="20260815T230000Z-r3-f333333333333-ccccccccccccc"
make_generation "$bad_digest" 21
python3 - "$fixture_root/$bad_digest/generation.json" <<'PY'
import json
import sys
path = sys.argv[1]
manifest = json.load(open(path))
manifest["platforms"]["linux-x86_64-gnu2.36"]["sha256"] = "0" * 64
open(path, "w").write(json.dumps(manifest) + "\n")
PY
if run_installer "$bad_digest" >"$test_root/digest.out" 2>&1; then
  fail 'outer digest mismatch was accepted'
fi
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_digest" || fail 'digest rejection published a release'

bad_manifest="20260815T231000Z-r4-f444444444444-cdddddddddddd"
make_generation "$bad_manifest" 21 linux-x86_64-gnu2.36 yes
if run_installer "$bad_manifest" >"$test_root/manifest.out" 2>&1; then
  fail 'inner manifest mismatch was accepted'
fi
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_manifest" || fail 'manifest rejection published a release'

bad_platform="20260815T232000Z-r5-f555555555555-ceeeeeeeeeeee"
make_generation "$bad_platform" 21 darwin-aarch64
if run_installer "$bad_platform" >"$test_root/platform.out" 2>&1; then
  fail 'wrong-platform generation was accepted'
fi
grep -Fq 'no linux-x86_64-gnu2.36 artifact' "$test_root/platform.out" || fail 'wrong-platform error was unclear'

shadow_home="$test_root/shadow-home"
shadow_bin="$test_root/shadow-bin"
mkdir -p "$shadow_home/.config/flotilla" "$shadow_bin"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$shadow_home/.config/flotilla/fleet-reader-token"
printf '#!/bin/sh\nexit 0\n' >"$shadow_bin/cleat"
chmod 0755 "$shadow_bin/cleat"
if HOME="$shadow_home" PATH="$shadow_bin:$shadow_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  SHELL="$fake_bin/zsh" LOGIN_SHELL_PATH="$shadow_bin:$shadow_home/.local/bin:$fake_bin:/usr/bin:/bin" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/shadow.out" 2>&1; then
  fail 'PATH shadow was accepted'
fi
grep -Fq 'PATH shadows the fleet launcher for cleat' "$test_root/shadow.out" || fail 'cleat PATH shadow error was unclear'
test ! -L "$shadow_home/.local/opt/flotilla-fleet/current" || fail 'PATH shadow switched current'

echo 'fleet-install contract passed'
