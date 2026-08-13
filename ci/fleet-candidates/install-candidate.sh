#!/usr/bin/env bash
set -euo pipefail

bundle_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${1:-${FLEET_INSTALL_PREFIX:-$HOME/.local}}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) expected_platform="linux-x86_64-gnu2.36" ;;
  Darwin-arm64) expected_platform="darwin-aarch64" ;;
  *)
    printf 'unsupported install platform: %s-%s\n' "$(uname -s)" "$(uname -m)" >&2
    exit 1
    ;;
esac

actual_platform="$(python3 - "$bundle_root" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

bundle = Path(sys.argv[1])
manifest = json.loads((bundle / "manifest.json").read_text())
expected_files = {entry["path"] for entry in manifest["files"]}
actual_files = {
    str(path.relative_to(bundle))
    for path in bundle.rglob("*")
    if path.is_file() and path.name != "manifest.json"
}
if actual_files != expected_files:
    raise SystemExit("candidate files do not match the manifest")
for entry in manifest["files"]:
    path = bundle / entry["path"]
    if not path.is_file():
        raise SystemExit(f"candidate file is missing: {entry['path']}")
    if path.stat().st_size != entry["size_bytes"]:
        raise SystemExit(f"candidate size mismatch: {entry['path']}")
    if hashlib.sha256(path.read_bytes()).hexdigest() != entry["sha256"]:
        raise SystemExit(f"candidate checksum mismatch: {entry['path']}")
print(manifest["platform"])
PY
)"
if [[ "$actual_platform" != "$expected_platform" ]]; then
  printf 'candidate is for %s, not %s\n' "$actual_platform" "$expected_platform" >&2
  exit 1
fi

mkdir -p "$prefix/bin" "$prefix/lib"
for source in "$bundle_root"/lib/*; do
  [[ -e "$source" ]] || continue
  name="$(basename "$source")"
  install -m 0755 "$source" "$prefix/lib/.$name.new"
done
# Deliberately no empty-glob guard: a candidate without binaries is invalid,
# so installation must fail rather than report success without installing them.
for source in "$bundle_root"/bin/*; do
  name="$(basename "$source")"
  install -m 0755 "$source" "$prefix/bin/.$name.new"
done
for source in "$bundle_root"/lib/*; do
  [[ -e "$source" ]] || continue
  name="$(basename "$source")"
  mv -f "$prefix/lib/.$name.new" "$prefix/lib/$name"
done
# As above, reaching this loop without staged binaries must fail closed.
for source in "$bundle_root"/bin/*; do
  name="$(basename "$source")"
  mv -f "$prefix/bin/.$name.new" "$prefix/bin/$name"
done

printf 'installed unsigned fleet candidate to %s\n' "$prefix"
printf 'restart any running flotillad and cleat processes before use\n'
