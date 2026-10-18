# Bounded inbox replica: revised schema review before implementation

Status: awaiting the second owner look-over under #3466 / PR #3467. This is
still the schema-first phase, not the bounded-replica implementation. Runtime
inbox paths, the InboxBackend trait, shared acceptance suite and Quint specs are
unchanged. This proposal incorporates the owner's first look-over: replace the
mixed open index and carry admission order explicitly on the row.

## Proposed schema

Message gains one required integer column, `admitted_seq: u64`. Replace
`open_by_inbox` with one ordered Pending-only index:

```sql
CREATE INDEX "message_pending_by_inbox" ON "message" ("inbox", "admitted_seq") WHERE "state" IN ('pe');
```

| Table | Columns | Enum expansion | Children | Change |
|---|---:|---|---|---|
| `message` | 23 (was 22) | 5 columns: expectation 2, reason 3 | `message_references` | Add `admitted_seq`; replace mixed open index with ordered Pending index |
| `message_references` | 5 | none | none | none |

Neither table exceeds 30 columns. The Message readable view also exposes
`admitted_seq`. There are no new tables, state codes, frozen objects or foreign
keys. Existing deadline/reply/supersession indexes remain. No Awaiting index is
proposed: no current runtime read needs one. Generated DDL and the readable
schema view accompany this proposal.

`MessageSpec` here declares the engine's stored Message, not the legacy
`flotilla-resources::MessageSpec` used by production. The build-graph guard
prevents production callers of this backend. There are no out-of-repo authors of
this engine kind; no companion author changes are needed. This is nevertheless
a stored-shape change: the pre-cutover SQLite runtime creates missing schema but
does not migrate existing tables. The draft is not a deployable migration, and
existing-schema compatibility must be resolved before any production switch.

## Admission order and query cover

The owning home assigns `admitted_seq` before committing create, using that
write's home sequence. The value stays immutable through subsequent edges and
is replicated with the row, so stores with different local resource ids retain
the same home-local admission order. It is not the mutable resource version.
The zero constructor value is unassigned; stamping the field belongs to the
runtime phase after this look-over, not this schema patch.

The Pending index orders by numeric `admitted_seq`, with local id only the
store's generic tie-breaker. Pending admissions within one home are unique, so
that tie-breaker cannot reorder that home's messages. Different homes have
independent sequences; there is no promised global admission order across
homes. The runtime must keep the home-local InboxBackend query cover explicit
and must not use local ids to merge homes into a fictitious global order.

The schema test writes twelve admissions at each of two source homes, preserving
the create's actual home sequence in the row. It then restores their snapshots
in different orders into two stores, with unrelated local rows shifting ids.
Both homes share the inbox and overlapping sequences. Generated page sizes
1–5 and snapshot rotations exercise keyset boundaries; assertions verify numeric
sequence order, each home's message-1 through message-12 subsequence, replicated
column values, and actual local ids on both sides of 9/10 and 99/100. The
existing stateful index property now models Pending membership and admission
ordering after inserts, edits, edges and deletes, including equal sequences.

## Candidate walk

- Whole-kind scan and filtering: prohibited for runtime reads by the issue and
  by `Read::scan`'s startup-only contract.
- Mixed `open_by_inbox` plus Pending index: rejected by the owner; nothing
  outside tests uses the mixed index, and it duplicates enqueue maintenance.
- Mixed open index alone: rejected because a cold pending query would walk
  unrelated Awaiting rows, whose ratio to Pending is unbounded.
- Local surrogate or message-name order: rejected because ids differ across
  stores and text names put message-10 before message-9.
- One Pending index ordered by the row's home-assigned sequence: chosen. No
  terminal or Awaiting rows occupy this index. Add another index only for a
  demonstrated read path and present it for owner look-over first.

## Sizing and implementation boundary

Root scales: 1,000/10,000 running convoys at target/ceiling, 50 messages per
convoy, 2 KB message bodies, 500 clients and 10,000 burst changes/s. If all
500,000 ceiling messages are Pending, the index has 500,000 entries. Assuming
32–56 bytes per SQLite entry including the sequence gives 16–28 MB on disk,
plus at most about 4 MB of raw 8-byte admission values in Message rows. These
are sizing assumptions, not measured page occupancy or resident Rust memory.

A cold pending page uses one declared-index read plus at most one typed load
per returned id: at most 257 logical store API reads for a 256-row page. Warm
pending reads remain resident index lookups. Neither route needs Settled or
Dropped history. The proposed index replaces the old mixed index rather than
adding another: at an all-enqueue burst of 10,000 changes/s, inbox-index
maintenance is 10,000 insertions/s rather than 20,000. Leaving Pending removes
one entry; Awaiting-to-terminal writes no longer maintain a mixed inbox index.
These counts exclude the unchanged deadline/reply/supersession indexes. Measure
write latency/throughput and actual resident allocations in the runtime phase;
this arithmetic is not a throughput result.

After the second owner look-over, implement log-backed watch via `changes_since`
(and its memory equivalent), keyed terminal loads and `about` receipt reads,
streamed startup recovery, and the bounded resident typed open state and indexes.
Budget bindings, receipts/evidence, consumer ports and transient query/worker
allocations as well as bodies and reference vectors. Eviction must preserve
query completeness. Report current bytes and the oldest/newest covered times,
not a fixed history window. Remove the copied watch journal and unjustified
64-worker permits, and avoid whole-world gate clones while leaving other
engine machines unchanged.

The final component workload and implementation record must record every
query's route, actual timings, resident bytes at the ceiling and expiry
conditions. Add the terminal-history read-count guard, run the shared suite
unchanged for both backends, and replay unchanged Quint race traces.

Revisit or remove this index when pending semantics or its ordering requirement
changes, or measured write/storage costs exceed the workload budget. It neither
sets resident retention duration nor trims stored data.

## Review requested

Look over the required admission column and the replacement ordered Pending
index before runtime code uses them. Keep the PR draft until the full #3466
implementation and acceptance evidence are ready.
