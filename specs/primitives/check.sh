#!/usr/bin/env bash
# Fast safety + generated ITF replay. Generated files stay beneath build/.
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
for scenario in convoy_deliver ensure_bind inbox_delivery message_race machine_edges admission_placement; do
  quint compile "scenarios/$scenario.qnt" --target tlaplus --invariant safety > "$run/$scenario.compile.log"
  awk '/^-+ MODULE/{found=1} found' "$run/$scenario.compile.log" > "$run/$scenario.tla"
  printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\nVIEW view\n' > "$run/$scenario.cfg"
  java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock \
    -workers "${TLC_WORKERS:-4}" -metadir "$run/$scenario.states" \
    -config "$run/$scenario.cfg" "$run/$scenario.tla" > "$run/$scenario.tlc.log" 2>&1
  grep -q 'Model checking completed. No error has been found' "$run/$scenario.tlc.log"
  quint run "scenarios/$scenario.qnt" --backend typescript --mbt --invariant safety --seed 3220 \
    --max-samples 128 --n-traces 128 --max-steps 80 --out-itf "$run/$scenario-{seq}.itf.json" > "$run/$scenario.replay.log"
done
cargo fmt --manifest-path ../../prototypes/3108-3209/Cargo.toml --check
cargo clippy --manifest-path ../../prototypes/3108-3209/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path ../../prototypes/3108-3209/Cargo.toml --locked
QUINT_TRACE_DIR="$run" cargo test --manifest-path ../../prototypes/3108-3209/Cargo.toml --locked --test quint_replay -- --ignored
./mutants/check.sh

./deep-contract-test.sh
python3 ./runner-contract-test.py
printf 'Fast-tier artifacts: %s\n' "$run"
