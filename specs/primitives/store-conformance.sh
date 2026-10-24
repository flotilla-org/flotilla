#!/usr/bin/env bash
# Store-backed half of the fast tier: the full-graph conformance and trace replay through the
# decided SQLite store, over the graphs and traces a check.sh run wrote.
# Usage: store-conformance.sh RUN_DIRECTORY (check.sh's fast-tier artifacts: traces, graphs/)
# check.sh runs this last; CI runs it as its own step so each half has its own time budget (#3778).
set -euo pipefail
run=$(cd "${1:?usage: store-conformance.sh RUN_DIRECTORY}" && pwd)
[ -d "$run/graphs" ] || { echo "no graphs in $run; run specs/primitives/check.sh first" >&2; exit 2; }
cd "$(dirname "$0")"
# Every probe replays its path into a fresh SQLite store, so these suites spend most of their
# time inside SQLite; an unoptimised bundled SQLite makes them about a third slower. Only
# libsqlite3-sys and its dependents build differently, and every suite here shares the setting.
cargo_test=(cargo test --config 'profile.dev.package.libsqlite3-sys.opt-level=3' --locked)
# The provisioning lifecycle kinds (#3629) run the lifecycle graph through the decided store.
QUINT_GRAPH_DIR="$run/graphs" "${cargo_test[@]}" -p flotilla-provisioning \
  --test lifecycle_conformance -- --ignored --nocapture 2>&1 | tee "$run/provisioning.log"
QUINT_TRACE_DIR="$run" "${cargo_test[@]}" -p flotilla-schema-sqlite \
  --test quint_replay -- --ignored --nocapture 2>&1 | tee "$run/follower.log"
# The crew kinds (#3629): the placement composition and Ensure through the decided store.
QUINT_GRAPH_DIR="$run/graphs" "${cargo_test[@]}" -p flotilla-engine-store \
  --test crew_conformance -- --ignored --nocapture 2>&1 | tee "$run/crew.log"
# Backends share the trace mapping but replay only the Message race; full-graph
# conformance remains with the engine in check.sh.
QUINT_TRACE_DIR="$run" "${cargo_test[@]}" -p flotilla-inbox \
  --test contract message_race_traces -- --ignored --nocapture 2>&1 | tee "$run/inbox.log"
printf 'Store-backed conformance artifacts: %s\n' "$run"
