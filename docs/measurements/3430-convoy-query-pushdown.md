# Convoy reader filter pushdown (#3430)

## Workload and scope

The root WORKLOAD.md sizes one daemon at 1,000 running convoys (10,000 ceiling),
100,000 historical convoys (1,000,000 ceiling), and 21 KB mean Convoy bodies.
Legacy adoption previously decoded the federated inventory on each Convoy or
TerminalSession event batch. Ensure reconciliation read it per ensure; change
request demand discovery and credential attention also read the whole kind.
These reads cross the in-process store boundary; replicas are already stored
locally, so the query adds no network round trips.

At the ceiling, an unfiltered call can decode about 21 GB of historical bodies.
The indexed live query decodes only live bodies (about 210 MB at 10,000 live
convoys); an ensure query decodes only that ensure's live generations. The local
generation-history query deliberately retains terminal matches, because failed
generations determine the restart budget. Its expiry is an ensure with enough
retained generations to dominate decode cost.

This is a targeted repair of the old store pending #3392, as required by #3430.
It reuses exact declared selectors and the existing phase/annotation indexes.
It does not claim that reading 210 MB per event meets the long-term ceiling:
a resident live view with snapshot-then-deltas is the eventual shape. Building
another cache or view in the retiring path would duplicate the replacement.
The existing declared-query seam allows either backing implementation to lower
these predicates. No resource shape, index, or wire format changes.

Legacy adoption runs at subscribed startup and periodic resync, including
recovery while subscriptions fail. Events reconcile current Message evidence
without adopting legacy queues. Newly replicated legacy intent waits at most
until the next successful resync. Terminal briefs and supervisor turns are
ignored under the owner's ruling. The live selector includes missing phase,
preserving the existing dispatch-board inventory semantics.

## Reproduction

`convoy_filter_pushdown_decode_cost` in the store-testkit `decode_cost` target
seeds 300 Landed plus 10 Active convoys from the real stored-record corpus.
Half the records are local and half replicas. Two ensure annotations divide
the inventory equally. Per-request counters measure decoded rows; independent
SQLite sums measure the exact stored body bytes selected by each predicate.
The old whole-kind list and the new queries run on the same seeded database.

```sh
cargo test -p flotilla-store-testkit --locked --test decode_cost \
  convoy_filter_pushdown_decode_cost -- --ignored --nocapture
```

Byte counts describe serialized input decoded, not heap allocation or wire
traffic. Timing is intentionally not an acceptance threshold.

## Results

The mixed local/replica run decoded 310 rows / 1,622,193 stored bytes for
the old whole-kind list and 10 rows / 54,008 bytes for the live query. This is a
31x row reduction and approximately 30x fewer serialized bytes. The ensure
selector further narrows the ten live records to five / 27,449 stored bytes. Corpus body sizes are
measured fixture sizes, rather than the root workload model's 21 KB mean.

## Dummy fleet

The existing `dummy_fleet_load::load_baseline` was run with one daemon,
10 running convoys, a 25-second window, and the debug test profile. It reached
10 open convoys in 3 seconds, delivered 31 in the window, and reported zero
dispatch errors and zero failed/abandoned convoys. Reported transient reconcile
failures were 2 Convoy and 5 TerminalSession; one read was shed. This is a
functional load check, not a claim of zero retries or a production throughput
comparison. The standard `load_driver_smoke` is also included in the local gate.

```sh
CARGO_BUILD_JOBS=1 TMPDIR="$PWD/.codex-tmp" \
FLOTILLA_LOAD_RUNNING=10 FLOTILLA_LOAD_WINDOW_SECS=25 \
FLOTILLA_LOAD_MAX_RAMP_SECS=60 FLOTILLA_LOAD_DEADLINE_SECS=120 \
  cargo test -p flotilla-daemon --locked \
  --features flotilla-daemon/skip-no-sandbox-tests --test dummy_fleet_load \
  load_baseline -- --ignored --nocapture
```
