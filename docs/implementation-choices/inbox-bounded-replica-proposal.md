# Bounded inbox replica: schema review before implementation

Status: proposed for owner look-over under #3466. This is the schema-first
phase, not the completed bounded-replica implementation. Runtime paths, the
InboxBackend trait, shared acceptance suite and Quint specifications are unchanged.

## Proposed schema

Add one declared partial index to the existing Message table:

```sql
CREATE INDEX "message_pending_by_inbox" ON "message" ("inbox") WHERE "state" IN ('pe');
```

| Table | Columns | Enum expansion | Children | Change |
|---|---:|---|---|---|
| `message` | 22 | 5 columns: expectation 2, reason 3 | `message_references` | Add `pending_by_inbox(inbox)` restricted to Pending |
| `message_references` | 5 | none | none | none |

Neither table exceeds 30 columns. There are no new tables, fields, state codes,
frozen objects, foreign keys or serialized values. Existing open/deadline/reply/
supersession indexes remain. Generated DDL and the readable schema view accompany
this proposal. There are no out-of-repo author changes: the serialized shape is
unchanged.

## Why this index

A bounded resident replica must be able to evict and refill pending query state
without losing completeness. The existing `open_by_inbox` index intentionally
includes Awaiting. It can rebuild open state, but using it for a cold pending
query makes that query walk every Awaiting message in the inbox, even when no
Pending message exists. Awaiting may be long-lived, and the workload gives no
bound on its ratio to Pending. A Pending-only declared index keeps the cold
pending query proportional to its answer. It also prevents a future fallback
from becoming a scan-and-filter over terminal messages.

The generated page order is `(inbox, resource id)`; local insertion ids preserve
admission order. A Pending resource cannot have a self-edge, so its admission
order does not change. The implementation must test this equivalence against
the existing numeric admission-sequence ordering, including interleaved consumer
writes and other inboxes. Cross-home replica arrival order is not a substitute
for a home's admission order; the implementation must restrict queries to its
home and preserve the home-local InboxBackend contract.

Candidates:

- Whole-kind scan and filtering: prohibited for runtime reads by the issue and
  by `Read::scan`'s startup-only contract.
- `open_by_inbox`: retained for open-state recovery/refill; rejected for the
  pending fallback because Awaiting cardinality is unrelated to pending output.
- Full resident pending bodies: rejected at 500,000 messages × 2 KB × the root
  2x typed factor, about 2 GB before indexes, above the 1 GB daemon budget.
- Pending-only partial index: chosen. No row duplication, and terminal/Awaiting
  messages consume no entries in this index.

## Sizing and implementation boundary

Root scales: 1,000/10,000 running convoys at target/ceiling, 50 messages per
convoy, 2 KB message bodies, 500 clients and 10,000 burst changes/s. If all
500,000 ceiling messages are Pending, the added index has 500,000 entries.
At an estimated 24–48 bytes per SQLite index entry, that is 12–24 MB on disk,
not resident Rust resource bodies. This is a sizing assumption to validate,
not a measurement or a claim about SQLite's page cache.

A cold pending page uses one declared-index read plus at most one typed load
per returned id, bounded by the requested page size. With 256-row pages that
is at most 257 logical store API reads per page; physical SQL statement counts
and frozen-object cache effects must be measured during implementation. A warm
pending query remains an indexed resident lookup. No Settled or Dropped history
is needed by either route. The index adds one B-tree update on enqueue and one
removal when leaving Pending, including Deliver to Awaiting.

The subsequent implementation must budget the resident open typed values,
bindings, receipts/evidence and pending index together, evict query coverage
explicitly, and report the oldest/newest covered timestamps and current bytes.
This proposal does not assert that a full open set always fits. The final size
model must include large bodies, reference vectors, receipt fanout, bindings,
consumer ports and transient query/worker allocations. Budget overflow must
never silently truncate a pending result.

After owner look-over, implement log-backed watch via `changes_since` (with
its memory equivalent), keyed cold terminal loads and `about` receipt reads,
streamed startup recovery, removal of the copied watch journal and unjustified
64-worker permits, and targeted evidence snapshots rather than whole-world
clones. Keep other engine machines unchanged. Record each query's default route,
new expiry conditions and actual timings in the component workload and existing
implementation record. Add the terminal-history read-count guard, run the shared
suite unchanged for both backends, and replay the unchanged Quint race traces.

The pending-only index's own expiry is a change to pending query semantics or
ordering, or measured index write/storage cost exceeding the workload budget.
It does not impose a fixed resident history window or trim stored data.

## Review requested

Approve or reject the added Pending-only partial index before runtime code is
written against it. The draft PR must remain unmerged until the full #3466
implementation and acceptance evidence replace this proposal-stage status.
