#!/usr/bin/env bash
# Runs inside the allocated formal-modelling host. A budget exhaustion is not a proof.
set -euo pipefail
# Expanded typed compositions exceed the JVM's default parser stack.
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
cd "$(dirname "$0")"
mkdir -p build/deep
: > build/deep/results.txt
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
status=0
for scenario in convoy_deliver ensure_bind inbox_delivery message_race admission_placement; do
  sed 's/pure val maxRows = [0-9]*/pure val maxRows = 6/' "scenarios/$scenario.qnt" > "build/deep_$scenario.qnt"
  if [[ "$scenario" = inbox_delivery || "$scenario" = message_race ]]; then
    grep -q '^  pure val maxRows = 6$' "build/deep_$scenario.qnt" || {
      echo "$scenario: failed to set maxRows = 6" >&2; exit 2;
    }
  fi
  quint run "build/deep_$scenario.qnt" --main "$scenario" --backend typescript --mbt --invariant safety --seed 3220 \
    --max-samples 16 --n-traces 16 --max-steps 120 --out-itf "build/deep/$scenario-{seq}.itf.json" > "build/deep/$scenario.simulation.log"
  quint compile "build/deep_$scenario.qnt" --main "$scenario" --target tlaplus --invariant safety --temporal liveness > "build/deep/$scenario.compile.log"
  awk '/^-+ MODULE/{found=1} found' "build/deep/$scenario.compile.log" > "build/deep/$scenario.tla"
  printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\nPROPERTY q_temporalProps\nVIEW view\n' > "build/deep/$scenario.cfg"
  result=0
  timeout --kill-after=5s "${PER_SCENARIO_SECONDS:-60}s" java "-Xmx${TLC_HEAP:-8g}" -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock \
    -workers "$(getconf _NPROCESSORS_ONLN)" -metadir "build/deep/$scenario.states" \
    -config "build/deep/$scenario.cfg" "build/deep/$scenario.tla" > "build/deep/$scenario.tlc.log" 2>&1 || result=$?
  if [ "$result" = 124 ]; then
    echo "$scenario: budget exhausted (not a pass)" | tee -a build/deep/results.txt
    if [ "$status" = 0 ]; then status=124; fi
  elif [ "$result" != 0 ] || ! grep -q 'Model checking completed. No error has been found' "build/deep/$scenario.tlc.log"; then
      echo "$scenario: failed; see log" | tee -a build/deep/results.txt
    status=1
  else
    echo "$scenario: safety and liveness hold" | tee -a build/deep/results.txt
  fi
done
quint run scenarios/machine_edges.qnt --backend typescript --mbt --invariant safety --seed 3220 \
  --max-samples 16 --n-traces 16 --max-steps 120 --out-itf "build/deep/machine_edges-{seq}.itf.json" > build/deep/machine_edges.simulation.log
QUINT_TRACE_DIR="$PWD/build/deep" cargo test --manifest-path ../../prototypes/3108-3209/Cargo.toml --locked --test quint_replay -- --ignored
symbolic=0
timeout --kill-after=5s "${SYMBOLIC_SECONDS:-180}s" ./symbolic.sh || symbolic=$?
if [ "$symbolic" != 0 ]; then
  echo "symbolic: unfinished or failed (exit $symbolic), see logs" | tee -a build/deep/results.txt
  if [ "$status" = 0 ]; then status=$symbolic; fi
fi
exit "$status"
