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
if [ -n "${QUINT_RUN_DIR:-}" ]; then
  mkdir -p "$QUINT_RUN_DIR"
  run=$(cd "$QUINT_RUN_DIR" && pwd)
else
  run=$(mktemp -d "$PWD/build/fast.XXXXXX")
fi
for scenario in agent_activity convoy_deliver ensure_bind inbox_delivery message_race controller_delivery machine_edges admission_placement reply_exchange; do
  # Assignment coverage is mandatory even for branches unreachable at depth one.
  # Simulation/typecheck alone can silently lose an unassigned world variable.
  quint verify "scenarios/$scenario.qnt" --invariant safety --max-steps 1 > "$run/$scenario.frames.log" 2>&1
  quint compile "scenarios/$scenario.qnt" --target tlaplus --invariant safety > "$run/$scenario.compile.log"
  awk '/^-+ MODULE/{found=1} found' "$run/$scenario.compile.log" > "$run/$scenario.tla"
  printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\nVIEW view\n' > "$run/$scenario.cfg"
  java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock \
    -workers "${TLC_WORKERS:-4}" -metadir "$run/$scenario.states" \
    -config "$run/$scenario.cfg" "$run/$scenario.tla" > "$run/$scenario.tlc.log" 2>&1
  grep -q 'Model checking completed. No error has been found' "$run/$scenario.tlc.log"
  # Activity has full-graph conformance; the shared replay runner handles the existing compositions.
  if [ "$scenario" != agent_activity ]; then
  quint run "scenarios/$scenario.qnt" --backend typescript --mbt --invariant safety --seed 3220 \
    --max-samples 128 --n-traces 128 --max-steps 80 --out-itf "$run/$scenario-{seq}.itf.json" > "$run/$scenario.replay.log"
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
