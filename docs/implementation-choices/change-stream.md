# Change stream schema checkpoint (#3463)

Status: proposed, awaiting owner schema review. This PR is a checkpoint, not a working
stream. `stream_schema` declares stored rows and renders DDL without installing tables or
reading/writing them. No resource serialized shape changes; no out-of-repo authors need updates.

## Readable table view

All tables are STRICT and WITHOUT ROWID. No table exceeds 30 columns. None has child tables
or expands a data-carrying enum. `disposition` is a single closed code column (`up`, `rm`);
`scope_code` and `relation` are open codes, not enums. Each table has a SQL `_view` resolving
local identity and string IDs. `stream.sql` is the generated DDL snapshot.

| Table | Columns | Primary key | Other indexes |
|---|---|---|---|
| resource_relation | 4: resource, relation, host, via | all four | host/resource; via/resource |
| resource_scope | 3: resource, scope_code, scope_key | all three | scope_code/scope_key/resource |
| change_scope | 5: home, home_seq, scope_code, scope_key, disposition | first four | home/scope_code/scope_key/home_seq |
| stream_subscription | 4: subscriber, home, scope_code, scope_key | all four | home/scope_code/scope_key/subscriber |
| stream_watermark | 3: home, version, applied_through | home | none |
| stream_history | 2: home, retained_after | home | none |

`resource_relation.via` preserves each independent source of a relation: removing one vessel
must not evict a resource still related through another vessel. A kind's derivation declares
relation codes; Message's derivation remains #3317. `resource_scope` materializes the union of
these reasons. `all` with an empty key means definitions everywhere; `host` keys name related
hosts. Project/time keys later add codes and values, without columns or migrations.

The write transaction derives relations and scope membership once, emits the event, and
persists one change tag per key in the union of old/new membership. A departing key gets
`rm`; a retained/entering key gets `up`. Entry sends a resource snapshot at that version;
resume must reconstruct that snapshot from retained history rather than attach today's body
to an older event. Removal evicts the body only. Global identity create/delete events are
sent independently of body predicates, including on otherwise empty slices.

Tags reference log entries, not live bodies; an expunge tag survives the identity it removes.
Relation/membership rows cascade on identity expunge. Log cleanup cascades its tags. The home
advances `retained_after` atomically before removing resumable history. Nothing in this PR
adds compaction or archives. A cursor equal to the floor is servable; one below needs snapshot.

Subscriptions are a union of predicates, deduplicated by subscriber. Scope changes force a
fresh snapshot; resuming a cursor under a different predicate set is forbidden. Snapshot
completion must establish a consistent cut before a watermark is published. One receiving
host combines its local gate requirements into this union; external view-client progress is
connection-local, not written over the host's watermark.

`stream_watermark` is latest observation state, outside the durable log. A keepalive carries
home time and a version proving all earlier frames were applied. Filtered-out versions still
advance the covered prefix. Stalls hold time; receipt alone never advances it. Scope expansion
invalidates coverage until its snapshot is installed; gates must treat the input as unavailable
while coverage is missing. Resets cannot claim a fresh timestamp before the snapshot commits.
The implementation must reject regressing home time and sequence. No gate reads link health.

## Workload note and candidate walk

Reference: root WORKLOAD.md (hosts 3/50/500, clients 2/50/500, events <10/1,000/10,000 per s).
These are sizing calculations, not benchmark results. Runtime measurements follow approval.

- Per write: O(derived relation changes + scope keys + matching subscribers). Derivation is
  once per write; fan-out unions resident bitmap postings using the judgement-index mechanism,
  with no subscriber predicate evaluation or durable-store read per delivery. Tags/membership
  are persisted with the write, not once per recipient. Reject per-resource subscriptions:
  maintaining them scales with resource/vessel churn and violates the ruling.
- A home normally has two host predicates per host (definitions and that host's related set),
  plus view-client predicates. At the ceiling, 500 hosts x 2 + 500 one-scope clients = 1,500
  predicate rows per home, 1,000 recipients. Additional client scopes scale with scopes per
  client, an unresolved cardinality, never with resources. 1,000-bit postings use 125 bytes per
  scope key before overhead. Reject scanning all 1,500 predicates on every event.
- At 10,000 events/s, broadcasting all definitions could require 10 million recipient
  deliveries/s. This is an output cost, not 10 million store reads or predicate evaluations.
  Runtime resources normally have fewer related hosts; that distribution is not specified.
  With 500 view clients each matching the root's 5%, views alone can require 250,000
  deliveries/s. Backpressure must disconnect slow consumers to resume/resnapshot, with bounded
  frames and buffers. Root budgets do not establish bytes/event or relation-count bounds;
  don't claim throughput from arithmetic alone.
- Joining a host needs all live identities plus definitions and its related bodies, paged by
  a fixed byte and row budget from one snapshot cut, then a tail. Worst case it relates to
  everything. Root compact resident form is ~150 MB at the ceiling; full stored bodies plus
  retained history approach ~9.5 GB typed and cannot be materialized as one snapshot buffer.
  As a live lower-bound example, 10,000 convoys + 36,000 vessels + 36,000 terminals + 500,000
  messages yield 582,000 identities. At an assumed 128 bytes/identity that's ~74.5 MB before
  retained identities and definitions. Identity width and total definition count are named
  gaps; page limits must bound memory independently of both. Reject full snapshot buffering.
- Persistent tags add one row per scope key per durable log event. At the root's ~59 durable
  writes/s/host over 7 days there are ~35.7 million log entries. Even one 40-byte tag adds
  ~1.43 GB before SQLite/index overhead; relation fan-out multiplies it. This worsens the
  already failing disk budget and must be measured under #3425 before claiming budget fit.
- Store crossings: startup builds resident scope and subscription indexes; each local durable
  write persists tags; attaching/resuming reads bounded snapshot/log pages. Fan-out after
  warm-up and watermark reads by gates use resident state. Persisted latest watermark may be
  coalesced, but a crash may only restore a conservative prefix, never move it ahead of rows.
- Evolution: new shared metadata follows the pre-production schema baseline of `shared.rs`.
  Codes are never reused. Post-cutover persisted changes follow ADR 0047. Transport will be
  ADR 0063 CBOR with strings, no interned IDs. Keep HTTP routing behind a listener-independent
  handler; leave old replication intact. These protocol/transport parts are not implemented
  in this schema checkpoint, and Quint is unchanged.

## Review question

Please review the six shared tables, especially provenance (`via`), per-change departures,
and latest observation watermark storage. After approval, implement the stream, in-memory
behavior suite, HTTP seam, routed misses, Quint conformance and ADR 0064 carry in this PR.
