#!/usr/bin/env bash
# Operator acceptance on a Docker host: pass a freshly built crew image tag.
set -euo pipefail
image="${1:?usage: accept-stable-fmt.sh IMAGE}"
docker run --rm --network=none --pull=never --user 12345:12345 \
    --entrypoint sh "$image" -ec '
cd /opt/flotilla
. ./pin.sh
pin=$(read_rust_pin rust-toolchain.toml)
test "$(rustc --version | cut -d " " -f2)" = "$pin"
if rustup toolchain list | grep -q nightly; then
    echo "crew image must not contain a nightly toolchain" >&2
    exit 1
fi
probe=$(mktemp -d)
trap '\''rm -rf "$probe"'\'' EXIT
printf "fn main() {}\n" > "$probe/main.rs"
rustfmt --check "$probe/main.rs"
cargo fmt --version
'
