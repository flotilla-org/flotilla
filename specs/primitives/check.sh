#!/usr/bin/env bash
# Fast safety, full-graph conformance and generated ITF replay against
# flotilla-state-machines, then the store-backed suites (store-conformance.sh).
# Generated files stay beneath build/, or in $QUINT_RUN_DIR when it is set.
# Usage: check.sh [--skip-store]; --skip-store leaves the store-backed suites to
# store-conformance.sh, which CI runs as its own step (#3778).
set -euo pipefail
store=yes
case "${1:-}" in
  --skip-store) store=no ;;
  "") ;;
  *) echo "usage: check.sh [--skip-store]" >&2; exit 2 ;;
esac
# Expanded typed compositions exceed the JVM's default parser stack.
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
cd "$(dirname "$0")"
for tool in quint java; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool; see specs/primitives/README.md" >&2; exit 2; }
done
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
[ -r "$jar" ] || { echo "missing readable Apalache jar: $jar; install Apalache or set APALACHE_JAR" >&2; exit 2; }
mkdir -p build
quint test tests/stream_properties.qnt --backend typescript
quint test tests/lifecycle_endings.qnt --backend typescript
if [ -n "${QUINT_RUN_DIR:-}" ]; then
  # A fresh directory only: stale graphs or traces would stand in for a missing export.
  if [ -e "$QUINT_RUN_DIR" ] && [ -n "$(ls -A "$QUINT_RUN_DIR")" ]; then
    echo "QUINT_RUN_DIR $QUINT_RUN_DIR is not empty; name a new or empty directory" >&2; exit 2
  fi
  mkdir -p "$QUINT_RUN_DIR"
  run=$(cd "$QUINT_RUN_DIR" && pwd)
else
  run=$(mktemp -d "$PWD/build/fast.XXXXXX")
fi
# Keep subprocess diagnostics visible even when CI does not retain the run directory.
step_log() {
  local log=$1
  shift
  local status=0
  "$@" > "$log" 2>&1 || status=$?
  if [ "$status" -ne 0 ]; then
    printf 'Failed step: %s (exit %s)\n' "$log" "$status" >&2
    tail -n 80 "$log" >&2
    return "$status"
  fi
}
for scenario in agent_activity convoy_deliver ensure_bind inbox_delivery message_race controller_delivery machine_edges admission_placement reply_exchange; do
  # Assignment coverage is mandatory even for branches unreachable at depth one.
  # Simulation/typecheck alone can silently lose an unassigned world variable.
  step_log "$run/$scenario.frames.log" quint verify "scenarios/$scenario.qnt" --invariant safety --max-steps 1
  step_log "$run/$scenario.compile.log" quint compile "scenarios/$scenario.qnt" --target tlaplus --invariant safety
  awk '/^-+ MODULE/{found=1} found' "$run/$scenario.compile.log" > "$run/$scenario.tla"
  printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\nVIEW view\n' > "$run/$scenario.cfg"
  step_log "$run/$scenario.tlc.log" java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock \
    -workers "${TLC_WORKERS:-4}" -metadir "$run/$scenario.states" \
    -config "$run/$scenario.cfg" "$run/$scenario.tla"
  if ! grep -q 'Model checking completed. No error has been found' "$run/$scenario.tlc.log"; then
    tail -n 80 "$run/$scenario.tlc.log" >&2
    exit 1
  fi
  # Activity has full-graph conformance; the shared replay runner handles the existing compositions.
  if [ "$scenario" != agent_activity ]; then
  step_log "$run/$scenario.replay.log" quint run "scenarios/$scenario.qnt" --backend typescript --mbt --invariant safety --seed 3220 \
    --max-samples 128 --n-traces 128 --max-steps 80 --out-itf "$run/$scenario-{seq}.itf.json"
  fi
done
# Full-graph conformance: TLC dumps every reachable state of each machine's
# harness, and the engine must accept, reach and refuse exactly as the spec does.
python3 conformance/test_dump2json.py
./conformance/export.sh "$run/graphs"
# Workspace CI runs the crate's fmt, Clippy and unit tests; these two need Quint's output.
QUINT_TRACE_DIR="$run" QUINT_GRAPH_DIR="$run/graphs" cargo test -p flotilla-state-machines --locked \
  --test conformance --test quint_replay -- --ignored --nocapture 2>&1 | tee "$run/engine.log"
./mutants/check.sh

python3 ./runner-contract-test.py
printf 'Fast-tier artifacts: %s\n' "$run"
# The store-backed suites read the graphs and traces above.
if [ "$store" = yes ]; then
  ./store-conformance.sh "$run"
fi
