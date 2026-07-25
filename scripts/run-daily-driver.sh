#!/usr/bin/env bash
# Launch the daily-driver zellij session: andamento rail + git-watcher +
# the flotilla manifest connector (layouts/daily-driver.kdl, #708).
#
# Uses the native-plugins zellij (spike/native-plugins worktree) with
# andamento compiled in — the wasm plugin path has unacceptable perf for
# daily driving. Builds that zellij via its build-native-andamento script,
# checks the flotilla dev binaries exist and are properly signed (macOS
# 26.5.x syspolicyd mitigation, #689 — signing itself stays a manual step),
# then execs zellij with the composed layout.
set -euo pipefail

FLOTILLA_ROOT=${FLOTILLA_ROOT:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}
export ANDAMENTO_ROOT=${ANDAMENTO_ROOT:-"$HOME/dev/andamento"}
export FLOTILLA_BIN=${FLOTILLA_BIN:-"$FLOTILLA_ROOT/target/debug/flotilla"}
ZELLIJ_NATIVE_ROOT=${ZELLIJ_NATIVE_ROOT:-"$HOME/dev/zellij.native-plugins"}
ZELLIJ_BIN=${ZELLIJ_BIN:-"$ZELLIJ_NATIVE_ROOT/target/dev-opt/zellij"}

if [[ ! -x "$FLOTILLA_BIN" ]]; then
    echo "error: flotilla binary not found at $FLOTILLA_BIN — run cargo build first" >&2
    exit 1
fi

# Warn (don't block) when the dev binaries look ad-hoc signed: per #689 the
# per-exec assessment cost on macOS 26.5.x wants a real identity. Signing is
# deliberately manual; see the dev-signing notes.
if command -v codesign >/dev/null 2>&1; then
    for bin in "$FLOTILLA_BIN" "$(dirname "$FLOTILLA_BIN")/flotillad"; do
        if [[ -x "$bin" ]] && ! codesign -dv "$bin" 2>&1 | grep -q "Authority="; then
            echo "warning: $bin appears ad-hoc signed — re-sign it (see #689 dev-signing notes)" >&2
        fi
    done
fi

# Build the native-plugins zellij with andamento compiled in. The worktree's
# script handles the zellij-tile package-identity patching; skip it entirely
# when the caller points ZELLIJ_BIN somewhere else.
if [[ "$ZELLIJ_BIN" == "$ZELLIJ_NATIVE_ROOT/target/dev-opt/zellij" ]]; then
    if [[ ! -x "$ZELLIJ_NATIVE_ROOT/scripts/build-native-andamento" ]]; then
        echo "error: native-plugins zellij worktree not found at $ZELLIJ_NATIVE_ROOT" >&2
        echo "       (set ZELLIJ_NATIVE_ROOT, or set ZELLIJ_BIN to skip the built-in build)" >&2
        exit 1
    fi
    "$ZELLIJ_NATIVE_ROOT/scripts/build-native-andamento"
fi
echo "Trying to run $ZELLIJ_BIN" 
exec "$ZELLIJ_BIN" --layout "$FLOTILLA_ROOT/layouts/daily-driver.kdl"
