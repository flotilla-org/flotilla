#!/usr/bin/env bash
# Each isolated mutation must produce a safety counterexample, never a tool error.
set -euo pipefail
# Expanded typed compositions exceed the JVM's default parser stack.
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
cd "$(dirname "$0")/.."
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
mkdir -p build
run=$(mktemp -d "$PWD/build/mutants.XXXXXX")
for mutant in expiry_without_watermark release_only_realised duplicate_turn bind_without_drain promise_resettled quota_exceeded host_overbooked queue_bypassed lease_retained undeliverable_with_receipt; do
  case "$mutant" in
    expiry_without_watermark) scenario=message_race ;;
    release_only_realised) scenario=ensure_bind ;;
    duplicate_turn|bind_without_drain|undeliverable_with_receipt) scenario=inbox_delivery ;;
    promise_resettled) scenario=convoy_deliver ;;
    *) scenario=admission_placement ;;
  esac
  destination="$run/$mutant"
  mkdir -p "$destination"
  cp -R machines world scenarios "$destination/"
  patch -s -d "$destination" -p1 < "mutants/$mutant.patch"
  quint compile "$destination/scenarios/$scenario.qnt" --target tlaplus --invariant safety > "$destination/compile.log"
  awk '/^-+ MODULE/{found=1} found' "$destination/compile.log" > "$destination/$scenario.tla"
  printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\nVIEW view\n' > "$destination/$scenario.cfg"
  if java -Xmx2g -Xss512k -XX:+UseParallelGC -cp "$jar" tlc2.TLC -deadlock -workers "${TLC_WORKERS:-4}" \
      -metadir "$destination/states" -config "$destination/$scenario.cfg" "$destination/$scenario.tla" > "$destination/tlc.log" 2>&1; then
    echo "mutant survived: $mutant" >&2; exit 1
  fi
  grep -q 'Invariant.*violated' "$destination/tlc.log" || { cat "$destination/tlc.log"; exit 1; }
  echo "caught: $mutant"
done
printf 'Mutation evidence: %s\n' "$run"
