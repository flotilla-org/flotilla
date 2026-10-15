#!/usr/bin/env bash
# Exports each conformance harness's full reachable graph: Quint compiles it to
# TLA+, TLC dumps every reachable state, and dump2json.py writes them as JSON.
# Usage: conformance/export.sh OUTPUT_DIRECTORY
set -euo pipefail
cd "$(dirname "$0")"
out=$1
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
[ -r "$jar" ] || { echo "missing readable Apalache jar: $jar; install Apalache or set APALACHE_JAR" >&2; exit 2; }
mkdir -p "$out"
for machine in ${MACHINES:-lifecycle agent_session convoy message ensure vessel}; do
  module="${machine}_conformance"
  quint compile "$machine.qnt" --target tlaplus > "$out/$machine.compile.log"
  awk '/^-+ MODULE/{found=1} found' "$out/$machine.compile.log" > "$out/$module.tla"
  printf 'INIT q_init\nNEXT q_step\n' > "$out/$module.cfg"
  java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock -workers "${TLC_WORKERS:-4}" \
    -metadir "$out/$machine.states" -dump "$out/$machine" \
    -config "$out/$module.cfg" "$out/$module.tla" > "$out/$machine.tlc.log" 2>&1
  grep -q 'Model checking completed. No error has been found' "$out/$machine.tlc.log"
  python3 dump2json.py "$out/$machine.dump" "$out/$machine.graph.json"
done
