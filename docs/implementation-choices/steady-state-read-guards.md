# Steady-state durable-store read guards

Issue #3141 enforces the crossing budget in root `WORKLOAD.md` without doing
resident-state redesign (#3120 and successors).

## Workload and choice

The operations are event delivery, leaf wake, Aggregator delta emission, stall
judgement, and subscriptions after warm-up. They run within one process through
real store adapters; no Docker or live hosts are involved. At target, naive
per-event/per-client reads cost 1,000 events/s × 50 clients = 50,000 crossings/s;
at ceiling, 10,000 × 500 = 5 million/s. The budget is zero at either size.
Startup snapshots and explicit gap recovery are outside the measured interval.

Use the existing per-kind, per-operation counters, with a snapshot delta after
warm-up. `ZeroReadGuard` holds one backend handle and two small counter maps
(one baseline, one comparison), independent of object/history/client counts.
The maps are bounded by the resource-kind/operation vocabulary, not the number
of events. No production data path, resident index, retention policy, protocol,
or stored shape changes. The only adapter changes correct get accounting:
missing/failed gets cost a call, with zero returned objects.

A task-local request scope was rejected for worker-driven delivery: it excludes
spawned watch tasks, including replica identity resolution. Backend-wide counts
include those tasks. Each scenario owns an isolated backend; fixture operations
use an uncounted clone sharing its state. Completion is established through an
observable event, invalidation, or completed judgement, rather than a sleep.
These isolation and completion assumptions are stated at the harness seam.

Small deterministic scenarios cover adapter boundaries and event variants;
existing generated leaf routing scenarios continue covering unrelated objects,
duplicates and fan-out. These are crossing guards, not timing or load claims.
The profiler test is the arithmetic above: any positive crossing fails the
steady-state budget regardless of object population. Timing and resident-memory
validation of the redesign remain its own work.

## Coverage and expected failures

| Path | Guard |
| --- | --- |
| Local watches, 1 and 8 clients, memory and SQLite | zero for create/update/delete |
| Raw and replica-inclusive watches, 4 clients, memory and SQLite | zero for peer create/update/delete |
| Warm WatchView subscriptions, memory and SQLite | zero for empty/populated snapshots and delivery |
| Aggregator | zero for convoy/session deltas and warm result-set retrieval |
| Aggregator definition deltas | expected failure: definition watch winning source lookup (#3120) |
| Leaf reconcile and stall judgement | zero after views bootstrap |
| Addressed leaf wake | expected failure: addressed subjects reload on wake (#3120) |
| Leaf subscription after warm-up | expected failure: even already watched subjects reload on attachment (#3120) |
| Self-origin replica-inclusive delivery | expected failure: identity shadow lookup (#3120) |
| Merged definition subscription delivery | expected failure: winning source lookup (#3120) |

Expected failures are **executed** `should_panic` tests, matching only the
zero-budget diagnostic. Setup, timeout and behaviour failures cannot satisfy
them. When a path reaches zero, the test fails because it no longer panics;
remove the attribute as part of its redesign. They are not ignored tests.

## Adding a path

1. Give the controller/view a `backend.clone().with_read_counts()`; keep the
   original backend for fixture reads and writes.
2. Await bootstrap, then `ZeroReadGuard::after_warm_up(&counted)`.
3. Drive an event and await its observable result.
4. `guard.assert_zero("path name")`.

For work confined to the calling task, `measure_reads` and
`reads.of(&backend).assert_zero("path name")` also work. The harness refuses
uncounted backends. Workspace tests register the new store scenarios in the
existing integration binary; no feature flag or platform selector is needed.
