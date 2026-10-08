#!/usr/bin/env bash
# Live acceptance of the Rust adapter and first-party daemon identity contract.
# Requires a full Codex CLI package, logged-in CODEX_HOME, cleat, and uv.
set -euo pipefail
cd "$(dirname "$0")/.."
transport_home=${CODEX_HOME:?Set CODEX_HOME to a logged-in Codex home}
transport_codex=${CODEX_BIN:-$(command -v codex)}
transport_root=$(mktemp -d)
chmod 700 "$transport_root"
mkdir "$transport_root/source"
cp "$transport_home/auth.json" "$transport_root/source/auth.json"
chmod 600 "$transport_root/source/auth.json"
cleanup() {
    CODEX_HOME="$transport_root/source/flotilla-vessels/operator-probe" "$transport_codex" app-server daemon stop >/dev/null 2>&1 || true
    python3 -c 'import shutil,sys; shutil.rmtree(sys.argv[1])' "$transport_root"
}
trap cleanup EXIT
CODEX_HOME="$transport_root/source" CODEX_BIN="$transport_codex" cargo run -p flotilla-core --locked --example codex_managed_transport_probe -- "$transport_root"
CODEX_HOME="$transport_home" CODEX_BIN="$transport_codex" uv run --with websockets python scripts/prove-codex-vessel-identity.py
