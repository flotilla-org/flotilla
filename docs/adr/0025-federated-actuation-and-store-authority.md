# Federated actuation preserves store authority

**Status:** Accepted
**Date:** 2026-07-29
**Relates to:** [ADR 0016](0016-overlay-replication-for-cross-root-state.md)
(overlay replication), [ADR 0023](0023-fact-history-is-evidence-never-truth.md)
(current facts and freshness), [ADR 0024](0024-declared-state-machines-and-field-ownership.md)
(writer roles and enforcement), [#1188](https://github.com/flotilla-org/flotilla/issues/1188)
(the contract grill), [#1257](https://github.com/flotilla-org/flotilla/issues/1257)
(bidirectional replication over one connection), and
[#1179](https://github.com/flotilla-org/flotilla/pull/1179) (the facts-merge
pattern).

## Context

A convoy admitted on one host can place vessels and checkouts on another. The
admitting leader is the only host that can resolve leader-side inputs such as
issue snapshots, but the placement host is the only host that can resolve and
actuate against its local environments. Reading the placement host's environment
from the admitting store therefore consults the wrong authority.

Moving the convoy bundle to the placement host was considered and retracted. It
would make a convoy's ownership depend on one placement host, which does not
survive multi-repository and fork-DAG workflows, and would leave every cross-host
verb needing bespoke routing. Federation instead lets placement intent move while
each store retains authority over what its local writers observe.

Source: [the retracted ownership-routing proposal](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5102291187)
and [the ruling that replaced it with federation](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5102315713).

## Decision

### Convoy ownership stays with the admitting leader

The convoy and its pinned snapshots live in the admitting leader's store. Inputs
available only during admission — issue snapshots, the workflow snapshot, and
grant resolution — are pinned there as content. Placement hosts do not rerun
leader-side providers to reconstruct them.

Source: [the federation-slice ruling](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5102315713).

### Placement hosts actuate replicated-in placement intent

Convoys and their placement resources replicate through the overlay. Reconcilers
on each placement host watch replicated-in vessels and checkouts, select the
resources placed on that host, and actuate them against that host's local
environments. The admitting host does not reach across stores to resolve a remote
environment.

This placed-on-me reconciliation path is implemented by
[#1249](https://github.com/flotilla-org/flotilla/pull/1249).

Source: [the federation-slice ruling](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5102315713)
and [the implementing PR](https://github.com/flotilla-org/flotilla/pull/1249).

### Stores write only their own records; views merge facts

An actuator never writes status into an object in the owner's store. Each host
appends its observations to its own store and log. Consumers merge the owner's
specification with the actuator-authored facts when building a view.

This follows the pattern landed in [#1179](https://github.com/flotilla-org/flotilla/pull/1179):
a remote host's self-report and a local observation remain source-qualified facts,
and the aggregator combines them without collapsing one into the other.

Source: [the single-writer ruling](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5102315713)
and [the #1179 merge pattern](https://github.com/flotilla-org/flotilla/pull/1179).

### Owner fields and actuator fields are separate

The admitting store is authoritative for:

- spec;
- metadata, including pin annotations;
- deletion intent; and
- the phase roll-up.

Phase transitions are decisions derived from merged facts, with the owner's
reconciler as the sole decider.

Each actuating host is authoritative in its own store for:

- vessel, checkout, and work status;
- session liveness;
- integration conditions and landed evidence; and
- crew completion claims.

These are per-host fact streams merged by the aggregator. In
[ADR 0024](0024-declared-state-machines-and-field-ownership.md), this contract's
store-level split is refined into static writer roles: operator-authored,
loop-derived, and actuator-observed. In particular, owner-store authority does not
make all its fields operator-authored; the phase roll-up is owner-loop-derived.

Source: [the ruled field-ownership split in #1188](https://github.com/flotilla-org/flotilla/issues/1188).

### Owner-offline actuation uses last-known state, except for teardown

When the owner is unreachable, actuator hosts continue provisioning and crew work
from the last replicated convoy state. Idempotent actuation and level-triggered
reconciliation make later catch-up safe.

Teardown and deletion freeze until the owner becomes reachable. Owner
unreachability makes the evidence too stale for those destructive transitions.
Actuator facts may continue to accumulate while the owner is offline, but the
owner-decided phase roll-up and its display can lag until reconnection.

Explicit ownership takeover, including handoff from a laptop owner, is a future
contract and is not decided here.

Source: [the offline-semantics ruling](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5104044825)
and [the ruled field-split consequences](https://github.com/flotilla-org/flotilla/issues/1188).

### One established host channel carries replication both ways

Once two hosts have an established connection, that channel carries both
directions of resource replication, regardless of which host dialed. Both sides
offer their resource API over the same session, and each side's replicator can
pull as though it had opened the connection.

Symmetric dialing, an N-choose-2 set of SSH connections, and follower-side peer
address books are not part of the solution. Retiring the follower setting remains
a question about roles — who admits and who leads — rather than connection
direction.

Source: [the 2026-07-29 transport ruling](https://github.com/flotilla-org/flotilla/issues/1257#issuecomment-5116591866).

### Connectivity topology does not assign control-plane roles

The expected topology includes always-on home hubs and disconnectable hosts such
as laptops. A disconnectable may dial a hub when it wakes, and a future public
forwarder at `flotilla.work` may act as rendezvous. These dial paths establish
connectivity only: replication still flows both ways over the established channel,
and admission and leadership roles remain orthogonal to who dialed.

The forwarder is a topology note, not a protocol or deployment design in this ADR.

Source: [the hub, disconnectable, and public-forwarder topology ruling](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5116631543).

## Consequences

- Placement intent can be owned on one host and actuated on its placement host
  without transferring convoy ownership or adding verb-specific remote routing.
- Store conflicts are avoided by construction: owner decisions and per-host facts
  have distinct authorities, and merged views do not become authoritative writes.
- A disconnectable owner does not halt non-destructive work already known to
  actuators, but it does halt teardown and deletion and can leave the phase display
  stale.
- Connection direction may follow the practical topology without constraining
  replication direction or control-plane roles.

These consequences restate the rulings above; they do not add merge precedence,
takeover, forwarding, or conflict-resolution semantics.

## Deferred, not decided

Explicit ownership takeover remains future work. So does the recorded possibility
of running delegated workflow subgraphs inside a vessel with a daemon subset. That
possibility fits the actuator model but does not change this contract.

Source: [the offline-semantics ruling's takeover note](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5104044825)
and [the in-vessel-subgraph note](https://github.com/flotilla-org/flotilla/issues/1188#issuecomment-5106930318).
