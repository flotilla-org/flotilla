#!/usr/bin/env bash
# Live first-party acceptance. Run in a provisioned Linux vessel with the
# candidate flotilla binary, Codex >= 0.160 and a logged-in CODEX_HOME.
set -euo pipefail
cd "$(dirname "$0")/.."
transport_binary=${FLOTILLA_BIN:-"$PWD/target/debug/flotilla"}
transport_codex=${CODEX_BIN:-$(command -v codex)}
transport_home=${CODEX_HOME:?Set CODEX_HOME to a logged-in Codex home}
transport_model=${CODEX_TRANSPORT_MODEL:-gpt-6.1-sol}
transport_root=$(mktemp -d /tmp/flotilla-codex-accept.XXXXXX)
chmod 700 "$transport_root"
cp "$transport_home/auth.json" "$transport_root/auth.json"
chmod 600 "$transport_root/auth.json"
transport_socket="$transport_root/control.sock"
transport_record_dir=${1:-"$transport_root/recordings"}
mkdir -p "$transport_record_dir"
cleanup() {
    "$transport_binary" codex-app-server-stop --socket "$transport_socket" || true
    wait "$transport_server" || true
    rm -rf "$transport_root"
}
CODEX_HOME="$transport_root" "$transport_binary" codex-app-server --binary "$transport_codex" --socket "$transport_socket" >"$transport_root/server.log" 2>&1 &
transport_server=$!
trap cleanup EXIT
for _ in $(seq 1 100); do
    test -S "$transport_socket" && break
    kill -0 "$transport_server" || { cat "$transport_root/server.log"; exit 1; }
    sleep 0.1
done
for transport_mode in delivery steering approval; do
    cargo run -p flotilla-core --locked --example codex_transport_probe -- "$transport_socket" "$transport_record_dir/codex_0_160_${transport_mode}.json" "$transport_model" "$transport_mode"
done
"$transport_binary" codex-app-server-stop --socket "$transport_socket"
test ! -e "$transport_socket.supervisor-pid"
printf '%s\n' 'PASS: supervised startup, native delivery, urgent steering, approvals, and graceful orphan cleanup'
# A process-boundary fixture proves that a crashing app-server's detached tool
# becomes the supervisor's child and is reaped even after starting a new session.
cat > "$transport_root/crashing-codex" <<'EOF'
#!/usr/bin/env bash
set -eu
setsid sleep 300 &
printf '%s\n' "$!" > "$FLOTILLA_ORPHAN_PROBE_PID_FILE"
sleep 0.2
exit 7
EOF
chmod 700 "$transport_root/crashing-codex"
if FLOTILLA_ORPHAN_PROBE_PID_FILE="$transport_root/orphan-pid" "$transport_binary" codex-app-server --binary "$transport_root/crashing-codex" --socket "$transport_root/crash.sock" >"$transport_root/crash.log" 2>&1; then
    printf '%s\n' 'FAIL: crashing app-server unexpectedly succeeded' >&2
    exit 1
fi
transport_orphan=$(cat "$transport_root/orphan-pid")
if kill -0 "$transport_orphan" 2>/dev/null; then
    printf '%s\n' 'FAIL: detached tool survived app-server crash' >&2
    exit 1
fi
printf '%s\n' 'PASS: app-server crash reaps detached tool commands'
