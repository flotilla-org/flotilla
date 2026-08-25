#!/usr/bin/env bash
set -euo pipefail

require_sha() {
  local name="$1"
  local value="$2"
  if [[ ${#value} -ne 40 || "$value" == *[!0-9a-fA-F]* ]]; then
    printf '%s must be an exact 40-character hexadecimal commit: %s\n' "$name" "$value" >&2
    return 1
  fi
  printf '%s' "$value" | tr 'A-F' 'a-f'
}

fetch_exact() {
  local repository="$1"
  local commit="$2"
  local destination="$3"

  mkdir -p "$destination"
  if [[ ! -d "$destination/.git" ]]; then
    git -C "$destination" init
    git -C "$destination" remote add origin "$repository"
  else
    git -C "$destination" remote set-url origin "$repository"
  fi
  git -C "$destination" fetch --depth=1 origin "$commit"
  test "$(git -C "$destination" rev-parse FETCH_HEAD)" = "$commit"
  git -C "$destination" checkout --detach --force FETCH_HEAD
  git -C "$destination" clean -ffdx -e target/ -e .tools/
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

read_protocol_version() {
  local binary="$1"
  local version
  version="$("$binary" --version)"
  if [[ "$version" =~ proto=([0-9]+) ]]; then
    printf '%s' "${BASH_REMATCH[1]}"
    return
  fi
  printf '%s --version did not report a peer protocol version: %s\n' "$binary" "$version" >&2
  return 1
}

main() {
flotilla_sha="$(require_sha FLEET_FLOTILLA_SHA "${FLEET_FLOTILLA_SHA:-}")"
cleat_sha="$(require_sha FLEET_CLEAT_SHA "${FLEET_CLEAT_SHA:-}")"
mattpocock_skills_sha="$(require_sha FLEET_MATTPOCOCK_SKILLS_SHA "${FLEET_MATTPOCOCK_SKILLS_SHA:-}")"
rjw_skills_sha="$(require_sha FLEET_RJW_SKILLS_SHA "${FLEET_RJW_SKILLS_SHA:-}")"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    platform="linux-x86_64-gnu2.36"
    ghostty_library="libghostty-vt.so"
    ;;
  Darwin-arm64)
    platform="darwin-aarch64"
    ghostty_library="libghostty-vt.dylib"
    ;;
  *)
    printf 'unsupported fleet candidate platform: %s-%s\n' "$(uname -s)" "$(uname -m)" >&2
    exit 1
    ;;
esac

test "$(cargo --version)" = "cargo 1.97.1 (c980f4866 2026-06-30)"
test "$(zig version)" = "0.15.2"
if [[ "$platform" == darwin-aarch64 ]]; then
  xcodebuild -version | grep -Fx 'Xcode 26.6'
  security find-identity -v -p codesigning | grep -Eq '^[[:space:]]*0 valid identities found[[:space:]]*$'
fi

source_root="$PWD/sources"
flotilla_root="$source_root/flotilla"
cleat_root="$source_root/cleat"
output_root="$PWD/dist"
bundle_name="fleet-candidate-$platform"
bundle="$output_root/$bundle_name"
if [[ "$PWD" == / || "$output_root" != "$PWD/dist" ]]; then
  echo 'refusing an unsafe output directory' >&2
  exit 1
fi

fetch_exact https://github.com/flotilla-org/flotilla.git "$flotilla_sha" "$flotilla_root"
fetch_exact https://github.com/flotilla-org/cleat.git "$cleat_sha" "$cleat_root"

(
  cd "$flotilla_root"
  export FLOTILLA_BUILD_ID="${flotilla_sha:0:12}"
  cargo build --locked --release --bin flotilla --bin flotillad
)

(
  cd "$cleat_root"
  ./tools/prepare-ghostty-vt.sh
  cargo build -p cleat --locked --features ghostty-vt --release
)

rm -rf "$output_root"
mkdir -p "$bundle/bin" "$bundle/lib"
install -m 0755 "$flotilla_root/target/release/flotilla" "$bundle/bin/flotilla"
install -m 0755 "$flotilla_root/target/release/flotillad" "$bundle/bin/flotillad"
install -m 0755 "$cleat_root/target/release/cleat" "$bundle/bin/cleat"

skills_bundle="$bundle/share/flotilla/skills"
mkdir -p "$skills_bundle"
FLEET_SKILLS_BUNDLE="$skills_bundle" \
  FLEET_FLOTILLA_SHA="$flotilla_sha" \
  FLEET_CLEAT_SHA="$cleat_sha" \
  FLEET_MATTPOCOCK_SKILLS_SHA="$mattpocock_skills_sha" \
  FLEET_RJW_SKILLS_SHA="$rjw_skills_sha" \
  python3 - <<'PY'
import json
import os
from pathlib import Path

bundle = Path(os.environ["FLEET_SKILLS_BUNDLE"])
# Supply side only: the manifest pins sources. Per-crew skill requirements are
# demand declarations (flotilla-org/flotilla#1790), never a list baked into a
# generation.
manifest = {
    "schema_version": 4,
    "sources": [
        {
            "name": "mattpocock-skills",
            "repository": "https://github.com/flotilla-org/mattpocock-skills.git",
            "revision": os.environ["FLEET_MATTPOCOCK_SKILLS_SHA"],
        },
        {
            "name": "rjw-skills",
            "repository": "https://github.com/rjwittams/rjw-skills.git",
            "revision": os.environ["FLEET_RJW_SKILLS_SHA"],
            "paths": ["plugins/rjw-sdlc/skills"],
        },
        {
            "name": "cleat",
            "repository": "https://github.com/flotilla-org/cleat.git",
            "revision": os.environ["FLEET_CLEAT_SHA"],
        },
        {
            "name": "flotilla",
            "repository": "https://github.com/flotilla-org/flotilla.git",
            "revision": os.environ["FLEET_FLOTILLA_SHA"],
        },
    ],
}
(bundle / ".flotilla-sources.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY

if [[ "$platform" == darwin-aarch64 && -f "$cleat_root/.tools/ghostty-install/lib/$ghostty_library" ]]; then
  install -m 0755 "$cleat_root/.tools/ghostty-install/lib/$ghostty_library" "$bundle/lib/$ghostty_library"
fi

if [[ "$platform" == linux-x86_64-gnu2.36 ]]; then
  dependency="$(patchelf --print-needed "$bundle/bin/cleat" | awk '/^libghostty-vt\.so/{print; exit}')"
  if [[ -n "$dependency" ]]; then
    ghostty_source="$cleat_root/.tools/ghostty-install/lib/$dependency"
    if [[ ! -f "$ghostty_source" ]]; then
      ghostty_source="$cleat_root/.tools/ghostty-install/lib/$ghostty_library"
    fi
    test -f "$ghostty_source"
    install -m 0755 "$ghostty_source" "$bundle/lib/$dependency"
    # The literal ELF loader token must reach patchelf unchanged.
    # shellcheck disable=SC2016
    patchelf --set-rpath '$ORIGIN/../lib' "$bundle/bin/cleat"
    # shellcheck disable=SC2016
    readelf -d "$bundle/bin/cleat" | grep -Fq '$ORIGIN/../lib'
    ldd "$bundle/bin/cleat" | grep -F "$dependency" | grep -Fvq 'not found'
  fi
else
  dependency="$(otool -L "$bundle/bin/cleat" | awk '/libghostty-vt\.dylib/{print $1; exit}')"
  if [[ -n "$dependency" ]]; then
    test -f "$bundle/lib/$ghostty_library"
    install_name_tool -change "$dependency" "@rpath/$ghostty_library" "$bundle/bin/cleat"
    install_name_tool -add_rpath '@executable_path/../lib' "$bundle/bin/cleat"
    codesign --force --sign - "$bundle/bin/cleat"
    otool -L "$bundle/bin/cleat" | grep -Fq "@rpath/$ghostty_library"
    codesign --verify --strict "$bundle/bin/cleat"
  else
    rm -f "$bundle/lib/$ghostty_library"
  fi
fi

cp "$flotilla_root/scripts/fleet-install" "$bundle/install.sh"
cp "$flotilla_root/ci/fleet-candidates/generation_validation.py" "$bundle/generation_validation.py"
chmod 0755 "$bundle/install.sh" "$bundle/generation_validation.py"

wire_generation="${flotilla_sha:0:12}"
"$bundle/bin/flotilla" --version | grep -F "wire=$wire_generation"
"$bundle/bin/flotillad" --version | grep -F "wire=$wire_generation"
protocol_version="$(read_protocol_version "$bundle/bin/flotilla")"
test "$(read_protocol_version "$bundle/bin/flotillad")" = "$protocol_version"
"$bundle/bin/cleat" launch --help | grep -q -- --tag

export FLEET_BUNDLE="$bundle"
export FLEET_PLATFORM="$platform"
export FLEET_FLOTILLA_SHA="$flotilla_sha"
export FLEET_CLEAT_SHA="$cleat_sha"
export FLEET_MATTPOCOCK_SKILLS_SHA="$mattpocock_skills_sha"
export FLEET_RJW_SKILLS_SHA="$rjw_skills_sha"
export FLEET_PROTOCOL_VERSION="$protocol_version"
python3 - <<'PY'
import hashlib
import json
import os
from pathlib import Path

bundle = Path(os.environ["FLEET_BUNDLE"])
files = []
for path in sorted(bundle.rglob("*")):
    if not path.is_file() or path.name == "manifest.json":
        continue
    files.append(
        {
            "path": str(path.relative_to(bundle)),
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "size_bytes": path.stat().st_size,
        }
    )

manifest = {
    "schema_version": 1,
    "kind": "unsigned-fleet-candidate",
    "platform": os.environ["FLEET_PLATFORM"],
    "sources": {
        "flotilla": os.environ["FLEET_FLOTILLA_SHA"],
        "cleat": os.environ["FLEET_CLEAT_SHA"],
        "mattpocock-skills": os.environ["FLEET_MATTPOCOCK_SKILLS_SHA"],
        "rjw-skills": os.environ["FLEET_RJW_SKILLS_SHA"],
    },
    "build_profile": "release",
    "peer_protocol_version": int(os.environ["FLEET_PROTOCOL_VERSION"]),
    "signed": False,
    "files": files,
}
(bundle / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY

archive="$output_root/$bundle_name.tar.gz"
tar -C "$output_root" -czf "$archive" "$bundle_name"
archive_sha="$(sha256_file "$archive")"
printf '%s  %s\n' "$archive_sha" "$(basename "$archive")" >"$archive.sha256"

export FLEET_ARCHIVE="$archive"
export FLEET_ARCHIVE_SHA="$archive_sha"
python3 - <<'PY'
import json
import os
from pathlib import Path

archive = Path(os.environ["FLEET_ARCHIVE"])
metadata = {
    "schema_version": 1,
    "kind": "unsigned-fleet-candidate-archive",
    "platform": os.environ["FLEET_PLATFORM"],
    "sources": {
        "flotilla": os.environ["FLEET_FLOTILLA_SHA"],
        "cleat": os.environ["FLEET_CLEAT_SHA"],
        "mattpocock-skills": os.environ["FLEET_MATTPOCOCK_SKILLS_SHA"],
        "rjw-skills": os.environ["FLEET_RJW_SKILLS_SHA"],
    },
    "artifact": archive.name,
    "sha256": os.environ["FLEET_ARCHIVE_SHA"],
    "size_bytes": archive.stat().st_size,
    "signed": False,
}
archive.with_suffix(archive.suffix + ".json").write_text(
    json.dumps(metadata, indent=2, sort_keys=True) + "\n"
)
PY

rm -rf "$bundle"
printf 'built %s (%s)\n' "$archive" "$archive_sha"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  main "$@"
fi
