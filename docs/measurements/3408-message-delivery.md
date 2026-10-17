# Message delivery scheduling (#3408)

Measured in the crew vessel on 2026-10-17. Baseline is the assignment checkout
before the scheduling change, with only measurement/test instrumentation added.
Both runs use the default unoptimized Cargo test profile, default TMPDIR,
production controller cadences, and no concurrent builds during measurement.
The reports below describe the initial implementation at `d78d3c180`.
This is a small vessel comparison, not the operator's feta d1-n1000 check.

## Commands

```sh
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=100 \
FLOTILLA_LOAD_RAMP_PER_SECOND=20 FLOTILLA_LOAD_MERGE_AFTER_MS=180000 \
FLOTILLA_LOAD_WINDOW_SECS=60 FLOTILLA_LOAD_REPORT=/tmp/message-before-clean \
cargo test -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
```

Use `/tmp/message-after` as the report prefix for the changed version. The clean
baseline invokes the already-built test executable directly with the same
settings, to exclude Cargo compilation from the measured process lifetime.

The harness reports latency as **p50 / p90 / p99 / max**, not min / p50 / p90 / max.
Quantiles are histogram bucket upper bounds. The issue's column labels should
not be used to reinterpret these rows.

## Store reads per delivery pass

```sh
cargo test -p flotilla-messaging --locked --test message delivery_receiver_reads_are_bounded -- --nocapture
```

The test retains these counts as upper-bound assertions rather than printing
measurement scaffolding. A real in-memory store with its read observer counts all adapter read calls,
including empty queries, across one entire `reconcile_delivery` call. Each
active record addresses a distinct unresolved role. Fixtures are fully admitted
before the counter snapshot; no unrelated background controllers run.

| Active messages | Before reads/pass | After reads/pass |
|---|---:|---:|
| 0 | 3 | 3 |
| 10 | 30 | 30 |
| 200 | 410 | 410 |

For nonempty passes the receiver snapshot costs six reads: one local and one
replica read for each of Convoy, TerminalSession and ConvoyEnsure. One Demand
query and `2n + 3` Message reads account for the remainder. Empty passes make
two Message queries and one Demand query. These counts describe this fixture;
ready receivers and reference/outcome conditions have additional reads.
The scheduler additionally makes one active-message query per pass to choose
its deadline. The fix removes multiplicative passes, not the per-pass reads.

## Scheduling regression

```sh
cargo test -p flotilla-daemon --locked --lib message_resync_coalesces_active_messages
```

Before: ten active messages produce ten full passes per resync (assertion:
`left: 10, right: 1`). After: one pass at each population.
The changed scenario covers empty, singleton, ten- and hundred-message inboxes,
then adds new admissions through the real inbox and federated watch. It checks
that all admissions are observed, including admissions interleaved with delivery.

## Mutation checks

- Remove the Message dependency watch: the controller scenario fails because
  new admissions do not wake delivery (`active=0`).
- Select the latest rather than earliest delivery deadline: Hegel shrinks the
  failing case to deadlines one and two seconds away (`Some(2s)` vs `Some(1s)`).

Both mutations were reverted before the local gate.

## Smaller load results

| Measurement | Before | After |
|---|---|---|
| Sampled active-message peak | 7 | 14 |
| Message reconcile p50 / p90 / p99 / max | 6.4 / 213 / 1180 / 3271 ms | 106 / 442 / 2032 / 2295 ms |
| Message reconciles in window | 659 | 271 |
| Message queue peak | 192 | 1 |
| Message failures in window | 0 | 2 |
| Turn delivery p50 / p90 / p99 / max | 1704 / 4456 / 9437 / 19103 ms (n=192) | 2032 / 4456 / 19923 / 32369 ms (n=186) |
| Resident memory | 1347.8 MiB | 1220.6 MiB |

Active counts are point samples at the load driver's sampling cadence, not an
exact high-water mark. Before samples: 0, 6, 2, 4, 7, 7, 3, 1.
After samples: 0, 3, 2, 8, 14, 14, 2, 4.

`Message queue peak` is now constant at one **by construction**: it denotes one
namespace dirty bit, rather than the old queued-message-name count. Similarly,
`resync_enqueued` now counts namespace wakes, not Message objects. These metrics
are not like-for-like queue measurements; the controller regression directly
checks pass counts. Active-message samples used one temporary active query per
home per sample; the sampling query and print were removed after collecting
these reports. Future load runs do not print active-message samples.

The queue no longer accumulates message names, and resync scheduling is constant
in message count. This small unoptimized run **does not demonstrate improved
turn latency**: p90 is unchanged and p99/max are worse. It also has two failed
passes, versus zero before. A failed pass is handled by controller supervision;
watch relisting restarts the namespace loop. The full operator run is needed
to assess latency at the scale where redundant queued passes dominated.
The old reconcile histogram mixes terminal-record no-ops with whole-inbox
passes; the new histogram records whole-inbox passes, so its median is not a
like-for-like per-pass timing comparison.

### Before report

Run started 2026-10-17 14:38:05.425070031 UTC.

**100 running convoys on 1 daemon(s)**, mix Coder×6, CoderReviewer×3, LongRunner×1.

Ramp: reached after 18 s (peak 100 open). Window: 60 s.

| Measure | Value |
|---|---|
| Delivered in window | 8 (0.13/s) |
| Abandoned / failed in window | 0 / 0 (0 at the deadline) |
| Dispatch → delivered | 50332 / 63538 / 63538 / 63538 ms (n=8) |
| — LongRunner | 50332 / 63538 / 63538 / 63538 ms (n=8) |
| Admission | 2490 / 3801 / 4447 / 4447 ms (n=108) |
| Dispatch errors | 0 of 108 |
| Resident memory (process) | 1347.8 MiB |

**Daemon 0**

| Measure | Value |
|---|---|
| Write queue wait | 6.4 / 31.7 / 69.6 / 106 ms (n=7591) (peak depth 20) |
| Write execution | 3.7 / 7.2 / 15.4 / 27.6 ms (n=7591) (126.4/s) |
| Read queue wait | 0.77 / 8.2 / 65.5 / 590 ms (n=121184) (peak depth 32) |
| Read execution | 0.16 / 0.57 / 20.5 / 733 ms (n=121184) (2018.4/s, 0 shed) |
| Turn delivery (created → receipt) | 1704 / 4456 / 9437 / 19103 ms (n=192) |
| Replication lag (admitted → replica) | – |
| Replication | 0.0 events/s, 0 snapshots, 0 lag resets, 0 stream failures |
| Database (with WAL) | 56.6 MiB |

| Controller | Wakes/s | Requeues/s (coalesced) | Resync enqueued/s | Reconcile | Queue peak | Failures |
|---|---|---|---|---|---|---|
| Checkout | 30.0 | 1.1 (0) | 1.6 | 8.7 / 53.2 / 279 / 778 ms (n=1711) | 96 | 0 |
| Clone | 0.0 | 0.0 (0) | 0.0 | – | 0 | 0 |
| Convoy | 16.0 | 9.6 (426) | 1.7 | 41.0 / 123 / 442 / 753 ms (n=936) | 104 | 0 |
| Environment | 5.2 | 0.0 (0) | 1.6 | 10.2 / 55.3 / 221 / 707 ms (n=229) | 97 | 0 |
| ImageBuild | 0.0 | 0.0 (0) | 0.0 | – | 0 | 0 |
| Message | 11.0 | 2.0 (97) | 3.2 | 6.4 / 213 / 1180 / 3271 ms (n=659) | 192 | 0 |
| Repository | 3.3 | 0.0 (0) | 0.0 | 127 / 475 / 1180 / 1528 ms (n=199) | 1 | 0 |
| TerminalSession | 23.8 | 0.0 (0) | 2.1 | 22.5 / 73.7 / 377 / 671 ms (n=1293) | 128 | 1 |
| Vessel | 19.1 | 4.3 (159) | 1.7 | 31.7 / 102 / 426 / 829 ms (n=1063) | 100 | 0 |

Open at the end of the window:

| Where | Convoys |
|---|---|
| Active · coder Working | 66 |
| Active · coder Working, reviewer Done | 30 |
| Active · aide Working, governor Working | 2 |
| Active · coder Working, reviewer Working | 2 |

### After report

Run started 2026-10-17 14:46:40.835175277 UTC.

**100 running convoys on 1 daemon(s)**, mix Coder×6, CoderReviewer×3, LongRunner×1.

Ramp: reached after 22 s (peak 100 open). Window: 61 s.

| Measure | Value |
|---|---|
| Delivered in window | 6 (0.10/s) |
| Abandoned / failed in window | 0 / 0 (0 at the deadline) |
| Dispatch → delivered | 60817 / 66500 / 66500 / 66500 ms (n=6) |
| — LongRunner | 60817 / 66500 / 66500 / 66500 ms (n=6) |
| Admission | 3277 / 4194 / 4629 / 4629 ms (n=106) |
| Dispatch errors | 0 of 106 |
| Resident memory (process) | 1220.6 MiB |

**Daemon 0**

| Measure | Value |
|---|---|
| Write queue wait | 7.9 / 41.0 / 102 / 147 ms (n=7352) (peak depth 20) |
| Write execution | 4.4 / 9.2 / 19.5 / 95.9 ms (n=7353) (121.5/s) |
| Read queue wait | 0.70 / 7.4 / 47.1 / 650 ms (n=116674) (peak depth 33) |
| Read execution | 0.18 / 0.64 / 22.5 / 746 ms (n=116676) (1928.5/s, 0 shed) |
| Turn delivery (created → receipt) | 2032 / 4456 / 19923 / 32369 ms (n=186) |
| Replication lag (admitted → replica) | – |
| Replication | 0.0 events/s, 0 snapshots, 0 lag resets, 0 stream failures |
| Database (with WAL) | 51.0 MiB |

| Controller | Wakes/s | Requeues/s (coalesced) | Resync enqueued/s | Reconcile | Queue peak | Failures |
|---|---|---|---|---|---|---|
| Checkout | 27.8 | 1.1 (0) | 1.6 | 10.8 / 61.4 / 246 / 550 ms (n=1602) | 98 | 0 |
| Clone | 0.0 | 0.0 (0) | 0.0 | – | 0 | 0 |
| Convoy | 15.5 | 9.3 (406) | 1.7 | 41.0 / 127 / 410 / 754 ms (n=917) | 102 | 0 |
| Environment | 5.1 | 0.0 (0) | 1.6 | 12.3 / 69.6 / 311 / 585 ms (n=229) | 99 | 0 |
| ImageBuild | 0.0 | 0.0 (0) | 0.0 | – | 0 | 0 |
| Message | 4.5 | 0.0 (0) | 0.0 | 106 / 442 / 2032 / 2295 ms (n=271) | 1 | 2 |
| Repository | 3.5 | 0.0 (0) | 0.0 | 139 / 492 / 1049 / 1476 ms (n=211) | 1 | 0 |
| TerminalSession | 23.0 | 0.0 (0) | 2.2 | 24.6 / 73.7 / 295 / 867 ms (n=1271) | 136 | 0 |
| Vessel | 18.0 | 3.9 (136) | 1.6 | 36.9 / 106 / 360 / 858 ms (n=1011) | 99 | 0 |

Open at the end of the window:

| Where | Convoys |
|---|---|
| Active · coder Working | 66 |
| Active · coder Working, reviewer Done | 30 |
| Active · aide Done, governor Working | 3 |
| Active · aide Working, governor Working | 1 |


## Operator check

Run from this branch on feta, using the original release load profile:

```sh
FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_RUNNING=1000 \
FLOTILLA_LOAD_RAMP_PER_SECOND=20 FLOTILLA_LOAD_MERGE_AFTER_MS=180000 \
FLOTILLA_LOAD_WINDOW_SECS=300 FLOTILLA_LOAD_REPORT=/tmp/message-d1-n1000 \
cargo test --release -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
```
