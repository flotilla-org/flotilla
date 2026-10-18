# Inbox reference implementations

The [component workload](../../crates/flotilla-inbox/WORKLOAD.md) records the
operations, inherited scales, arithmetic, bounds and expiry of the reference
implementations from #3316.

The open seam is InboxBackend: foreign backends implement its operations
without inheriting the engine, SQLite or the reference implementations' indexes.
For the two local implementations, one Home decides Message edges and its Log
persists them. The SQLite StoreLog promotes the schema integration test's
insert/apply-edge transaction boundary; no table or stored kind changes.

A per-inbox BTree index ordered by admission sequence serves pending reads. A globally bounded journal serves
watch deltas. A separate channel per subscriber would multiply backlog by the
root ceiling of 500 clients; scanning history per cursor would make successive
pulls quadratic. The journal's indexed ranges and shared payloads avoid both.
At 10,000 changes/s its 65,536-entry default bridges about 6.5 seconds. A slow
reader gets CursorExpired rather than silently missing changes. After expiry,
establish a live watch (no cursor), then read the pending baseline and continue
that watch. Both result streams materialize one item per pull.

A single writer lock spans each engine decision and its persistence. Tokio
blocking workers own that work, behind 64 permits, so SQLite does not block an
async executor thread and the blocking pool's submission queue stays bounded.
The 64 permits accommodate the root's roughly 59 budgeted durable writes/s with
about one second of admission backlog; callers beyond that wait asynchronously.
Notifications are registered before looking for a change. A bind that finds an
observed receipt waits outside the writer and retries after delivery.

Binding sets and gate evidence are home-local observations. Receipt ports must
commit Receive on the named consumer's resource before reporting success. Mode
rules are ordered, use conjunctive optional selectors, and default to Turn.
Dead letters have deterministic identities derived from their source message,
so recovery can finish a reaction interrupted after the source drop. An explicit reaction-provenance set, rebuilt from dropped sources on recovery,
forces Notify for those dead letters; an ordinary system message still
uses the caller's rules. The supervision port supplies the declared system role,
leaving real system-sender mapping to #3317.

The no-production-caller assumption is enforced in ci/build-graph/check.py.
Home's full resident resources and history are a deliberately smaller reference
query model: at the inherited ceiling they exceed the daemon budget. This
choice expires before the #3317 production switch. The open trait, streamed
results, and explicit expired-cursor outcome allow compact open-state views,
indexed cold reads and archival without a public contract migration. The
contract and its conformance suite remain the acceptance boundary for that work.

SQLite consumer ports identify their shared Store/home scope. A port on another
store is rejected before Receive rather than allowing its event to reuse the
inbox's unchanged cursor. Publication also enforces strictly increasing cursors.
Repeated Drop keeps the machine's Ended refusal, even when it repairs a pending
reaction: it never records a second Message edge. Construction uses fallible open;
synchronous registration/inspection are setup/diagnostic only, with async worker
variants available for runtime callers. Poisoning the writer requires reopening.

Quint parsing, graph traversal, replay helpers and Message value/evidence mapping
live once in the dev-only flotilla-quint-testkit. The engine adapter retains
full-graph conformance; inbox adapters replay only message_race through the
backend trait. Both run from specs/primitives/check.sh in the existing Quint
job, without a separate runner or workflow change.

SQLite watch recovery reads historical typed state by applying recorded store
effects into a temporary in-memory store, using the same registry. It does not
interpret Message edges. This adds a temporary full-history store at startup;
the resident query model, watch journal and worker permits are replaced by
#3466 before production use.
