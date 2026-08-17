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
    printf '#!/usr/bin/env bash\nif [[ "${1:-}" == daemon && "${2:-}" == stop ]]; then echo "daemon stop requested"; exit "${STOP_FAIL:-0}"; fi\nif [[ "%s" == flotilla && "${1:-}" == --json && "${2:-}" == host && "${3:-}" == list ]]; then\n  [[ -n "${FLEET_HOST_LIST_JSON:-}" ]] || exit 1\n  printf "%%s\\n" "$FLEET_HOST_LIST_JSON"\n  exit 0\nfi\nprintf "%s from %s\\n"\n' "$name" "$name" "$generation" >"$bundle/bin/$name"
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
  local directory="$fixture_root/$generation"
  local bundle="$test_root/darwin-$generation/fleet-signed-darwin-aarch64"
  mkdir -p "$bundle/bin" "$bundle/lib"
  for name in flotilla flotillad cleat; do
    printf '%s signed for %s\n' "$name" "$generation" >"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty signed\n' >"$bundle/lib/libghostty-vt.dylib"
  chmod 0755 "$bundle/lib/libghostty-vt.dylib"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$bundle/install.sh"
  chmod 0755 "$bundle/install.sh"
  TEST_BUNDLE="$bundle" TEST_GENERATION="$generation" TEST_SOURCE_GENERATION="$source_generation" python3 - <<'PY'
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
    "peer_protocol_version": 21,
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
add_darwin_derivative "$generation_two" "$source_generation_two"

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
  HOME="$test_root/home" \
    PATH="$test_root/home/.local/bin:$fake_bin:$PATH" \
    FIXTURE_ROOT="$fixture_root" \
    FLEET_INSTALL_UNAME_S=Linux \
    FLEET_INSTALL_UNAME_M=x86_64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    "$installer" "$@"
}

run_darwin_installer() {
  local home="$1"
  shift
  HOME="$home" \
    PATH="$home/.local/bin:$fake_bin:$PATH" \
    FIXTURE_ROOT="$fixture_root" \
    CODESIGN_LOG="$test_root/codesign.log" \
    FAIL_CODESIGN_FOR="${FAIL_CODESIGN_FOR:-}" \
    FLEET_INSTALL_TESTING=1 \
    FLEET_INSTALL_TEST_CODESIGN="$fake_bin/codesign" \
    FLEET_INSTALL_UNAME_S=Darwin \
    FLEET_INSTALL_UNAME_M=arm64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
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

run_installer "$generation_one" >"$test_root/install-one.out"
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'exact generation was not selected'
test -x "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/bin/flotilla" || fail 'candidate binaries were not staged'
test ! -w "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json" || fail 'selected generation is writable'
test "$(file_mode "$test_root/home/.local/bin")" = 755 || fail 'credential umask leaked into the launcher directory'
for name in flotilla flotillad cleat; do
  grep -Fq '# managed by fleet-install' "$test_root/home/.local/bin/$name" || fail "missing stable $name launcher"
done

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
run_installer "$generation_one" >/dev/null
after_manifest="$(file_sha256 "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
[[ "$before_manifest" == "$after_manifest" ]] || fail 'exact-generation reinstall mutated the release'

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

DAEMON_RUNNING=1 STOP_FAIL=0 run_installer latest >"$test_root/latest.out"
grep -Fq "$generation_one -> $generation_two" "$test_root/latest.out" || fail 'latest did not print the exact transition'
grep -Fq 'daemon stop requested' "$test_root/latest.out" || fail 'running daemon was not stopped before switching'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" || fail 'latest did not select newest promoted generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_one" || fail 'switch did not record previous generation'

run_installer rollback >"$test_root/rollback.out"
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'rollback did not restore previous generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_two" || fail 'rollback did not retain displaced generation'

darwin_home="$test_root/darwin-home"
mkdir -p "$darwin_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_home/.config/flotilla/fleet-reader-token"
: >"$test_root/codesign.log"
run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-install.out"
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_two" \
  || fail 'signed Darwin generation was not selected'
test -f "$darwin_home/.local/opt/flotilla-fleet/releases/$generation_two/lib/libghostty-vt.dylib" \
  || fail 'signed Darwin dynamic library was not installed'
test "$(grep -c -- '--verify' "$test_root/codesign.log")" = 4 \
  || fail 'Darwin install did not strictly verify every Mach-O payload'
test "$(grep -c -- '--entitlements' "$test_root/codesign.log")" = 4 \
  || fail 'Darwin install did not verify every Mach-O entitlement set'

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
for name in flotilla flotillad cleat; do
  printf '#!/bin/sh\nexit 0\n' >"$shadow_bin/$name"
  chmod 0755 "$shadow_bin/$name"
done
if HOME="$shadow_home" PATH="$shadow_bin:$shadow_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/shadow.out" 2>&1; then
  fail 'PATH shadow was accepted'
fi
grep -Fq 'PATH shadows the fleet launcher' "$test_root/shadow.out" || fail 'PATH shadow error was unclear'
test ! -L "$shadow_home/.local/opt/flotilla-fleet/current" || fail 'PATH shadow switched current'

echo 'fleet-install contract passed'
