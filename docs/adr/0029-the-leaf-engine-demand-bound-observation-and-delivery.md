# The leaf engine: one event-delivery substrate over demand-bound observations

**Status:** Accepted
**Date:** 2026-08-04
**Relates to:** ADR 0027 (condition leaves, the single subscription surface —
this ADR builds its engine), ADR 0028 (world-terminal exits and vessel-as-cache
— the consumer contracts), ADR 0021 (Landing/Landed writers, unchanged),
ADR 0024/0025 (declared state machines; store authority — the federation frame),
#1322 (the grill whose rulings this records; its comment thread is the ruling
trail), #1340/#1321 (the incidents and Bosun runs that supplied the evidence),
#1368 (the store-representation revisit this ADR feeds). Enacted as PR #1365
(evaluator core + blocking `wait`), PR #1366 (observed ChangeRequests +
refresher), PR #1374 (ReconcilerWake), PR #1370 (authority-side observation).

## Context

Two Bosun runs (#1321) measured what driving a workflow by polling costs: 83%
of a resident agent's wall time was sleep between `gh` polls that benefited
only itself, and every driver, reconciler, and script re-derived the same
facts separately. Meanwhile the 2026-08-02/03 settlement incidents showed the
lifecycle machinery evaluating conditions by probing remote checkouts from the
wrong host. ADR 0027 had already named the shape of the fix — one condition
table, leaves as the only condition language — but not its engine. The #1322
grill ruled the engine; three slices and the authority-side observation fix
shipped it inside 24 hours; six convoys then settled autonomously within
seconds of deploy, the first machine-observed settlements in the fleet's
history.

## Decision

### A leaf is a comparison, exactly

One serializable, three-valued comparison against one stored record:
`(record address, field path, operator, bound literal)` →
`True | False | Unknown`. Unknown is structural — absent record, Unknown
observation, or staler than the row's freshness demand — and triggers
nothing. Cross-record references (a claim's timestamp for postdating, a prior
head SHA) are **bound at subscription time** and frozen into the row as
literals, so evaluation stays single-record and pure. No connectives, joins,
or counting: `wait` ORs leaves; everything else composes in the caller's own
language (ADR 0027, unchanged). A leaf is data — the same form serves
`wait --for`, reconciler wakes, standing rules, and ADR 0028's exit tables.

### Evaluator and refresher are separate halves

The **evaluator** is pure over the resource store, driven by store watch
events. The **refresher** is demand-driven: subscribing a leaf is what causes
its backing observation to be maintained — no subscriber, no polling.
Cadences are tunables tightened by the max freshness demand among live
subscriptions; push transports (webhooks) are a later refresher, not a new
engine. Postdating is a freshness demand the evaluator places on the
refresher. Leaves never touch external systems.

### The admitted vocabulary is closed

An unknown path is a loud admission error at subscribe time, never an
Unknown-forever row — the worst failure mode this system could have is a
typo that silently never fires. The evaluator lives behind a `LeafSubject`
seam (kind + path → three-valued value), currently implemented as a
hand-written per-kind table — the second such table beside ADR 0024's field
ownership. **Whether a shape-descriptor (facet) replaces the table family is
explicitly undecided**: a measured spike (PR #1372, parked open) found the
mirrored-attribute surface, a +64% incremental check cost, and a working
value-free schema; the reject recommendation is contested (the
mirrored-attribute objection is circular if a serde exit is the destination,
and reflection demand is a trajectory, not a snapshot). The seam contract is
identical in both worlds; #1368 inherits the decision.

### Observed subjects: binding creates records, browsing never does

The v1 vocabulary: machine leaves (`convoy.phase`, `vessel.phase`,
`work.phase`), claim leaves (`work.latest-claim.disposition`, `.claimed-at`),
checkout integration conditions, and a first-class **observed ChangeRequest
record** — subject-keyed (`cr/<service>/<scope>/<number>`), carrying `state`,
`head_sha`, `checks`, `review.actionable_at_head`, `mergeable`, each with
`observed-at`.

An observed subject becomes a record only because a live subscription
**bound** to it. Collections and browse — "find the PR for issue N", board
queries, anything unbounded — are served by the on-demand Aggregator and
never materialize records; leaves may not address collections. Observed
records carry three riders: **demand-scoped GC** (a record dies when its last
subscription ends — resources without a death story accrete, per the
checkout-orphan incident), **coalesced writes** (persist on change, never per
poll pass), and **thin event history** (peers must not pay an event-replay
tax for polling). Steady-state cardinality is live convoys × bound subjects,
GC'd at settlement.

### One table, two watcher kinds, zero persistence

The single subscription table's rows are
`{leaves (OR-set, literals frozen), watcher, freshness demand, created-at}`
plus reserved episode-identity keys. v1 watchers: **WaitCaller** (a blocked
`flotilla wait` connection; block-only per ADR 0027's suspend ladder — the
row dies with the connection) and **ReconcilerWake** (a parked convoy's
lifecycle evaluation re-runs when an armed leaf fires; rows are derived,
recomputed idempotently from convoy phase at boot). **TurnDelivery —
ensure-flavored delivery of agent turns on wakeup — is deliberately deferred**
to the delivery-ladder work; resident drivers (the Bosun) remain the
sanctioned stopgap for judgment-shaped rounds. No row is persisted: wait rows
are intentionally ephemeral, reconciler rows are derived state.

### Per-host engines; observation runs at the record's authority

Each daemon runs the engine for its own authoritative convoys and wait
callers. Observed records follow store authority (ADR 0025): the first
demanding host creates the record and runs its refresher; everyone else reads
replicas. **Integration observation runs on the checkout's authority host**
(the host that owns the path, environment, and credentials), triggered by any
replica of the owning convoy being in Landing; the convoy-authority host
evaluates stored evidence only and never probes remotely (PR #1370 — the fix
that produced the first autonomous settlements). Capability loss at the
observing authority degrades to staleness → Unknown → convoys hold, visibly.
**Invariant: every kind the leaf vocabulary can address must be in the peer
replication set.**

## Why the weak shape

The verb-surface-plus-leaves design is a weakness-maximization argument in
Bennett's sense (*The Optimal Choice of Hypothesis Is the Weakest, Not the
Shortest*, arXiv:2301.12987): a workflow formalism with its own control flow
is a strong hypothesis about what workflows are, over-fitted to the loops its
author imagined — the shape #1283 falsified empirically. Leaves-as-data over
a verb surface is the weakest hypothesis covering the observations, which is
why a bash loop, a restricted driver agent, and the lifecycle reconciler
turned out to be the same kind of thing. The standing test for future
formalization pressure: *does the proposed step widen or narrow the
extension?* Declared exit tables widen (data naming observations, any driver
carries them); a workflow DSL with control flow narrows, and gets rejected on
that ground.

## Consequences

- Settlement is event-driven end to end: merge observed → record write →
  leaf fires → reconciler evaluates → `Landed`. The per-resync integration
  fetch is retired; ADR 0021's "manual post-merge sweeps end" is
  operationally true.
- Drivers stop paying the polling tax individually: `flotilla wait` replaces
  sleep loops, and the refresher's cost is shared and demand-scoped.
- ADR 0028's exit tables gain their evaluation engine; their declaration
  syntax is follow-on work that consumes this ADR, not part of it.

## Deferred, with owners

- **TurnDelivery** — with the delivery-ladder work (ADR 0028's ladder); Bosun
  run 3 is its validation vehicle.
- **Exit-table declaration syntax** — follow-on to ADR 0028, small grill.
- **Webhook refreshers** — a transport upgrade inside the refresher.
- **The evaluator-substrate ruling** (declared tables vs facet descriptor) —
  parked on PR #1372's contested record; #1368 sequences on it.
- **Anchored's entry/exit wiring** — same table, pre-claim leaves; lands
  when a real mid-work wait consumer appears.
