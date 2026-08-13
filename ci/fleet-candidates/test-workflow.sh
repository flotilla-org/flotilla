#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
workflow="$repo_root/.forgejo/workflows/fleet-candidates.yml"
builder="$repo_root/ci/fleet-candidates/build-candidate.sh"
setup="$repo_root/ci/fleet-candidates/setup-linux-toolchain.sh"
installer="$repo_root/ci/fleet-candidates/install-candidate.sh"

bash -n "$builder" "$setup" "$installer"

grep -Fq 'workflow_dispatch:' "$workflow"
if grep -Eq '^[[:space:]]+(push|pull_request):' "$workflow"; then
  echo 'fleet candidate workflow must be manual-only' >&2
  exit 1
fi
grep -Fq 'runs-on: debian-12' "$workflow"
grep -Fq 'runs-on: darwin-aarch64' "$workflow"
grep -Fq 'retention-days: 7' "$workflow"
grep -Fq 'actions/cache@6f8efc29b200d32929f49075959781ed54ec270c' "$workflow"
test "$(grep -Fc 'actions/upload-artifact@a8a3f3ad30e3422c9c7b888a15615d19a852ae32' "$workflow")" -eq 2

# shellcheck disable=SC1090,SC1091
source "$builder"
valid_sha=0123456789abcdef0123456789abcdef01234567
test "$(require_sha TEST_SHA "$valid_sha")" = "$valid_sha"
if require_sha TEST_SHA main >/dev/null 2>&1; then
  echo 'accepted a floating source ref' >&2
  exit 1
fi

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
(bundle / "manifest.json").write_text(json.dumps({"platform": target, "files": files}))
PY
"$bundle/install.sh" "$prefix" >/dev/null
cmp "$bundle/bin/cleat" "$prefix/bin/cleat"
cmp "$bundle/lib/libghostty-vt.dylib" "$prefix/lib/libghostty-vt.dylib"

echo 'fleet candidate workflow contract passed'
