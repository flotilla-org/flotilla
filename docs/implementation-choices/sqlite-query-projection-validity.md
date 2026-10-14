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
explicit extraction-semantics version (`query-projections-v1`). Change the version
when projection semantics change. A missing or mismatched marker repairs metadata
and field projections in one transaction and publishes the current marker only
on commit. A native write never restores an invalid marker. Both empty and
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
