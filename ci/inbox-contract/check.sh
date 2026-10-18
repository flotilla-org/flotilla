#!/usr/bin/env bash
# Replay the unchanged Message specs through both InboxBackend implementations.
# Requires the same Quint/Java installation as specs/primitives/check.sh.
set -euo pipefail
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
cd "$(dirname "$0")/../../specs/primitives"
for tool in quint java; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool; see specs/primitives/README.md" >&2; exit 2; }
done
mkdir -p build
run=$(mktemp -d "$PWD/build/inbox.XXXXXX")
# Like the existing CI bootstrap, this installs Quint's pinned Apalache jar.
quint compile conformance/message.qnt --target tlaplus > "$run/bootstrap.log" 2>&1
MACHINES=message ./conformance/export.sh "$run/graphs"
quint run scenarios/message_race.qnt --backend typescript --mbt --invariant safety --seed 3220 \
  --max-samples 128 --n-traces 128 --max-steps 80 --out-itf "$run/message_race-{seq}.itf.json" > "$run/race.log"
QUINT_TRACE_DIR="$run" QUINT_GRAPH_DIR="$run/graphs" cargo test -p flotilla-inbox --locked \
  --test contract -- --ignored --nocapture 2>&1 | tee "$run/inbox.log"
printf 'Inbox contract artifacts: %s\n' "$run"
