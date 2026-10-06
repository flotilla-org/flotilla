#!/usr/bin/env bash
# Scratch checkout must live outside the vessel checkout.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
source "$repo_root/ci/cleat-environment/revisions.sh"
source "$repo_root/ci/fleet-candidates/cleat-toolchain.sh"
source_dir=$(realpath "$1")
output_dir=$(realpath -m "$2")
mkdir -p "$output_dir"
cd "$source_dir"
test -z "$(git status --porcelain)"
for version in old new; do
  if [ "$version" = old ]; then revision=$CLEAT_OLD_REVISION; else revision=$CLEAT_NEW_REVISION; fi
  git checkout --detach "$revision"
  rust_version=$(read_cleat_toolchain_value rust-toolchain.toml toolchain channel)
  rustup toolchain install "$rust_version" --profile minimal
  bash "$repo_root/ci/fork-actions/runtime/cleat/prepare-ghostty-vt.sh"
  git restore tools/ghostty-toolchain.toml
  cargo +"$rust_version" build -p cleat --release --locked --features ghostty-vt
  mkdir -p "$output_dir/$version"
  install -m 0755 target/release/cleat "$output_dir/$version/cleat"
  if ldd "$output_dir/$version/cleat" | grep -q ghostty-vt; then
    echo 'contract binary must use static Ghostty VT' >&2
    exit 1
  fi
  {
    printf 'revision=%s\n' "$revision"
    rustc +"$rust_version" --version --verbose
    printf 'ghostty_revision=%s\n' "$(git -C .tools/ghostty-src rev-parse HEAD)"
    cat tools/ghostty-toolchain.toml
    "$output_dir/$version/cleat" version --json
    sha256sum "$output_dir/$version/cleat"
  } > "$output_dir/$version-provenance.txt"
done
