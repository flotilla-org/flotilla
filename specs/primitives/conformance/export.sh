#!/usr/bin/env bash
# Exports each conformance harness's full reachable graph: Quint compiles it to
# TLA+, TLC dumps every reachable state, and dump2json.py writes them as JSON.
# Usage: conformance/export.sh OUTPUT_DIRECTORY
set -euo pipefail
server=()
if [ -n "${QUINT_SERVER_ENDPOINT:-}" ]; then
  server=(--server-endpoint "$QUINT_SERVER_ENDPOINT")
fi
cd "$(dirname "$0")"
out=$1
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
[ -r "$jar" ] || { echo "missing readable Apalache jar: $jar; install Apalache or set APALACHE_JAR" >&2; exit 2; }
mkdir -p "$out"
for machine in ${MACHINES:-lifecycle agent_session convoy message ensure vessel awaiting_reply}; do
  module="${machine}_conformance"
  invariant=()
  if [ "$machine" = convoy ]; then invariant=(--invariant quotaRefinesPlacement); fi
  quint compile ${server[@]+"${server[@]}"} "$machine.qnt" --target tlaplus ${invariant[@]+"${invariant[@]}"} > "$out/$machine.compile.log"
  awk '/^-+ MODULE/{found=1} found' "$out/$machine.compile.log" > "$out/$module.tla"
  printf 'INIT q_init\nNEXT q_step\n' > "$out/$module.cfg"
  if [ "$machine" = convoy ]; then printf 'INVARIANT q_inv\n' >> "$out/$module.cfg"; fi
  java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock -workers "${TLC_WORKERS:-4}" \
    -metadir "$out/$machine.states" -dump "$out/$machine" \
    -config "$out/$module.cfg" "$out/$module.tla" > "$out/$machine.tlc.log" 2>&1
  grep -q 'Model checking completed. No error has been found' "$out/$machine.tlc.log"
  python3 dump2json.py "$out/$machine.dump" "$out/$machine.graph.json"
done
