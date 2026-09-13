#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
workflow="$repo_root/.forgejo/workflows/fleet-candidates.yml"
builder="$repo_root/ci/fleet-candidates/build-candidate.sh"
setup="$repo_root/ci/fleet-candidates/setup-linux-toolchain.sh"
installer="$repo_root/ci/fleet-candidates/install-candidate.sh"
toolchain="$repo_root/ci/fleet-candidates/cleat-toolchain.sh"

bash -n "$builder" "$setup" "$installer" "$toolchain"
if grep -Fq 'test "$(zig version)" = "0.16.0"' "$builder"; then
  echo 'candidate builder must derive the Zig version from the Cleat toolchain pin' >&2
  exit 1
fi
grep -Fq 'read_cleat_toolchain_value "$cleat_root/tools/ghostty-toolchain.toml" zig version' "$builder"

# shellcheck disable=SC1090
source "$toolchain"
test "$(read_cleat_toolchain_value /dev/stdin zig version <<'EOF'
[zig]
version = "0.16.0"

[zig_sha256]
x86_64-linux = "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
EOF
)" = 0.16.0
test "$(read_cleat_toolchain_value /dev/stdin zig_sha256 x86_64-linux <<'EOF'
[zig]
version = "0.16.0"

[zig_sha256]
x86_64-linux = "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
EOF
)" = 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00

grep -Fq 'workflow_dispatch:' "$workflow"
if grep -Eq '^[[:space:]]+(push|pull_request):' "$workflow"; then
  echo 'fleet candidate workflow must be manual-only' >&2
  exit 1
fi
grep -Fq 'runs-on: debian-12' "$workflow"
grep -Fq 'runs-on: darwin-aarch64' "$workflow"
# The literals below are workflow syntax and shell source, not expressions for
# this contract test to expand.
# shellcheck disable=SC2016
grep -Fq 'FLEET_ORCHESTRATION_SHA: ${{ forgejo.sha }}' "$workflow"
# shellcheck disable=SC2016
test "$(grep -Fc 'git -C orchestration fetch --depth=1 origin "$FLEET_ORCHESTRATION_SHA"' "$workflow")" -eq 2
if grep -F 'git -C orchestration fetch' "$workflow" | grep -Fq 'FLEET_FLOTILLA_SHA'; then
  echo 'orchestration checkout must not use the selected Flotilla source SHA' >&2
  exit 1
fi
grep -Fq 'retention-days: 7' "$workflow"
grep -Fq 'actions/cache/restore@6f8efc29b200d32929f49075959781ed54ec270c' "$workflow"
grep -Fq 'actions/cache/save@6f8efc29b200d32929f49075959781ed54ec270c' "$workflow"
test "$(grep -Fc 'actions/upload-artifact@a8a3f3ad30e3422c9c7b888a15615d19a852ae32' "$workflow")" -eq 2

# shellcheck disable=SC1090,SC1091
source "$builder"
valid_sha=0123456789abcdef0123456789abcdef01234567
test "$(require_sha TEST_SHA "$valid_sha")" = "$valid_sha"
if require_sha TEST_SHA main >/dev/null 2>&1; then
  echo 'accepted a floating source ref' >&2
  exit 1
fi

fake_flotilla="$(mktemp "${TMPDIR:-/tmp}/fake-flotilla.XXXXXX")"
printf '#!/bin/sh\nprintf "flotilla 0.1.0 (wire=test, proto=20)\\n"\n' >"$fake_flotilla"
chmod 0755 "$fake_flotilla"
test "$(read_protocol_version "$fake_flotilla")" = 20
printf '#!/bin/sh\nprintf "flotilla 0.1.0 (wire=test)\\n"\n' >"$fake_flotilla"
if read_protocol_version "$fake_flotilla" >/dev/null 2>&1; then
  echo 'accepted a binary that did not report its protocol version' >&2
  exit 1
fi
rm -f "$fake_flotilla"

test_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-candidate-test.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT
bundle="$test_root/bundle"
prefix="$test_root/prefix"
mkdir -p "$bundle/bin" "$bundle/lib"
printf '#!/bin/sh\nexit 0\n' >"$bundle/bin/cleat"
printf 'test library\n' >"$bundle/lib/libghostty-vt.dylib"
cp "$installer" "$bundle/install.sh"
chmod 0755 "$bundle/bin/cleat" "$bundle/install.sh"
export TEST_BUNDLE="$bundle"
python3 - <<'PY'
import hashlib
import json
import os
import platform
from pathlib import Path

bundle = Path(os.environ["TEST_BUNDLE"])
system = platform.system()
machine = platform.machine()
if (system, machine) == ("Darwin", "arm64"):
    target = "darwin-aarch64"
elif (system, machine) == ("Linux", "x86_64"):
    target = "linux-x86_64-gnu2.36"
else:
    raise SystemExit(f"unsupported test platform: {system}-{machine}")
files = []
for path in sorted(bundle.rglob("*")):
    if path.is_file():
        files.append({
            "path": str(path.relative_to(bundle)),
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "size_bytes": path.stat().st_size,
        })
(bundle / "manifest.json").write_text(json.dumps({"platform": target, "peer_protocol_version": 20, "files": files}))
PY
"$bundle/install.sh" "$prefix" >/dev/null
cmp "$bundle/bin/cleat" "$prefix/bin/cleat"
cmp "$bundle/lib/libghostty-vt.dylib" "$prefix/lib/libghostty-vt.dylib"

echo 'fleet candidate workflow contract passed'
