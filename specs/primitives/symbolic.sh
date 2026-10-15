#!/usr/bin/env bash
set -euo pipefail
# Expanded typed compositions exceed the JVM's default parser stack.
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
cd "$(dirname "$0")"
mkdir -p build
# Induction: initial inclusion and one-step preservation, hence every depth.
quint verify scenarios/convoy_deliver.qnt --inductive-invariant inductive > build/convoy.induction.log 2>&1
# Bounded SMT checks at a larger stream bound than the fast TLC tier.
# Admission/placement has no expanded stream bound: the fast tier exhausts its
# complete finite state graph, which covers every depth (including this bound).
for scenario in inbox_delivery message_race; do
  sed 's/pure val maxRows = [0-9]*/pure val maxRows = 6/' "scenarios/$scenario.qnt" > "build/symbolic_$scenario.qnt"
  quint verify "build/symbolic_$scenario.qnt" --main "$scenario" --invariant safety \
    --max-steps "${SYMBOLIC_DEPTH:-10}" > "build/$scenario.symbolic.log" 2>&1
done
