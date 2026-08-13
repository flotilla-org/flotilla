# Plane A retires: rank dies with polling, and the store keeps what survives disconnection

**Status:** Accepted
**Date:** 2026-08-13
**Relates to:** the roadmap (phases 3–4, which this ADR schedules and
sharpens), ADR 0025 (store authority and federated actuation — the admission
model that made "leader" unnecessary), ADR 0029 (demand-bound observation —
the direction on-demand reads align with), #1432 (external-provider
decoupling), #1457/#1458/#1459 (the cull slices), #885/#1169 (the original
single-poller rationale that became follower mode).

## Context

Plane A — providers → correlation → WorkItems → snapshots → the repo page —
was frozen (roadmap phase 0) while the control plane grew underneath it. By
2026-08 the straddle had stopped paying: the operator had not initiated
Plane-A work for a week, convoys carried all real work, and the repo page
went unread. The straddle's cost surfaced concretely when governor placement
(ADR 0030) hit `this host runs in follower mode; dispatch from a leader
host` — a refusal whose rationale, on inspection, belonged entirely to the
frozen plane.

The `follower` flag suppressed construction of *all* external service
providers on non-leader hosts. That single gate conflated three orthogonal
things: continuous polling (the #885 don't-poll-GitHub-three-times concern —
a Plane-A observation need), on-demand external reads (one API call at
dispatch time), and admission (already leaderless: ADR 0025 puts each convoy
on its admitting host's store, single writer per store).

## Decision

**Plane A is deleted, not merely frozen, in a ruled sequence; "leader" and
"follower" are retired as concepts; and the store's boundary is fixed: it
keeps what must survive disconnection, and nothing whose truth is the live
link.**

### Rank dies with polling

Hosts differ by **capability, not rank**. External service providers are
constructed on every host whose environment supports them (credentials,
binaries); a host that cannot is reported by the missing capability's name,
never by rank. On-demand reads — issue resolution at dispatch, PR state —
work anywhere (aligned with ADR 0029's demand-bound direction). Continuous
polling remains singleton only until the observer deletion removes polling
altogether, at which point the `follower` flag evaporates. (#1432, landed.)

### The deletion sequence

1. **Repo page** (#1457, landed): the interactive WorkItem surfaces leave the
   TUI; resource-fed project/query tabs are the surviving surface.
2. **Observer pipeline** (#1458): correlation/union-find, `WorkItem`, the
   `ProviderData → WorkItem → Snapshot` pipeline, provider polling, the
   last WorkItem-consuming CLI commands (`repo detail`, `repo work` —
   operator-confirmed), and the follower flag. Subsystems with mixed
   consumers (attachables, RepoModel) are audited and split, not bulldozed.
3. **Peer-merge** (#1459): the PeerData snapshot-merge subsystem goes; the
   shared socket transport stays, carrying overlay resource replication — the
   precursor shape for the Tender extraction, which remains out of scope
   until boundary-proven.

### The store boundary

**If it must survive disconnection to be useful, it is a resource; if its
truth is the live link, it is transport-state.** Host facts — capacity,
floors, adapter availability — are resources (they feed disconnected
admission and replicate with bounded staleness). Peer connectivity, dial
direction, and routing are link-state, consulted live at the point of use
and never stored: a replicated "connected" record is exactly the stale claim
the settlement discipline (ADR 0017) exists to distrust.

### Deletion contracts escalate

Every cull contract carries a binding escalation rule: discovering a live
surviving-plane consumer of something slated for deletion is an escalation —
stop that piece, report the dependency path, continue with the rest. No
shims, no improvised re-homings. **An incomplete deletion is acceptable;
breaking used surviving-plane behavior is not.** (#1457 validated the rule
immediately: two live dependencies found, reported, preserved.)

## Consequences

- Dispatch, and therefore governor placement (ADR 0030), is host-agnostic;
  provisioning a host is adding capabilities, not conferring rank.
- The TUI becomes a pure Surface over resources and the view-model layer;
  the extraction path in the roadmap's phase 6 loses its Plane-A
  entanglement.
- The peer transport survives as deliberately unglamorous plumbing with a
  clean link-state interface — what the Tender inherits later.
- Store surgery residue (stale self-origin replicas, abandoned finalizers —
  the #1422 lineage) shrinks as the merge-era write paths disappear; the
  raw-delete recovery path stays as the operator's sharp tool.
- The roadmap's phase 3–4 ordering is now enacted rather than aspirational;
  correlation returns, if ever, as an on-demand Aggregator utility over
  observed resources — not as a pipeline.
