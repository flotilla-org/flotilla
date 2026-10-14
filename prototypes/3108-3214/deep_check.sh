#!/bin/bash
# Bounded symbolic check with Apalache (via `quint verify`, default backend) at a larger bound than the
# exhaustive TLC runs: two messages AND two sessions with every failure mode on. Apalache proves the
# invariants for every run of up to --max-steps steps; it is a bound, not a proof for all lengths.
#   ./deep_check.sh [max-steps]     (default 8; about 5 minutes)
set -euo pipefail
cd "$(dirname "$0")"
STEPS=${1:-8}
python3 - <<'P'
import sys; sys.path.insert(0, ".")
import check, cases
spec = [s for s in cases.SPECS if s["name"] == "inbox"][0]
case = dict(name="deep", over=dict(NM=2, NS=2, HOST_MAY_DIE=True, ALLOW_FAIL=True, ALLOW_END=True), props={p: "hold" for p in spec["invariants"]})
check.compile_case(spec, case)
print(",".join(p for p in spec["invariants"] if not p.startswith("witness") and p != "droppedImpliesNeverTurned"), file=open("build/inbox_deep.invariants", "w"))
P
quint verify build/inbox_deep.qnt --main inbox_deep --max-steps "$STEPS" \
  --invariant "$(cat build/inbox_deep.invariants)"
