# Change stream schema checkpoint (#3463)

Status: implemented after the [owner's schema approval](https://github.com/flotilla-org/flotilla/pull/3465#issuecomment-6252741558).
Every host follows every other home's whole log. Each home's log is the partition.
Fine-grained filtered views belong to read-only observation consumers and are out of scope.

The approved DDL is in `shared.rs`, installed by store startup, with readable views synchronized
on open like the kind views. The temporary `stream_schema` API and declaration macro are gone.
No resource kind's serialized shape changes; no out-of-repo resource authors need updates.
The log Event adds a coarse code, defaulted for the previous stored generation. Quint is unchanged.

`stream::serve` emits bounded ADR 0063 framed CBOR through an injected sink. It scans identities
in indexed pages and reads a snapshot one logical row at a time, so long append-only histories
are not materialized. A resource's root precedes its child rows; referenced frozen documents
travel with the rows. The follower installs a complete snapshot and each subsequent delivery
round transactionally. A disconnected round rolls back to its last committed prefix.

The listener-independent `Service` owns the new store. The daemon exposes
`GET /apis/flotilla.work/v1/change-stream[?after=<home_seq>]` on the existing resource HTTP API.
Its chunked response sends a snapshot when needed, tails ordered frames, and sends idle
keepalives. Peers request `once=true` rounds, which long-poll a resident committed home cursor
at 100 ms (one fifth of the root's 500 ms delivery budget), with an idle keepalive after five
seconds (root host cadence) and a 15-second I/O deadline (three cadences). No polling step
reads SQLite or writes an observation. There are two queued frames per round, with four
inbound and four outbound rounds permitted per daemon; permits release between rounds for
fairness. Frames allow the log profile’s 16 MiB body plus 1 KiB for the enclosing stream envelope. Input/output budgets are independent to
avoid cyclic backpressure. Queues plus active codec buffers can use roughly 512 MiB at their
maxima before model/SQLite caches; this is a bound, not a whole-daemon budget-fit claim.

Production readers use pooled read-only WAL connections. A brief writer barrier before the
read cut keeps a timed keepalive behind earlier transactions, then the writer lock is released
before any network backpressure. Identity caches are cleared between WAL read transactions
so an expunged/reused local ID cannot name an old resource. A shared 16 MiB encoded tail
serves recent followers without decoding/encoding each event again. Cold/resume misses read
bounded log pages. Pending lookup overlays are periodically cleared during large snapshot
transactions; uncommitted SQLite rows remain authoritative.
`GET /apis/flotilla.work/v1/record?kind=<code>&name=<encoded-name>` serves one home point read;
`read_record` routes one local miss there, marking it remote with its home-time age. A gate
reads `Read::watermark` locally and receives no remote transport capability.

Peer connection generations own their follower tasks over the existing forwarded resource
sockets; displacement/disconnect cancels the matching task. Failed links resume from committed
progress. No old replicator paths or peer-merge logic change. The core opens the decided store
beside the old store; kind migrations install the registered Service before starting peer tasks
through `install_decided_store`. The default registry is empty until kinds move (#3317), keeping
the existing no-production-machine-dependency transition guard intact. In-memory tests use
registered real/test kinds with the same source, follower, wire codec and local inputs.

## Readable table views

| Table | Columns | Primary key | Other indexes |
|---|---|---|---|
| log (changed) | 14: home, home_seq, resource, prev_seq, cause, edge, from, to, about, writer, at, payload, effect, **topic** | home, home_seq | resource/home_seq; about/at (partial); home/home_seq for expunges (partial) |
| stream_watermark (new) | 3: home, **home_seq**, applied_through | home | none |
| stream_history (new) | 2: home, **home_seq** | home | none |

No table exceeds 30 columns. These tables have no child tables or enum-expanded columns.
The log remains STRICT with its existing rowid; the two new tables are STRICT, WITHOUT ROWID.
The existing log cause/edge/state columns keep their code representation and readable names.
`topic` is an open scalar code (one to eight lowercase ASCII letters/digits/underscores,
starting with a letter), displayed directly by `log_view`. Both new `_view`s resolve the
home string and expose `home_seq`; applied time is INTEGER microseconds since the Unix epoch.
`stream.sql` is the complete shared DDL snapshot. The fixture snapshot includes its kind tables too.

The reserved topic is `all` for now. The event's originating write transaction chooses and
stores it alongside the event, including deletes and expunges; followers preserve it verbatim.
The schema defaults it to `all`, and later topic codes require no column or CHECK-list change.
No topic filter or extra index exists. Event, wire and log-append paths carry it explicitly;
a non-default replicated code round-trips in the behavior suite.

Missing watermark/history rows read as zero/epoch and floor zero; writers upsert.
All new progress uses `home_seq`, matching the log's partition key. A watermark names the
latest completely applied prefix; a history row names the minimum servable resume cursor.
The existing `home_sequence.last`, `resource_identity.seq` and kind-table `version` columns
are unchanged baseline fields, not additional progress schemas; `version` remains the public
resource version. No baseline column renames are proposed in this checkpoint.

## Transactions and correctness invariants

- **Resume floor:** the home's existing expunge write transaction advances
  `stream_history.home_seq` to at least the greatest home sequence of any log entry that the
  expunge removes. That same SQLite transaction deletes those entries and appends the expunge
  event. Commit publishes the deletion and floor together; rollback publishes neither. This
  covers holes anywhere in a home's log, not only removal of a contiguous prefix. The minimum
  floor is zero. A cursor equal to the floor can resume; one below requires a snapshot.
  Only expunge raises the floor; no archive/compaction or other trimming is implemented. Applied expunges that remove a follower's retained entries must conservatively
  advance that copy's floor in their apply transaction too, if it serves that home's log.
- **Applied watermark:** the follower's apply transaction commits rows and
  `stream_watermark(home, home_seq, applied_through)` together. The snapshot-install transaction
  establishes its cut before publishing progress. A keepalive carries the home's time and
  prefix sequence; its observation transaction may advance time only after that prefix is
  fully committed locally. A stalled stream holds the watermark. A gate sees only the local
  applied-time input, never link health or a remote query.
- **Monotonicity:** writer invariants reject decreasing home sequences or applied times and
  never restore a prefix ahead of committed rows. A same-sequence keepalive may advance time.
  No monotonicity trigger is proposed: the implementation must validate the combined
  snapshot/apply/keepalive transaction, including the prefix's actual coverage, which a
  scalar CHECK cannot establish. A reset/rebuild first makes the input unavailable and
  establishes a new complete snapshot; it cannot publish a speculative timestamp.
- **Observation state:** watermark updates never append durable events. Latest state may be
  coalesced only if crash recovery restores a conservative watermark. Floor metadata must be
  durable in the same commit as history loss; it cannot be asynchronously coalesced.

## Workload note and candidate walk

Reference: root WORKLOAD.md (hosts 3/50/500, change events <10/1,000/10,000 per second).
These are arithmetic bounds, not benchmark results. The rate of durable stream events,
encoded event bytes, distribution of writes over homes, and overlap of per-home resource
populations need measurements with the ADR 0064 plans (#3425). No budget-fit claim follows
from the current estimates.

**Bandwidth per host.** Let H be hosts, R the rate of durable writes at each home, and B the
encoded event size. With evenly distributed writes, every host receives (H−1) × R × B bytes/s
and its own home sends the same amount; total mesh deliveries are H × (H−1) × R per second.
Replication never re-originates a copied event as a new local write.

Root scale rates describe one large daemon. Conservatively, if every home produces that
rate, target ingress is 49 × 1,000 = 49,000 events/s and ceiling ingress is 499 × 10,000 =
4.99 million events/s per host. Using 300 B/event as an explicit wire-size assumption (the
root's ~300 B durable-entry estimate includes local header/index costs and is **not** a CBOR
measurement), that is 14.7 MB/s at target and 1.497 GB/s (~12 Gbit/s) at ceiling, in each
direction before transport overhead. At 1 KiB/event the ceiling is 5.11 GB/s (~40.9 Gbit/s).
If 10,000 events/s is instead the **whole mesh's** burst budget, ingress is at most
10,000 × B (3 MB/s at 300 B), not 499 times that. Measure aggregate durable write rate before
selecting the deployment interpretation; observations are not durable log writes.

At the root disk-budget rate of 59 durable writes/s **per home**, 500 homes produce 29,500
entries/s. Every full replica retains ~5.35 TB of 300-byte entries over seven days, before
extra replication overhead; ingress from the other 499 homes is ~8.83 MB/s. The existing
10 GB daemon store budget does not fit that independent-per-home scenario. Removing per-item
metadata avoids its extra write/index amplification but does not solve full-log storage cost.
The topic is one inline scalar per log entry, with no separate rows or indexes.

**Joining snapshot.** Every joining host receives all identities and bodies from all homes,
plus referenced frozen objects (deduplicated by hash), paged at consistent per-home cuts.
Its bytes are the sum of each home's unique snapshot; replicated copies are not sent again
as independent ownership. The root's stored-body estimate is ~9.5 GB typed at ceiling for
one full dataset including its history (about 4.75 GB stored under the root's 2x assumption),
already above the 1 GB resident-state budget. It is a lower bound for snapshots that also
include kinds omitted from that estimate, and is not a measured CBOR snapshot size.

If each of 500 homes owns an independent ceiling-sized dataset, the corresponding total is
~2.375 TB stored (~4.75 TB typed); if the root dataset is shared across those homes, it remains
~4.75 GB stored. Do not multiply an already replicated dataset by host count. As a separate
live example, one cohort has 10,000 convoys + 36,000 vessels + 36,000 terminals + 500,000
messages = 582,000 identities. An assumed 128 B/identity adds ~74.5 MB before retained
identities and definitions; these sizes/counts are not measured wire widths. Current-state
snapshots need not transmit the old event log; the tail starts at each snapshot's cut.

**Execution shape.** Per local write: one event and one coarse code, then O(H−1) deliveries.
Each host has at most H−1 upstream home cursors (499 at ceiling), with no resource-dependent
replication bookkeeping. Reject whole-store polling: bounded snapshot then ordered event
pages avoids repeated decoding of unchanged state. Tail delivery uses resident bounded
buffers, not one store read per follower per event; shared encoded batches can be reused.
Restart and resume may read bounded log pages. Backpressure disconnects a slow follower to
resume or resnapshot instead of growing an unbounded queue. Snapshot pages are bounded by
bytes and rows, so memory use is independent of total dataset size. Watermarks are resident
local inputs, promoted only after commit and rebuilt at startup. Bandwidth, disk and snapshot duration require measurements before claiming
production-budget compliance.

**Evolution and boundary.** Topic codes are open and never reused. Stored data follows the
pre-production baseline in `shared.rs`, then ADR 0047 after cutover. Transport will be ADR
0063 CBOR with strings, never local IDs. Keep the future HTTP handler listener-independent,
leave old replication intact, and mark routed reads remote with their age. Partial replication
by whole topics, filtered observation consumers and archives remain deferred.

## Verification

Two/three-store scenarios cover snapshot/tail/resume, expunge-floor rollback and resnapshot,
whole-home identity/body distribution, stalled and invalid watermarks, non-default topic
preservation and routed misses. Hegel generates partition/resume sequences and snapshot row
shapes across all nine fixture kinds, including frozen objects and nested records. WAL writer/read concurrency, pooled ID reuse, shared-tail delivery and snapshots crossing
identity-page/cache bounds are also covered. The real
Unix HTTP client is tested against a strict in-process request stand-in; duplex I/O tests the
listener-independent response adapter. SQL tests execute the DDL with foreign keys and read
every view. Rust conformance exercises the unchanged stream module's prefix watermark rule;
`quint test specs/primitives/tests/stream_properties.qnt --backend typescript` checks its
finite property directly. No Quint spec edits were needed.
