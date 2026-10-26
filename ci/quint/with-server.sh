#!/usr/bin/env bash
# Quint 0.33.0 compile and verify both support --server-endpoint. Keep one
# Apalache 0.62.1 process for this command tree, and reap it on every exit.
# CmdExecutorService creates a fresh ParserModule/CheckerModule per request.
set -euo pipefail
export JVM_ARGS="${JVM_ARGS:--Xmx3g -Xss16m}"
port=${QUINT_SERVER_PORT:-8823}
export QUINT_SERVER_ENDPOINT=localhost:$port
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/quint-server.XXXXXX")
log=$work/server.log
server=
cleanup() {
  if [ -n "$server" ]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -r -- "$work"
}
trap cleanup EXIT
jar=${APALACHE_JAR:-$HOME/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar}
if [ ! -r "$jar" ] && [ -z "${APALACHE_JAR:-}" ]; then
  # Bootstrap the pinned distribution without starting a throwaway JVM.
  curl --fail --location --retry 3 https://github.com/apalache-mc/apalache/releases/download/v0.62.1/apalache.tgz -o "$work/apalache.tgz"
  # SHA-256 of the v0.62.1 release archive (also used by Quint 0.33.0).
  (cd "$work"; printf '%s  apalache.tgz\n' 38202a8c4ebeb3ab73e0fe5a7e4bb948442d46ad5de8a7fb45c9e5cac2beb2a6 |
    if command -v sha256sum >/dev/null; then sha256sum -c -; else shasum -a 256 -c -; fi)
  tar -xzf "$work/apalache.tgz" -C "$work"
  [ -r "$work/apalache/lib/apalache.jar" ] || {
    echo 'unexpected Apalache 0.62.1 archive layout: expected apalache/lib/apalache.jar' >&2
    exit 2
  }
  mkdir -p "$HOME/.quint/apalache-dist-0.62.1"
  mv "$work/apalache" "$HOME/.quint/apalache-dist-0.62.1/"
fi
[ -r "$jar" ] || { echo "missing readable Apalache jar: $jar" >&2; exit 2; }
# Use the same JVM arguments as Quint's apalache-mc launcher.
# Intentional word splitting: JVM_ARGS is the launcher's space-separated list.
(cd "$work"; exec java $JVM_ARGS -jar "$jar" server --port="$port") > "$log" 2>&1 &
server=$!
for ((attempt=0; attempt<60; attempt++)); do
  if ! kill -0 "$server" 2>/dev/null; then
    cat "$log" >&2
    echo "Apalache failed to start at $QUINT_SERVER_ENDPOINT; for a local port collision, set QUINT_SERVER_PORT to an unused port" >&2
    exit 1
  fi
  # This readiness marker is pinned to 0.62.1; update it with the jar/version.
  if grep -q 'The Apalache server is running' "$log"; then
    "$@"
    exit
  fi
  sleep 1
done
cat "$log" >&2
echo 'Apalache server did not become ready within 60 seconds' >&2
exit 1
