#!/usr/bin/env bash
# Launch the daily-driver zellij session: andamento rail + git-watcher +
# the flotilla manifest connector (layouts/daily-driver.kdl, #708).
#
# Builds the andamento plugins, checks the flotilla dev binaries exist and
# are properly signed (macOS 26.5.x syspolicyd mitigation, #689 — signing
# itself stays a manual step), then execs zellij with the composed layout.
set -euo pipefail

FLOTILLA_ROOT=${FLOTILLA_ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
export ANDAMENTO_ROOT=${ANDAMENTO_ROOT:-"$HOME/dev/andamento"}
export FLOTILLA_BIN=${FLOTILLA_BIN:-"$FLOTILLA_ROOT/target/debug/flotilla"}
ZELLIJ_BIN=${ZELLIJ_BIN:-"$HOME/dev/zellij/target/dev-opt/zellij"}

if [[ ! -x "$FLOTILLA_BIN" ]]; then
    echo "error: flotilla binary not found at $FLOTILLA_BIN — run cargo build first" >&2
    exit 1
fi

# Warn (don't block) when the dev binaries look ad-hoc signed: per #689 the
# per-exec assessment cost on macOS 26.5.x wants a real identity. Signing is
# deliberately manual; see the dev-signing notes.
for bin in "$FLOTILLA_BIN" "$(dirname "$FLOTILLA_BIN")/flotillad"; do
    if [[ -x "$bin" ]] && ! codesign -dv "$bin" 2>&1 | grep -q "Authority="; then
        echo "warning: $bin appears ad-hoc signed — re-sign it (see #689 dev-signing notes)" >&2
    fi
done

cargo build --manifest-path "$ANDAMENTO_ROOT/Cargo.toml" --workspace --target wasm32-wasip1 --release

exec "$ZELLIJ_BIN" --layout "$FLOTILLA_ROOT/layouts/daily-driver.kdl"
