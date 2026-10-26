# Change stream schema checkpoint (#3463)

Status: implemented after the [owner's schema approval](https://github.com/flotilla-org/flotilla/pull/3465#issuecomment-6252741558).
Whole-home replication currently requires a full mesh: every host dials every other
host. Each follower covers its directly connected peer's own home log only; a home that
is not a direct neighbour is not replicated. There is no relaying of non-neighbour homes.
Each home's log is the partition.
Fine-grained filtered views belong to read-only observation consumers and are out of scope.

The approved DDL is in `shared.rs`, installed by store startup, with readable views synchronized
on open like the kind views. The temporary `stream_schema` API and declaration macro are gone.
No resource kind's serialized shape changes; no out-of-repo resource authors need updates.
The log Event adds a coarse code, defaulted for the previous stored generation. Quint is unchanged.

`stream::serve` emits bounded ADR 0063 framed CBOR through an injected sink. It scans identities
in indexed pages and loads each resource through the existing snapshot API. A resource,
including all child rows and referenced frozen documents, normally occupies one frame. Only a
resource whose encoded snapshot exceeds a frame uses `ResourceChunk` frames. Those chunks are
split from the loaded logical rows, without a second SQL traversal, and assembled before the
same `restore()` call. There is no separate child-row install path. The follower installs a complete snapshot and each subsequent delivery
round transactionally. A disconnected round rolls back to its last committed prefix.

The listener-independent `Service` owns the new store. The daemon exposes
`GET /apis/flotilla.work/v1/change-stream[?after=<home_seq>]` on the existing resource HTTP API.
Its chunked response sends a snapshot when needed, tails ordered frames, and sends idle
keepalives. Peers request finite rounds every five seconds, with a 15-second
frame-production deadline and a 20-second network read deadline. Both transports share
one pool of 16 blocking workers and one current session per peer; each round opens
one read-only WAL connection, reads `changes_since` pages, encodes them, and closes the
connection. A brief writer barrier before the read cut waits for the transaction currently
holding the mutex; it does not impose FIFO order on queued writers. The writer lock is
released before delivery. The outgoing channel holds at most two frames; the frame limit is the log profile's 16 MiB plus 1 KiB envelope room.

The follower stages each HTTP round in an automatically deleted temporary file, validating
frame bounds as bytes arrive. Only after a complete response ending in a keepalive does it
acquire the store mutex and replay that local file in one transaction. A disconnect discards
the staging file without changing rows or progress. Disk staging avoids buffering a complete
snapshot in memory. Its disk cost is one round per connected home; snapshots can be much
larger than tails. There is no whole-round byte cap: provision temporary disk for the largest
home snapshot at each simultaneously active peer (see the workload note below). The network
never waits inside `with_store`. Resident committed watermarks have a separate lock, so gates can read the old safe watermark during both network stalls and
snapshot installation. Lookup overlays follow the existing transaction lifetime; the per-row snapshot journal flush is removed.

`GET /apis/flotilla.work/v1/record?kind=<code>&name=<encoded-name>` serves one home point read;
production `read_record` routes one local miss there and marks it remote with its home-time
age. Gates receive no remote transport capability.

Peer connection generations own their follower tasks. Homes with forwarded resource sockets
prefer HTTP; homes reached through command routing use `QueryChangeStream`. A socket
connection failure or HTTP 404 falls back to a fresh routed round from the committed
watermark, replacing the abandoned cursor. Every later round tries the socket again;
there is no sticky transport state within a connection generation. Mid-body, framing,
decode, spool-write and other HTTP status failures remain errors without fallback. To authenticate
HTTP without treating the forwarded Unix socket as an interactive user, every peer first
opens a round through peer-only routed dispatch. The returned opaque session token is a
capability carried in the `x-flotilla-change-stream-session` header over the forwarded
socket. HTTP resolves its originating peer and continues that same producer; it cannot
open a worker of its own. The follower stages the first routed frame before the HTTP
continuation. An empty tail ending in that first frame needs no HTTP request. Bearer,
browser-session and environment callers are refused even with a token; an ordinary
socket caller without a peer-issued capability is refused too. Tokens are never sent in
URLs, and expire with the cursor. A routed session
retains one consistent read cut and pulls one base64-wrapped CBOR frame per request, applying
backpressure independently of the daemon event broadcast ring. Two bounded channels each
hold at most two frames. A producer or abandoned session expires after 15 seconds without
consumer progress. At most **16 sessions/workers** may exist at once, protected by a
semaphore permit held until the blocking worker actually exits, rather than only until its
cursor disappears. This leaves capacity in the daemon's default blocking pool for other
SQLite work. A single originating node holds one current session: a fresh round replaces
its abandoned cursor immediately, and another node cannot use its cursor. The replaced
worker retains its permit until it exits, so the global cap, not the per-peer rule,
bounds workers. Routed pulls wait 20 seconds, giving the producer's 15-second frame
deadline a five-second round-trip allowance; producer errors reach the follower unchanged.
Dropping a query dispatch removes its pending acknowledgement, including cancellation
while sending the request. Completion and delivered errors remove the session immediately.
Interactive query dispatch refuses `QueryChangeStream` before routing. The destination
also refuses forwarded requests carrying a surface session or delegated caller, and the
ordinary daemon query entry point cannot serve it. Only the internal replication dispatch
path sends the peer-owned query with neither surface field. The destination binds the
cursor to the originating node carried by the trusted peer envelope. This inherits the
mesh's existing trust in connected daemons; every admitted home is allowed to replicate the
whole log, with no per-kind ACL or topic filtering. Pinned router rows cover local/remote
user and crew refusal, and destination-origin rows cover forwarded surface/caller refusal
and peer admission. A routed-only snapshot/tail test goes through both production routers.
The receiver uses the same disk spool and production Follower as HTTP. Lost responses expire
the session and the next round resumes from committed progress. Displacement/disconnect
cancels the matching follower task. Failed links resume from committed
progress. The persisted watermark row, including zero, identifies a completed baseline after
restart. Home point reads retain every connected home, including routed-only homes, and reuse
HTTP clients for those with sockets. A missing socket or failed read yields an explicit
`home unreachable` if the search finds no value; only an entirely successful search
returns absent. The engine propagates that error instead of treating it as an absent
message. Routed point-read queries are not introduced. The old replicator is unchanged. No decided store is opened lazily: before a kind
migration installs its registered Service with `install_decided_store`, point reads and
peer cursor creation refuse service and no followers start. #3317 installs the first kind before peer tasks start and verifies
the complete production path. Tests exercise the generic mechanism before that integration.

## Stream health and unservable resources

Each decided follower reports its last committed sequence, last error and consecutive
failed rounds through the existing replication-health projection. Host status includes
`change_stream`; fleet replica failures identify the home, committed watermark and error.
Successful follower rounds clear the error and failure count. Connection generations retain
the last state until a new round completes; restarting a task does not hide a stuck home.

Before emitting exceptional row chunks, the sender encodes them once for validation and
counts their cumulative framed bytes against the follower's 64 MiB assembly budget. A
second pass sends the validated chunks, retaining at most one encoded row at a time.
An unencodable row or excessive assembly produces `StoreError::ResourceLimit`, naming the
home, kind and resource. This does not cap an entire snapshot or its disk spool. The home
reports these refusals in its own health; successful tails advance its sequence without
clearing an unservable-snapshot error. A successful full snapshot clears that error.
If all peers keep tailing after the resource is removed, the source alarm remains until a fresh snapshot is requested;
tail success alone cannot prove snapshot servability. The alarm remains in host
status and never creates an unreachable replica for the local home.
HTTP continuations encode producer failures as a terminal `Failure` frame so the follower
sees the same reason as routed dispatch, rather than a generic broken-body error. Failed
rounds never commit progress. These are query/wire types, not stored resource changes;
the protocol fingerprint changes with the query health fields.

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

**Expiry condition.** The full-mesh requirement (every host dials every other) remains
in force until this redesign; do not deploy a hub-and-spoke topology with the current
whole-home follower. Replace replicate-everything when the topology needs hubs and spokes:
hubs such as a desktop and a cloud host replicate everything, while spokes such as laptops
hold enough state to work offline. At larger scales, partition writes and replicate read
layers. Those shapes, placement policy, selective replication, topic filtering, archives,
and filtered observation consumers are out of scope. The current open topic codes reserve
room for later partitioning without implementing it now.

## Verification

Two/three-store scenarios cover snapshot/tail/resume, expunge-floor rollback and resnapshot,
whole-home identity/body distribution, stalled and invalid watermarks, and non-default topic
preservation. Hegel generates partition/resume sequences and per-resource snapshot
shapes across all fixture kinds, including frozen objects and nested records. WAL writer/read concurrency and snapshots crossing identity-page/cache bounds are also covered. The real
Unix HTTP client is tested against a strict in-process request stand-in; duplex I/O tests the
listener-independent response adapter. SQL tests execute the DDL with foreign keys and read
every view. The `flotilla-schema-sqlite` `quint_replay` target feeds the production Follower from the
same generated ITF stream prefixes as the engine replay, comparing stored rows and watermark
after every step, including disconnected apply attempts. The old Rust restatement of
`stream_properties.qnt` is deleted. No Quint spec edits were needed.

### In-process frame measurements (#3479)

The ignored `workload_ceiling_resource_frames` contract run uses WORKLOAD.md's 10,000 running
convoys and a 100 KiB frozen charter document per convoy, with the fixture's child records.
It restores and compares every resource: **10,000 resource frames, 10,003 total frames,
102,852 bytes largest frame, 61.03 seconds** in the dev profile. This measures convoy count
and upper convoy width, not all resident entity kinds or 500-home mesh throughput; the
existing topology expiry above still applies.

Resource sizes are not a hard schema bound: children can accumulate across individually
bounded writes. `oversized_resource_uses_chunks_and_restores_atomically` constructs 1,024
child strings of 17 KiB using 32 bounded updates. Its 17 MiB of child payload exceeds the
16 MiB frame profile, so only that resource uses row chunks. The measured exceptional case emits **1,025 chunks**, with a **17,547-byte** largest frame.
Both sender and receiver hold
one complete resource in memory; exceptional chunking bounds wire frames, not total
resource materialization. The follower also caps a resource's cumulative framed chunks at
**64 MiB**, rolling back when that budget is crossed; decoded values have additional
representation overhead. This is far above the root workload's 100 KiB upper convoy width
and above the measured 17 MiB exception. A real resource reaching that budget expires this
assembly design and requires retention or incremental restore. Only aggregate CBOR message
or element limits trigger chunking; malformed values, byte-string and depth errors fail
before any chunk delivery. A disconnected chunk sequence rolls back atomically. If
append-only resources routinely reach this size, revisit resource retention and incremental
snapshot construction rather than applying per-row framing to every resource.

Routed delivery adds one command round trip per frame. It trades extra recovery latency for
a bounded consumer and consistent cut without a second bulk transport. At the measured
convoy ceiling this is 10,003 round trips for a fresh home; steady-state tail rounds are
proportional to new events plus one keepalive. This remains the existing small-mesh shape,
not a claim of viability at the root workload's 500-host ceiling.

The five-second cadence is the idle delay between successful rounds, not a joining-snapshot
latency guarantee. Per-frame pulls are deliberate for this first small-mesh transport: they
retain bounded backpressure and the existing atomic round install without an additional
bulk channel or retained partially spooled transaction. At 20 ms routed RTT, 10,003 requests
add about 200 seconds before encoding and disk cost; at 100 ms, about 1,000 seconds. A failed
request discards the partial spool and resumes from the last committed prefix, so a joining
snapshot may restart. Batch delivery or resumable staged snapshots should replace this
choice when WAN joining latency or frequent link failure makes that cost unacceptable.
A whole-round timeout would also reject sufficiently large but steadily progressing homes;
the 15-second idle deadline instead bounds a stalled consumer while permitting progress.
