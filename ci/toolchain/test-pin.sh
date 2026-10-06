#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
source "$root/ci/toolchain/pin.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
# Stand-in at the rustc subprocess boundary; CI must inspect the running compiler.
cat > "$work/bin/rustc" <<'EOF'
#!/bin/sh
printf 'rustc %s (test 2026-10-01)\n' "$TEST_RUST_VERSION"
EOF
chmod +x "$work/bin/rustc"
export PATH="$work/bin:$PATH"
expected=$(read_rust_pin "$root/rust-toolchain.toml")
# Contract #2792: exact matching compiler passes; other stable and nightly fail.
export TEST_RUST_VERSION="$expected"
"$root/ci/toolchain/assert.sh"
for version in 0.0.0 "${expected}-nightly" ''; do
    export TEST_RUST_VERSION="$version"
    if "$root/ci/toolchain/assert.sh" >"$work/out" 2>&1; then
        echo "accepted mismatched compiler '$version'" >&2; exit 1
    fi
    grep -q 'Rust compiler mismatch' "$work/out"
done
# Contract #2792: only one exact stable channel is accepted (no floating pins).
for channel in stable nightly '' 1.99 1.99.0-nightly; do
    printf '[toolchain]\nchannel = "%s"\n' "$channel" > "$work/pin.toml"
    if read_rust_pin "$work/pin.toml" >/dev/null 2>&1; then
        echo "accepted invalid channel '$channel'" >&2; exit 1
    fi
done
printf '[toolchain]\nchannel = "1.2.3"\n[other]\nchannel = "stable"\n' > "$work/pin.toml"
test "$(read_rust_pin "$work/pin.toml")" = 1.2.3
printf '[toolchain]\nchannel = "1.2.3"\nchannel = "1.2.4"\n' > "$work/pin.toml"
if read_rust_pin "$work/pin.toml" >/dev/null 2>&1; then
    echo 'accepted duplicate pins' >&2; exit 1
fi
# A missing pin must fail closed rather than selecting rustup's default.
if read_rust_pin "$work/missing.toml" >/dev/null 2>&1; then
    echo 'accepted a missing pin file' >&2; exit 1
fi
echo 'Rust pin contract passed'
