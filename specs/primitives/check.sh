#!/usr/bin/env bash
# Fast safety, full-graph conformance and generated ITF replay against
# flotilla-state-machines, plus Message race replay through InboxBackend.
# Generated files stay beneath build/.
set -euo pipefail
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
run=$(mktemp -d "$PWD/build/fast.XXXXXX")
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
# The provisioning lifecycle kinds (#3629) run the lifecycle graph through the decided store.
QUINT_GRAPH_DIR="$run/graphs" cargo test -p flotilla-provisioning --locked \
  --test lifecycle_conformance -- --ignored --nocapture 2>&1 | tee "$run/provisioning.log"
QUINT_TRACE_DIR="$run" cargo test -p flotilla-schema-sqlite --locked \
  --test quint_replay -- --ignored --nocapture 2>&1 | tee "$run/follower.log"
# Backends share the trace mapping but replay only the Message race; full-graph
# conformance remains with the engine above.
QUINT_TRACE_DIR="$run" cargo test -p flotilla-inbox --locked \
  --test contract message_race_traces -- --ignored --nocapture 2>&1 | tee "$run/inbox.log"
./mutants/check.sh

python3 ./runner-contract-test.py
printf 'Fast-tier artifacts: %s\n' "$run"
