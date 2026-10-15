# PM connector recovery scopes

Issue #3244 keeps the current catalog projection until #3245 moves it into
flotillad. It changes recovery work, not the catalog's query cover or storage.

## Workload

The root `WORKLOAD.md` sizes delivery at 1,000 sustained / 10,000 burst events
per second and 50 / 500 view clients (target / ceiling). The connector holds
six named-query cursors, including awareness, and eight resource kinds per
discovered namespace. Its catalog is a rebuildable materialized view; durable
records and the stored golden corpus are unchanged.

A named-query gap now costs one recovery request and at most that query's full
set. Broadcast lag costs one request carrying all current cursors; only stale
queries replay. A resource-watch gap costs one replacement watch snapshot for
its kind and namespace, with no query replay or sibling watch restart. At 500
clients, one common query gap means 500 single-query replays rather than 500
replays of every query plus every resource scope. No durable read is added to
steady-state event handling. These are counts of crossings, not timing claims.

Catalog projection/diff remains proportional to the held catalog, as before;
this change makes no claim that that existing cost fits the root delivery
budget. Incremental projection, resident catch-up and catalog windowing belong
to #3211/#3245, outside this recovery change. A repair still needs enough memory
for one complete source snapshot; it adds no retained history or second catalog.

## Choices

- Keep subscription replacement for initial setup and introduce `RecoverQueries`
  for selected replay. Replacing interest with one query would discard sibling
  subscriptions; replaying all queries would violate the recovery scope. Recovery
  reads existing materializations and socket dispatch refuses unowned queries.
- Keep one owner task per resource watch. It reopens its own scope on lag and
  sends a complete snapshot through the existing bounded (64-message) channel.
  Waiting on this channel backpressures the owner; broadcast overflow triggers
  another scoped repair. There are no detached recovery tasks or competing
  writers of cached records. Errors other than lag retain session-failure policy.
- Apply a repaired scope only after its bookmark, clearing that kind/namespace
  first so absent records retract. The ordered record key locates that scope
  without scanning unrelated records. Queue order prevents an old update from the
  same watch following its replacement snapshot. Other kinds and namespaces stay.
- Publish through `replace`/`send_batch`, committing the projected catalog only
  after acknowledgement. Wheelhouse snapshot refusal retries to the existing
  15-second publication deadline and exits; it never changes liveness mode.
  Zellij retains TTL because its pipe adapter has no provider-owned replacement
  or connection-liveness contract. Wheelhouse has no TTL fallback.

The wire is disposable under ADR 0047: matching client/daemon fingerprints are
required. No persisted shape or out-of-repo manifest changes. The scope assumption
is enforced by `read_subject_snapshot`'s kind/namespace validation and bookmark
barrier; it must be revisited if watches begin mixing scopes. The six-query count
is derived from `ConnectorState::cursors`, not a new protocol limit.
