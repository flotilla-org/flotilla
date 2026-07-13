# The Aggregator data plane is named-query result sets

**Status:** Accepted
**Date:** 2026-07-13
**Amends:** ADR 0005 (its wire-shape half; the composition-of-panels view model stands, but moves consumer-side)

The `Panel*` wire types (PR #664) welded three different things together: the
*query* (`PanelSource`/`PanelScope`, consumed by nothing), the *result set*
(rows keyed by `ResourceRef`, seq, delta maintenance), and the *layout*
(tab/panel ids, columns, titles — re-shipped in every snapshot and ignored by
every consumer). Issue #684 splits them.

## Decision

**The wire carries a data plane only: incrementally-maintained result sets of
named queries.**

- A **named query** (`QueryId`, e.g. `convoys`: all Convoys, durable ∪
  observed, fleet-merged, joined with Presentation attach state) is maintained
  by the daemon Aggregator as a `ResultSet { seq, rows }` updated by
  `ResultDelta { seq, changed, removed }` events
  (`flotilla-protocol/src/result_set.rs`). The query identity is **derived
  from the typed `Rows` variant** (`ResultSet::query()`), not carried as a
  separate field — a mismatched query/rows pair is unrepresentable and
  undeserializable, preserving the compile-time-safety rationale below as new
  queries are added. A removal-only delta carries an empty `changed` variant,
  which still tags the query.
- **Layout never crosses the wire.** Columns, labels, titles, and tab
  composition are consumer config; #666 designs the per-kind table layer on
  top. ADR 0005's "tab is a composition of panels" remains the view model —
  owned by surfaces, not the protocol.
- **Rows are typed per query** (`ConvoyRow` / `VesselRow`; #683 adds session
  rows as a new `Rows` variant).
- **Clients subscribe per query** (`Request::SubscribeQueries`), and the
  socket server filters per connection: unsubscribed queries never hit the
  socket.
- **Replica merge is federated query union**, stated in the types:
  `FleetReplicaSnapshot.result_sets` carries each host's *local* result sets;
  the Aggregator unions them into the fleet-merged set, stamping the origin
  host onto rows.

## Row representation: typed structs (not maps, not schema'd tuples)

Three options were on the table:

1. **Self-describing map** (`BTreeMap<String, Value>` — the PR #664 shape):
   flexible, generic consumers need no per-query knowledge; but every consumer
   pattern-matches stringly keys (`values["phase"].as_str()`), typos fail
   silently at runtime, and per-row key strings are pure wire overhead.
2. **Schema'd tuples** (column schema once per result set, positional values):
   most compact, but convoy rows nest vessel children with a *different*
   field set, so the "one schema per set" premise breaks immediately, and
   consumers zip positions back to names anyway.
3. **Typed per-query rows** (chosen): concrete serde structs per query.
   Producers and consumers are checked at compile time; phases are enums, not
   strings; nested rows are just typed fields.

Costs accepted with (3): each new named query adds wire types, and #666's
generic table layer binds per-kind rather than over an untyped bag — which it
does anyway, since k9s-style column config is inherently per-kind.

Two corollaries of typed rows:

- **Joins are visible as typed fields.** The Presentation join is
  `VesselRow.attach: Option<String>` — its docstring says what it is and where
  it comes from, where the map shape buried it inside invisibly-flattened
  `children` intents.
- **Intents collapse into capability fields.** `IntentTarget::Vessel` echoed
  namespace/convoy/vessel that the row already knows; typed rows carry only
  the capability facts (`attach: Some(_)` = the daemon will accept an attach;
  `complete_work: bool`). A consequence: a capability's target *is* the row
  identity (plus `VesselRow.host` for routing) — they can no longer diverge.

## Sequencing and recovery

- `seq` is **per query** and **contiguous**: a delta applies iff
  `seq == last_seen + 1`. A gap (or a delta for a query with no local state)
  means the client re-subscribes with its current cursors and receives a full
  `ResultSet` for any stale query. Deltas with `seq <= last_seen` are ignored
  as already covered — this makes the subscribe-vs-live-event race benign.
- `SubscribeQueries` replaces the connection's whole subscription set and
  returns the replay result sets in its response (mirroring `ReplaySince`,
  which now serves repo/host streams only).
- Delivery restriction is a transport concern: the socket server must filter
  per connection; `InProcessDaemon`'s shared broadcast may over-deliver.

## Versioning

This is an incompatible wire change (event/request tags, `StreamKey`,
`FleetReplicaSnapshot` fields), so `PROTOCOL_VERSION` bumps to 8. Peer
handshakes already enforced version equality; the client-role Hello handshake
now does too, on both sides (the server still replies with its Hello before
closing so the client can report which versions disagreed). The SSH
`replica-snapshot` path carries no version and relies on fleet hosts being
upgraded together — acceptable in the no-backwards-compatibility phase.

## Consequences

- `flotilla-protocol/src/panel.rs` is deleted; `ResourceRef` (row identity,
  k8s-isomorphic) moves to its own module.
- `QueryProjection<R>` in `flotilla-core/src/aggregator_projection.rs` is the
  query-agnostic projection core (local + per-replica rows, seq); the convoys
  query instantiates it, #683's sessions query is the next instantiation.
- The TUI's `convoy_model` adapter is a thin typed mapping; all stringly
  `values.get(...)` lookups are gone.
