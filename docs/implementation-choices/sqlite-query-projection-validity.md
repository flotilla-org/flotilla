# SQLite query projection validity

Issue #3197 requires steady-state store opens to stop rewriting labels, owners and
all declared field-value tables. These are rebuildable materialized views of the
local and replica object tables; object bodies remain the authority.

## Workload and operations

The issue measured 444 local and 2,352 replica objects in a 102 MB store: an open
rewrote about 8.5 MB of WAL. Root `WORKLOAD.md` targets 100k retained convoys and
sets a 1M ceiling, with 1,000 sustained / 10,000 burst change events per second.
Rebuilding projections per startup scales with all retained objects and their
projected fields. A native open now checks one durable schema fingerprint and
installs triggers over the closed set of declared kinds and fields; it does not
scan object bodies for query-index repair in steady state. Existing diagnostics,
event compaction and the legacy field-index sweep remain separate startup work.

Native writes already maintain indexes transactionally. Their additional cost is
one singleton write-proof upsert and delete plus an indexed marker comparison per
object mutation, independent of history size. At the event ceiling, assuming one
object mutation per event, that is 20k additional singleton mutations per second.
The proof table uses a bounded page rather than allocating per-object state; at
most one additional proof-table page enters the WAL per committed transaction
(typically 4 KiB, or about 39 MiB/s at 10k one-object transactions per second).
That is an upper-bound incremental disk bandwidth assumption, not a measured
throughput guarantee. Batching amortizes it; existing transaction fsyncs are reused.
There is no network traffic, new resident object copy, or unbounded result.

## Candidates and choice

- Rebuild every open: rejected because its read/write cost grows with history.
- Trust only schema migration markers: rejected because unconfigured writers can
  leave field projections stale without changing resource versions.
- Check connection-local TEMP schema through a table-valued pragma in persistent
  triggers: rejected by the existing statement-cache tests. SQLite prepares a
  statement inside the pragma on every write.
- Restore the marker from native AFTER triggers: rejected because ordering among
  triggers within a phase is not a public guarantee, and native writes must not
  bless stale projections left by an earlier unconfigured write.
- Transactional native write proof: chosen. Native TEMP BEFORE triggers supply
  the schema fingerprint; persistent AFTER triggers invalidate a mismatched or
  absent proof and consume it. BEFORE/AFTER phases, SQLite's serialized writes,
  and transaction rollback provide the ordering and isolation. No persistent
  trigger requires application-defined functions. Existing statement-cache tests
  continue to assert zero statement preparations after warm-up.

The durable marker fingerprints kind identities, field declarations and an
explicit extraction-semantics version (`query-projections-v1`). Bump the named
`QUERY_PROJECTION_SCHEMA_VERSION` constant when projection semantics change.
Debug-formatting changes conservatively force a benign one-off repair. A missing or mismatched marker repairs metadata
and field projections in one transaction and publishes the current marker only
on commit. Persistent invalidation triggers are also replaced inside the repair
transaction so a schema-version bump upgrades their bodies along with the data.
A native write never restores an invalid marker. Both empty and
populated stores are marked after successful bootstrap.

## Evolution and assumptions

The marker and proof tables are additive internal SQLite schema. Resource specs,
statuses, events, manifests and configuration keep their stored shapes. Previous
native implementations lack the TEMP proof triggers, so their writes invalidate
this marker; the next upgraded open rebuilds. Existing metadata triggers stay
persistent and compatible with stock SQLite. The legacy field-index sweep stays
in place for #3173.

The proof exists only between an object's BEFORE and AFTER phases, and rolls back
with failed writes. No production projection trigger recursively mutates either
object table. If recursive object writes are introduced, or the projected-field
set stops being closed, revisit this proof protocol. Direct administrative edits
to projection tables/markers or disabling persistent triggers are outside the
writer-detection contract. Concurrent mixed-schema native connections invalidate
one another's markers conservatively; mixed-version serving is not supported.

## Validation

Host-side tests use real SQLite connections, including two connections sharing an
in-memory database for generated writer/commit/rollback interleavings. A file-backed
observer's `PRAGMA data_version` proves sync and async reopens commit no persistent
writes. Failure injection proves metadata and field repairs roll back together,
while the pre-existing unconfigured-writer test verifies repaired field queries
and unchanged bodies. Thread-local tracing capture in an isolated integration binary checks all phase
durations without racing other tests' process-wide callsite registration.

## Write-path measurements (PR #3255 review)

Compared the parent of the initial PR commit (`f4aa51c5b`) with the revised code on
this Linux vessel, using the existing ignored measurements and real SQLite file
connections. Both variants retain all their native projection/digest triggers.
Each replica-upsert measurement runs 500 iterations; each replay uses 1,024
25 KiB events. Commands:

```sh
cargo test -p flotilla-store-sqlite --release --locked measure_ -- --ignored --nocapture --test-threads=1
cargo test -p flotilla-store-sqlite --locked measure_replica_upsert_prepare_cost -- --ignored --nocapture --test-threads=1
```

Release-profile observations (one run per variant):

| Measurement | Original | Revised |
|---|---:|---:|
| Cached replica upsert within a transaction | 0.0349 ms | 0.0365 ms |
| Uncached replica upsert | 3.7368 ms | 4.2325 ms |
| Replica apply, one transaction per call | 1.1095 ms | 1.1430 ms |
| Point get, end to end | 0.0118 ms | 0.0168 ms |
| Replay on writer, range over 5 runs | 12.73–21.53 ms | 15.34–22.65 ms |
| Longest writer no-op during reader replay, range over 5 runs | 7.93–13.58 µs | 10.89–21.63 µs |
| Cached replica-upsert statement memory | 860,912 bytes | 867,064 bytes |

The production cached upsert added 1.6 µs (4.6%) in this sample, and complete
replica apply added 33.5 µs (3.0%). Statement memory grew by 6,152 bytes. The
uncached comparison illustrates why retaining the zero-preparation contract
matters; it is not the production write path. Reader-replay probes remained
below 22 µs. The replay/no-op benchmark probes queue placement rather than object
write throughput; its timings and unchanged point-get path also show host noise.

Three additional debug runs per variant gave median cached upserts of 0.1025 ms
(original, range 0.0990–0.1038) and 0.1043 ms (revised, range 0.1042–0.1473),
and median complete replica applies of 1.3984 ms (range 1.3742–1.6193) and
1.4182 ms (range 1.2544–1.9031). One revised run had elevated latency across
writes and point reads. These observations support accepting the bounded proof
cost for this startup fix, but do not establish sustained throughput at the
workload ceiling or claim a performance improvement on unrelated paths.


### Local resource-object write comparison

The replica-upsert benchmark above does **not** measure plain `resource_objects`
writes. PR review prompted a separate retained ignored measurement,
`measure_local_status_write_cost`, which drives `update_status_raw` on a local
Host through the real file-backed backend, including local projection/digest
triggers, event recording, and one transaction per status change. After one
warm-up write it measures three consecutive rounds of 500 writes each:

```sh
cargo test -p flotilla-store-sqlite --release --locked measure_local_status_write_cost -- --ignored --nocapture --test-threads=1
```

The original implementation (parent of `f4aa51c5b`) and the revised implementation
were measured with the same new measurement fixture:

| Local `resource_objects` status write | Original | Revised |
|---|---:|---:|
| Round 1 | 1.3847 ms | 1.2920 ms |
| Round 2 | 1.5003 ms | 1.3914 ms |
| Round 3 | 2.1248 ms | 1.2397 ms |
| Median | 1.5003 ms | 1.2920 ms |

Observed median delta: **−0.2083 ms (−13.9%)**. This comparison explicitly covers
the local write path, rather than extrapolating replica timings. The spread,
especially the third original round, reflects host/I/O variability; it is not
evidence of an attributable speedup. The singleton proof mechanism still adds
bounded work to both paths. These samples do not establish ceiling throughput.

Every startup phase stays at INFO per #3197, including fast phases. Keeping the
complete phase history in normal logs allows attribution of a missed health
window without first enabling a debug logger or deciding a latency threshold.
