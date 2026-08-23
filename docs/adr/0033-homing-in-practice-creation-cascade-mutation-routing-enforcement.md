# Homing in practice: creation cascade, mutation routing, and enforcement

**Status:** Accepted
**Date:** 2026-08-21
**Relates to:** ADR 0016 (overlay replication — this ADR amends it from
operational evidence), ADR 0032 (convoy identity — its admission and
actuation-locality rulings become this ADR's two homing seams). Incident
provenance: #1593 (one logical `ConvoyEnsure` independently stored at all
three roots, quarantining every daemon under one schema change; #1592 is
the refusal-opacity side of the same incident), #1594 and its 2026-08-21
escalation comment (terminal records unaddressable; a raw single-root
delete of `Convoy/command-builder` resurrecting within hours via
re-federation). Two further observations from the same period
(2026-08-18 to -21) have no issue of their own and this ADR is their
record: placement policies and snapshots listed two and three times in
`resource list`, and dispatch refusals from different roots naming
different candidate sets — kiwi's admission error omitted three records
(among them the feta-authored crew-image policy) that feta's own error
listed, because admission lists placement policies through the local-only
read (`using::<PlacementPolicy>` at `in_process.rs`), not the
include-replicas view.

ADR 0016 ruled the cross-root model: convergent facts union, definitions
merge, home-bound runtime is reconciled only at home. One week of running
the fleet through daemon restarts, a schema change, and a generation roll
produced the incidents above — none falsified 0016. Every one was the
implementation failing to hold an invariant 0016 assumed by construction.
This ADR closes the gap: who authors a new record, how mutations reach a
record's home, what reads are required to see, and what enforces all of it.

## Records are authored at one home; practice now enforces it

For home-bound runtime kinds, a record `(namespace, kind, name)` has
exactly one home root; only the home's copy is authoritative; every other
root holds a read-only, staleness-stamped replica (0016 restated). What is
new is the standing: **same-name authored-here records at two roots are a
defect to surface, never inputs to merge.** The week's duplicates
(placement policies ×3, `command-builder` resurrecting after a
single-root raw delete) all trace to multi-authoring that nothing
detected.

Convergent facts and definitions stand exactly as 0016 ruled them —
union for facts, field-merge for definitions. Nothing here narrows them.

## Creation: authorship cascades from the parent

The gap 0016 left open: who authors a *new* record when several roots
could? The observed fact (#1593): after one schema change, every root
held an undecodable stored copy of `ConvoyEnsure/governor` in its own
store — clearing them took per-root store surgery, and the sets differed
(udder also held `ConvoyEnsure/andamento-governor`). Independent per-root
authorship is the inferred mechanism rather than a narrated one: the
materializer carries no home gate at all, so every root that observes a
project is architecturally positioned to author its entries. Whichever
path put three copies in three stores, no invariant forbade it — that is
the gap.

**A controller authors a child record only at the home of its parent.**
The root homing `Project/andamento` — and only that root — materializes
its ops entries. Children are born where their parent lives, recursively.

There are exactly **two homing seams** where a child's home departs from
its parent's, both already ruled by ADR 0032 and both explicit, recorded
decisions rather than silent drift:

1. **Admission placement** (ADR 0032 §4): a Convoy record is born at its
   primary placement host, however far from the ensure that demanded it.
2. **Per-child actuation locality** (ADR 0032 §5): a convoy's vessels,
   environments, and terminal sessions are born at whichever root
   actuates each one; their statuses replicate back and the convoy's home
   aggregates.

Two convoys of one project may therefore live at two roots, and one
convoy's children may span three. Nothing about cascade pins a project's
convoys to the project's root — the seams are precisely where actuation
demands the home be elsewhere.

## Mutation: route to the home; refuse when it is unreachable

A mutating verb (`convoy delete`, force-complete, resume) issued anywhere
resolves the record's home and executes **there** — finalizers and
teardown run at the home, and the deletion propagates outward as
tombstones through the ordinary replication channel. #1606 routes
resolution; this extends the rule to the mutation itself. Raw
`resource delete --host` remains the labelled break-glass, not a verb to
reach for.

When the home is unreachable, the verb **refuses, precisely**: name the
home, its last-seen time, and the break-glass. No queueing, no local
override. (Definitions-class records are untouched by this — 0016 makes
them editable from any root including offline, because such an edit is an
authored write to *your own* log, not a mutation of someone else's.)

**Deliberately deferred — disconnected operation and catch-up.** A
disconnected root already operates freely on records *it homes*: kiwi on
a train can create, run, and destroy its own convoys with zero
coordination, and reconnection is replication catching peers up, not
conflict resolution. The residue — syncing the history of records fully
born-and-died offline, projects created offline, and a possibly
interactive reconcile when a long-offline laptop returns — is real,
recognized, and later. The standing guidance until then: home durable,
shared things (projects, standing ensures) on always-on roots; a laptop
homes what is laptop-local. If refusal proves too blunt in practice, a
queued-mutation model is the named successor, to be designed against the
catch-up story rather than bolted on.

## Reads: decide against the merge view, reconcile only what you home

0016 built the include-replicas read layer with local-only as the
default. Diagnosing the quarantine incident exposed the remaining hazard:
admission lists placement policies through the local-only read
(`using::<PlacementPolicy>`, confirmed in `in_process.rs`), so the same
dispatch refused from kiwi and from feta named different candidate sets —
kiwi's omitted the feta-authored crew-image policy that sat in the merge
view the whole time. The failures' root cause that day was the readiness
gate (#1592/#1593), but the read-scope blindness is real in the code and
would produce wrong admission decisions on its own the moment a needed
record is homed elsewhere.

**Decision-making reads — admission, placement resolution, role and
record resolution, CLI and aggregator surfaces — are required to read
through the include-replicas view.** A decision made against a replica is
made against possibly-stale truth, which is acceptable because the
mutation that follows routes to the home — the consistency point.
Provenance and staleness stay visible ("as of feta, 40s ago").

Reconcilers keep the local default and never treat a replica as primary;
replicated records enter reconciliation only as inputs (the convoy home
aggregating replicated child status). This is 0016's invariant, restated
because admission turned out to be on the wrong side of it.

## Builtins: code-seeded records are a micro-class of their own

Builtin `WorkflowTemplate`s (`scratch`, `single-agent-contained`) are
seeded by code at every root's startup — same name, same content,
authored everywhere by construction. They are neither home-bound (no
single home) nor definitions in 0016's sense (no human author, no field
merge): they are **code-seeded builtins**, content-determined by the
running generation. Same-name multi-root authorship is legal for exactly
this micro-class; content divergence across roots is expected only while
a mixed-generation fleet is mid-roll and resolves at the next
`fleet-install`. A divergence that persists across a settled fleet
surfaces as a condition. A builtin edited by a human stops being a
builtin — that is a definitions-class record shadowing a seed, and the
seed-vs-edit relationship follows the existing managed-by labelling.

## Enforcement and the one-time sweep

By-construction turned out to be by-hope. Two mechanisms make the
invariants observable:

- **Collision detection.** The replicator, on holding a replica whose
  `(kind, namespace, name)` collides with a locally-authored record of a
  single-home kind, raises a Host condition (the `DecodeQuarantine`
  pattern, which the week proved gets noticed) naming both roots and the
  record. It never picks a winner silently.
- **Migration.** A one-time sweep resolves today's standing duplicates —
  crew-image placement policies, placement snapshots, host records, any
  surviving convoy husks — choosing the natural home for each (the root
  whose controllers actuate it) and deleting the rest through the raw
  path. The transitional `authoritative_host_source` heuristic for Host
  records retires once Host follows the general rule.

## Consequences

- Materializers become home-scoped: the project's home materializes its
  ops entries; other roots skip, and the skip is cheap (one home check).
- Mutating verbs grow home routing and a uniform unreachable-home
  refusal; their errors name the home and last-seen time.
- Admission and resolution switch their listing calls to the
  include-replicas view; the misleading "no placement policy satisfies"
  class of error dies with #1592's per-candidate reasons.
- The replicator gains collision detection; the fleet gains one Host
  condition kind and a migration sweep.
- The disconnected-laptop catch-up story is explicitly future work with
  its owner named (the queued-mutation successor designed against it),
  keeping today's refusal semantics honest rather than final.
