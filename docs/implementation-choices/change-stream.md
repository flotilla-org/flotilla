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
keepalives. Peers request `once=true` rounds every five seconds, with a 15-second network
read/write deadline. There is no peer-count semaphore or encoded tail cache: each round opens
one read-only WAL connection, reads `changes_since` pages, encodes them, and closes the
connection. A brief writer barrier before the read cut orders the home's time after earlier
transactions; the writer lock is released before delivery. The outgoing channel holds at
most two frames; the frame limit is the log profile's 16 MiB plus 1 KiB envelope room.

The follower stages each HTTP round in an automatically deleted temporary file, validating
frame bounds as bytes arrive. Only after a complete response ending in a keepalive does it
acquire the store mutex and replay that local file in one transaction. A disconnect discards
the staging file without changing rows or progress. Disk staging avoids buffering a complete
snapshot in memory. Its disk cost is one round per connected home; snapshots can be much
larger than tails. The network never waits inside `with_store`. Resident committed watermarks
have a separate lock, so gates can read the old safe watermark during both network stalls and
snapshot installation. Pending lookup overlays are periodically cleared during large
transactions; uncommitted SQLite rows remain authoritative.

`GET /apis/flotilla.work/v1/record?kind=<code>&name=<encoded-name>` serves one home point read;
production `read_record` routes one local miss there and marks it remote with its home-time
age. Gates receive no remote transport capability.

Peer connection generations own their follower tasks over the existing forwarded resource
sockets; displacement/disconnect cancels the matching task. Failed links resume from committed
progress. The old replicator is unchanged. No decided store is opened lazily: before a kind
migration installs its registered Service with `install_decided_store`, both routes answer
503 and no followers start. #3317 installs the first kind before peer tasks start and verifies
the complete production path. Tests exercise the generic mechanism before that integration.

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
  applied-time input, never link health or a remote query. Home clocks are trusted: a
  clock ahead of real time can advance freshness prematurely, and the monotonic maximum
  does not retract that time. Clock synchronization is an operational prerequisite.
- **Monotonicity:** writer invariants reject decreasing home sequences or applied times and
  never restore a prefix ahead of committed rows. A same-sequence keepalive may advance time.
  No monotonicity trigger is proposed: the implementation must validate the combined
  snapshot/apply/keepalive transaction, including the prefix's actual coverage, which a
  scalar CHECK cannot establish. A reset/rebuild first makes the input unavailable and
  establishes a new complete snapshot; it cannot publish a speculative timestamp.
- **Observation state:** watermark updates never append durable events. Latest state may be
  coalesced only if crash recovery restores a conservative watermark. Floor metadata must be
  durable in the same commit as history loss; it cannot be asynchronously coalesced.

## Workload note: first home-lab shape and its expiry

This first shape is for a few home-lab hosts, including laptops that disconnect. It is not
sized for WORKLOAD.md's 50-host target or 500-host ceiling. Assume three homes, an aggregate
sustained 10 durable events/s, and 1 KiB encoded per event, evenly distributed. Each host
receives about 6.7 KiB/s and sends about 6.7 KiB/s; the whole mesh delivers 20 events/s.
At a 100 events/s aggregate burst those figures rise tenfold. These are explicit planning
assumptions, not measurements or a claim that event widths are always 1 KiB. A large frozen
document may make an individual event much larger; observations do not enter this log.

Per write the extra storage is one inline topic code; serving does O(H−1) deliveries, SQL
page reads, and CBOR encoding for the peers. Each host holds H−1 progress cursors and opens
one reader per active outgoing round. At three hosts that means two upstream cursors and
normally two outgoing readers, with no resource-dependent subscriptions. Five-second idle
rounds add 0.4 incoming keepalives/s per host. There is no fleet-wide peer queue: every direct
home can make progress independently, while SQLite serializes local apply transactions.

A laptop offline for a day misses about 576,000 remote events under the sustained example:
roughly 563 MiB of wire data at 1 KiB/event, plus framing and transport overhead. If an
expunge raises the floor beyond its cursor, it receives a fresh snapshot instead. There is
no archive/history retention scheduler in this issue; expunge is the only floor advance.
Every full replica retains all three homes' durable logs and bodies, so disk use grows with
aggregate writes. At 10 events/s and 1 KiB/event the mesh's log alone adds about 844 MiB/day
per host before SQLite/index overhead. This must be measured and managed even at home-lab scale.

**Joining snapshot.** For an illustrative combined dataset of 30,000 unique resource bodies
and identities at 2 KiB each plus 100 MiB of referenced frozen objects, the join is about
159 MiB across the homes, not three times that already combined dataset. At 10 Mbit/s it
takes roughly 2.2 minutes before protocol/disk overhead. Current bodies are sent once at
consistent per-home cuts; old event history is not part of the snapshot. The receiver needs
temporary disk for each concurrent round plus its durable copy; it never holds the complete
snapshot in RAM. A stalled receive leaves the old watermark available and unchanged.
Frame/page buffers remain bounded, but memory and disk scale with simultaneously active peers.
Measure actual event widths, snapshot objects, recovery time and disk growth (#3425).

**Expiry condition.** Replace replicate-everything when the topology needs hubs and spokes:
hubs such as a desktop and a cloud host replicate everything, while spokes such as laptops
hold enough state to work offline. At larger scales, partition writes and replicate read
layers. Those shapes, placement policy, selective replication, topic filtering, archives,
and filtered observation consumers are out of scope. The current open topic codes reserve
room for later partitioning without implementing it now.

## Verification

Two/three-store scenarios cover snapshot/tail/resume, expunge-floor rollback and resnapshot,
whole-home identity/body distribution, stalled and invalid watermarks, and non-default topic
preservation. Hegel generates partition/resume sequences and snapshot row
shapes across all nine fixture kinds, including frozen objects and nested records. WAL writer/read concurrency and snapshots crossing identity-page/cache bounds are also covered. The real
Unix HTTP client is tested against a strict in-process request stand-in; duplex I/O tests the
listener-independent response adapter. SQL tests execute the DDL with foreign keys and read
every view. Rust conformance exercises the unchanged stream module's prefix watermark rule;
`quint test specs/primitives/tests/stream_properties.qnt --backend typescript` checks its
finite property directly. No Quint spec edits were needed.
