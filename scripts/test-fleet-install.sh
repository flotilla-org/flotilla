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

generation_one="20260815T210000Z-f111111111111-caaaaaaaaaaaa"
generation_two="20260815T220000Z-f222222222222-cbbbbbbbbbbbb"
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
    printf '#!/usr/bin/env bash\nif [[ "${1:-}" == daemon && "${2:-}" == stop ]]; then echo "daemon stop requested"; exit "${STOP_FAIL:-0}"; fi\nprintf "%s from %s\\n"\n' "$name" "$generation" >"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty\n' >"$bundle/lib/libghostty-vt.so.0"
  TEST_BUNDLE="$bundle" TEST_PLATFORM="$platform" TEST_PROTOCOL="$protocol" TEST_CORRUPT_INNER="$corrupt_inner" python3 - <<'PY'
import hashlib
import json
import os
from pathlib import Path

bundle = Path(os.environ["TEST_BUNDLE"])
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
    "peer_protocol_version": int(os.environ["TEST_PROTOCOL"]),
    "files": files,
}, sort_keys=True) + "\n")
PY
  local artifact="fleet-candidate-linux-x86_64-gnu2.36.tar.gz"
  tar -C "$(dirname "$bundle")" -czf "$directory/$artifact" "$(basename "$bundle")"
  local digest
  digest="$(sha256sum "$directory/$artifact" | awk '{print $1}')"
  TEST_GENERATION="$generation" TEST_PROTOCOL="$protocol" TEST_PLATFORM="$platform" TEST_ARTIFACT="$artifact" TEST_DIGEST="$digest" TEST_DIRECTORY="$directory" python3 - <<'PY'
import json
import os
from pathlib import Path

manifest = {
    "schema_version": 1,
    "kind": "flotilla-fleet-generation",
    "generation": os.environ["TEST_GENERATION"],
    "peer_protocol_version": int(os.environ["TEST_PROTOCOL"]),
    "platforms": {
        os.environ["TEST_PLATFORM"]: {
            "artifact": os.environ["TEST_ARTIFACT"],
            "sha256": os.environ["TEST_DIGEST"],
        }
    },
}
(Path(os.environ["TEST_DIRECTORY"]) / "manifest.json").write_text(json.dumps(manifest, sort_keys=True) + "\n")
PY
}

make_generation "$generation_one" 20
make_generation "$generation_two" 21

cat >"$fixture_root/packages-page-1.json" <<JSON
[
  {"type":"generic","name":"flotilla-fleet","version":"$generation_one","created_at":"2026-08-15T21:00:00Z"},
  {"type":"generic","name":"another-package","version":"ignored","created_at":"2026-08-15T23:00:00Z"},
  {"type":"generic","name":"flotilla-fleet","version":"$generation_two","created_at":"2026-08-15T22:00:00Z"}
]
JSON

cat >"$fake_bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
destination=""
url=""
while (($#)); do
  case "$1" in
    --output) destination="$2"; shift 2 ;;
    --header) shift 2 ;;
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
if [[ "${FAIL_ARTIFACT_FOR:-}" == "$generation" && "$file" == *.tar.gz ]]; then
  printf 'interrupted' >"$destination"
  exit 56
fi
cp "$source" "$destination"
SH
chmod 0755 "$fake_bin/curl"

cat >"$fake_bin/pgrep" <<'SH'
#!/usr/bin/env bash
[[ "${DAEMON_RUNNING:-0}" == 1 ]]
SH
chmod 0755 "$fake_bin/pgrep"

run_installer() {
  HOME="$test_root/home" \
    PATH="$test_root/home/.local/bin:$fake_bin:$PATH" \
    FIXTURE_ROOT="$fixture_root" \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    "$installer" "$@"
}

if HOME="$test_root/home" FLEET_INSTALL_UNAME_S=Darwin FLEET_INSTALL_UNAME_M=arm64 "$installer" status >"$test_root/darwin.out" 2>&1; then
  fail 'Darwin did not fail closed'
fi
grep -Fq '#1553' "$test_root/darwin.out" || fail 'Darwin refusal did not point at #1553'

status_home="$test_root/status-home"
mkdir -p "$status_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$status_home/.config/flotilla/fleet-reader-token"
HOME="$status_home" PATH="$status_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" status >"$test_root/fresh-status.out"
test ! -e "$status_home/.local/opt/flotilla-fleet" || fail 'status mutated the install root'

run_installer "$generation_one" >"$test_root/install-one.out"
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/current")")" = "$generation_one" || fail 'exact generation was not selected'
test -x "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/bin/flotilla" || fail 'candidate binaries were not staged'
test ! -w "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json" || fail 'selected generation is writable'
test "$(stat -c '%a' "$test_root/home/.local/bin")" = 755 || fail 'credential umask leaked into the launcher directory'
for name in flotilla flotillad cleat; do
  grep -Fq '# managed by fleet-install' "$test_root/home/.local/bin/$name" || fail "missing stable $name launcher"
done

status="$(run_installer status)"
grep -Fq "current: $generation_one (peer protocol 20)" <<<"$status" || fail 'status omitted current manifest protocol'
grep -Fq "latest:  $generation_two (peer protocol 21)" <<<"$status" || fail 'status selected the wrong promoted generation'
grep -Fq 'warning: peer protocol changes from 20 to 21' <<<"$status" || fail 'status omitted wire-bump warning'

before_manifest="$(sha256sum "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
run_installer "$generation_one" >/dev/null
after_manifest="$(sha256sum "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
[[ "$before_manifest" == "$after_manifest" ]] || fail 'exact-generation reinstall mutated the release'

exec 9>"$test_root/home/.local/opt/flotilla-fleet/.install.lock"
flock -n 9
if run_installer "$generation_one" >"$test_root/locked.out" 2>&1; then
  fail 'concurrent mutation lock was ignored'
fi
flock -u 9
exec 9>&-
grep -Fq 'another fleet-install mutation is already running' "$test_root/locked.out" || fail 'concurrent mutation error was unclear'

if FAIL_ARTIFACT_FOR="$generation_two" run_installer "$generation_two" >"$test_root/interrupted.out" 2>&1; then
  fail 'interrupted download was accepted'
fi
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/current")")" = "$generation_one" || fail 'interrupted download switched current'
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_two" || fail 'interrupted download published a release'

if DAEMON_RUNNING=1 STOP_FAIL=1 run_installer "$generation_two" >"$test_root/daemon.out" 2>&1; then
  fail 'daemon stop refusal was ignored'
fi
grep -Fq 'flotilla daemon stop' "$test_root/daemon.out" || fail 'daemon refusal omitted recovery instruction'
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/current")")" = "$generation_one" || fail 'failed daemon preflight switched current'

DAEMON_RUNNING=1 STOP_FAIL=0 run_installer latest >"$test_root/latest.out"
grep -Fq "$generation_one -> $generation_two" "$test_root/latest.out" || fail 'latest did not print the exact transition'
grep -Fq 'daemon stop requested' "$test_root/latest.out" || fail 'running daemon was not stopped before switching'
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/current")")" = "$generation_two" || fail 'latest did not select newest promoted generation'
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/previous")")" = "$generation_one" || fail 'switch did not record previous generation'

run_installer rollback >"$test_root/rollback.out"
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/current")")" = "$generation_one" || fail 'rollback did not restore previous generation'
test "$(basename "$(readlink -f "$test_root/home/.local/opt/flotilla-fleet/previous")")" = "$generation_two" || fail 'rollback did not retain displaced generation'

bad_digest="20260815T230000Z-f333333333333-ccccccccccccc"
make_generation "$bad_digest" 21
python3 - "$fixture_root/$bad_digest/manifest.json" <<'PY'
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

bad_manifest="20260815T231000Z-f444444444444-cdddddddddddd"
make_generation "$bad_manifest" 21 linux-x86_64-gnu2.36 yes
if run_installer "$bad_manifest" >"$test_root/manifest.out" 2>&1; then
  fail 'inner manifest mismatch was accepted'
fi
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_manifest" || fail 'manifest rejection published a release'

bad_platform="20260815T232000Z-f555555555555-ceeeeeeeeeeee"
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
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/shadow.out" 2>&1; then
  fail 'PATH shadow was accepted'
fi
grep -Fq 'PATH shadows the fleet launcher' "$test_root/shadow.out" || fail 'PATH shadow error was unclear'
test ! -L "$shadow_home/.local/opt/flotilla-fleet/current" || fail 'PATH shadow switched current'

echo 'fleet-install contract passed'
