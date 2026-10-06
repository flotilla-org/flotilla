#!/usr/bin/env sh
set -eu
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"
. ci/toolchain/pin.sh
assert_rust_pin rust-toolchain.toml
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Relay Workers must compile with the target stdlib on the pinned compiler,
# rather than on rustup's separately installed floating stable toolchain.
printf 'pub fn probe() {}\n' | rustc --crate-type lib --target wasm32-unknown-unknown --emit=metadata -o "$work/probe.rmeta" -
test -s "$work/probe.rmeta"
echo 'Pinned WebAssembly target contract passed'
