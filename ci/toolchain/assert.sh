#!/usr/bin/env sh
set -eu
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
. "$repo_root/ci/toolchain/pin.sh"
assert_rust_pin "$repo_root/rust-toolchain.toml"
