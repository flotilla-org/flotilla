#!/usr/bin/env bash

# This file is sourced by the Linux workflow so its PATH and toolchain homes
# remain in effect for the build step.
set -euo pipefail

toolchain_root="$PWD/.fleet-cache/toolchains"
export CARGO_HOME="$PWD/.fleet-cache/cargo"
export RUSTUP_HOME="$toolchain_root/rustup"
rust_toolchain="1.97.1"
rustup_version="1.28.2"
rustup_sha256="20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c"
zig_version="0.15.2"
zig_sha256="02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239"
zig_root="$toolchain_root/zig-$zig_version"

mkdir -p "$CARGO_HOME" "$RUSTUP_HOME" "$toolchain_root"

if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
  rustup_installer="$toolchain_root/rustup-init"
  curl --proto '=https' --tlsv1.2 --fail --show-error --silent \
    "https://static.rust-lang.org/rustup/archive/$rustup_version/x86_64-unknown-linux-gnu/rustup-init" \
    -o "$rustup_installer"
  printf '%s  %s\n' "$rustup_sha256" "$rustup_installer" | sha256sum --check --status
  chmod 0755 "$rustup_installer"
  "$rustup_installer" -y --no-modify-path --profile minimal \
    --default-toolchain "$rust_toolchain"
fi

export PATH="$CARGO_HOME/bin:$zig_root:$PATH"
if ! rustup toolchain list | grep -Eq "^${rust_toolchain}(-x86_64-unknown-linux-gnu)?[[:space:]]"; then
  rustup toolchain install "$rust_toolchain" --profile minimal
fi
rustup default "$rust_toolchain"

if [[ ! -x "$zig_root/zig" ]]; then
  zig_archive="$toolchain_root/zig-$zig_version.tar.xz"
  curl --proto '=https' --tlsv1.2 --fail --show-error --silent \
    "https://ziglang.org/download/$zig_version/zig-x86_64-linux-$zig_version.tar.xz" \
    -o "$zig_archive"
  printf '%s  %s\n' "$zig_sha256" "$zig_archive" | sha256sum --check --status
  rm -rf "$zig_root"
  mkdir -p "$zig_root"
  tar -xJf "$zig_archive" --strip-components=1 -C "$zig_root"
fi

test "$(rustc --version)" = "rustc 1.97.1 (8bab26f4f 2026-07-14)"
test "$(zig version)" = "$zig_version"
