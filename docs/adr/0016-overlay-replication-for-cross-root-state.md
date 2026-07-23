# Overlay replication for cross-root state

**Status:** Accepted
**Date:** 2026-07-14
**Relates to:** ADR 0002 (federation — this ADR is the mechanism for its
"materialized replica" primitive), ADR 0006 (resources vs reference data),
ADR 0007 (requirements-first placement — per-vessel re-provision rides it),
ADR 0012 (tables — the conflict UX assumes a small badge-with-actions
extension to its column model, not yet specified there), ADR 0014
(Repository as a
convergent-facts resource; mortal keys), issue #688 (the scenario-first
semantics grill whose expectations this mechanism must deliver).

The #688 grill fixed the cross-root expectations and found three resource
classes with different semantics: **convergent facts** (natural key, union
correct), **definitions** (human-named, edit-anywhere-including-offline,
conflicts surfaced never auto-resolved), and **home-bound runtime**
(reconciled only at home). This ADR decides the mechanism: where converged
state lives, how merges are represented, how deletion propagates, how host
death is declared, and at what granularity runtime is homed.

## Converged state is computed, never stored: the overlay model

Each root's store contains only what **that root's users and controllers
authored**. No root ever writes another root's log — ADR 0002's
double-merge-killing invariant, held by construction. Cross-root state is
delivered by three pieces:

- **Durable materialized replicas.** Each root persists a replica of every
  peer's log (same store, a replicas area keyed by origin root), maintained
  by tailing the peer and resumable via watch-from-version. Replicas are
  disk state, not caches: a peer going away — or the local daemon
  restarting while it is away — changes nothing observable. A definition
  edit, once synced anywhere, is as durable as the fleet. (An ephemeral
  replica would make the merged view *regress* when its origin slept; that
  failure mode is what "durable" exists to kill.)
- **Relay.** Origin-authored log segments are immutable facts, so any root
  may serve its replica of any origin's log. A joining root tails whoever
  is awake and converges to each origin's true prefix; knowledge spreads
  through the awake part of the fleet. Relay is also the death story:
  survivors collectively hold a dead root's log forever.
- **A deterministic merge view.** Consumers of definitions-class kinds —
  aggregator, admission, CLI — read through a merge function over {own log,
  replica per peer}. All roots compute the same function over the same
  eventually-delivered inputs, so they agree by **confluence, not
  consensus**: no quorum, no agreement round, no coordination. The merged
  object is never written back anywhere as authority.

## Field merge: causal supersession applies, concurrency surfaces

This is deliberately **not last-writer-wins**. The clock never decides
anything; only causality or a human does:

- A write made *having seen* the current value is an intentional overwrite
  and supersedes silently, however late it syncs in.
- Writes made *concurrently* (neither saw the other) to the **same field**
  are siblings: the merge output for that field is all causally-maximal
  values, badged as a conflict — per-field multi-value-register semantics.
  Disjoint-field concurrent edits both stick.
- **Resolution is an ordinary write**: pick a sibling (or a third value)
  from any root; that write has seen all siblings, so it supersedes them
  and the badge dissolves fleet-wide as it syncs. No resolution protocol,
  no resolution authority.

The causal metadata rides as an optional `merge` block on `ObjectMeta`,
present only on definitions-class kinds and stamped by the **local store at
write admission** (clients never fabricate causality):

- a **per-field dot** `(author_root, author_counter)` on each field's last
  write, and
- a **per-record seen-vector** `{root → highest counter incorporated}`.

Merge rule per field: a dot covered by the other side's seen-vector is
superseded; mutual non-coverage is a conflict. Vector size is the fleet's
root count, per record not per field. Wall-clock timestamps ride along for
display ("edited 3 days ago on kiwi"), never for deciding. Convergent facts
and home-bound runtime carry none of this.

## One channel: the store's own watch API

Replication uses the resource store's existing k8s-style HTTP API
(`flotilla-resources/src/http`) over a forwarded UDS — the Tender's job
eventually; any forwarding suffices interim. Each root runs a **replicator
per peer**: list + watch the replicated kinds, apply events into the
durable replica area, resume from the stored version on reconnect.

- **Cadence is not a design knob.** It is a live watch: convergence latency
  is connection latency; the only policy is reconnect/backoff. "Within a
  sync cycle" means "within seconds of both roots being awake".
- **Class determines merge, not transport.** All three classes ride the
  same stream; the read layer differs — MV-register merge for definitions,
  natural-key union for convergent facts, and for home-bound runtime the
  replica *is* the view (read-only, staleness-stamped, never merged).
- **Replication is opt-in per kind**, declared where the kind's class is
  declared: definitions (Project, WorkflowTemplate, Host spec), convergent
  facts (Repository, Host status, observed Checkouts), and the home-bound
  kinds fleet views need (Convoy, Vessel, TerminalSession). Per-host
  plumbing (e.g. Presentation) does not replicate.
- Relay serves the replica area over the same watch surface, namespaced by
  origin root, with identical mechanics.
- **Replica durability is relative to the origin log's class (ADR 0004).**
  The cross-root classes here are *merge* classes, orthogonal to storage
  class: definitions, Repository, and home-bound runtime records ride the
  durable managed log and replicate resumably across restarts on both ends.
  Generational observed kinds (Host status, observed Checkouts) follow ADR
  0004's rule — the replica survives the *holder's* restarts, but an origin
  generation change forces discard + re-list. Origin *absence* is not a
  generation change: the replica retains the last generation's facts as
  last-known, staleness-stamped state until a new generation replaces them
  — which is precisely the no-regress property durability exists for.
- The `FleetReplicaSnapshot` broadcast path and the Plane-A peer transport
  under it are **subsumed and deletable** — the deletion ADR 0002 promised.

## The read side is store-level, not per-consumer

*(Amendment, 2026-07-23 openable-latent grill.)* The "merge function over
{own log, replica per peer}" is implemented **once, in the store's read
layer**: list/watch grows an **include-replicas** option whose returned
replica rows are origin-tagged and staleness-stamped. Consumers (aggregator,
fleet views, CLI listing) opt in by asking; nobody hand-assembles a second
watch over the replica area. Two invariants by construction, not convention:

- **Local-only is the default.** A consumer that doesn't ask sees exactly
  what it saw before replication existed — reconcilers therefore never
  encounter a replica row, preserving "home-bound runtime is reconciled
  only at home".
- **The include-replicas view is read-only by type.** A write through it is
  an API error, not a policy violation.

Class-specific merge (MV-register for definitions, natural-key union for
convergent facts) slots in as branches of this same read layer when those
classes arrive; home-bound runtime needs no branch — the replica is the view.

## Deletion is an authored tombstone; tombstones are permanent

You only ever write your own log, so delete is an ordinary authored write
of a tombstone state, carrying a dot + seen-vector like any field write.
Delete concurrent with edit is therefore *detectable* and surfaces as a
conflict ("deleted on kiwi · edited on feta"), resolved by an ordinary
superseding write — confirm the delete or revive with the edit. The merged
view hides tombstoned records by default.

**There is no retention policy: definitions-class tombstones are kept
forever.** The class is tens of human-authored records; lifetime tombstone
accumulation is kilobytes, and every GC scheme reopens the resurrection
window — a root that slept past the horizon syncs its stale live record
back in with nothing to supersede it. With laptops that sleep for weeks as
first-class fleet members, the horizon would be months anyway; permanence
makes relay and late joining safe unconditionally. If a genuinely churning
kind ever joins this class, the principled cut is compaction below the
fleet-wide minimum seen-vector (the membership roster is known — Host
resources); noted here, not built.

## Host death is a decree: `host <name> declare-lost`

No timeout can distinguish a dead root from a laptop asleep for three
weeks; the flip is a human judgment, hence a verb (subject-verb order per
the CLI convention): **`flotilla host <name> declare-lost`**, revoked by
**`flotilla host <name> found`**. Host **spec is definitions-class**
(membership is a ceremony; decrees are human-authored fields that must
replicate and merge), Host **status stays convergent observed facts** — the
k8s Node spec/status split. Two roots declaring the same host lost converge
trivially; declare-vs-found races surface as ordinary conflicts.

What the decree does: home-bound runtime homed there flips from "stale" to
**lost** — a truthful inventory rendered from the last replica ("vessel X,
branch Y, last observed commit Z, never pushed"), archivable or
re-provisionable, never "resumable"; replicators stop dialing it; placement
excludes it; the compaction roster shrinks. Survivors keep relaying its log
forever. A declared-lost host heard from again is a **loud surfaced
event**, never a silent rejoin.

**Definitions need no takeover step.** (Deliberately not "adoption" — that
word is the Observed→Adopted→Managed lifecycle-authority axis of ADR 0003,
which is unrelated here.) A dead root's authored definitions remain
readable (replicas are permanent) and remain *editable*: an edit on a
survivor is written to the survivor's own log with a seen-vector covering
the dead root's dots, superseding cleanly. Authorship is not ownership;
there is nothing to transfer.

**A wrongful declaration cannot fork.** If a host is declared lost while
actually alive — still reconciling a convoy it homes — and a survivor
performs unilateral succession in the meantime, the causal record decides:
the successor (`succeeded_from` C, authored after the decree) is visible in
replicas when the "dead" host resurfaces and syncs, and **a home that
learns of a successor to its own record stands down** — it stops
reconciling, marks its record superseded, and its partition-era writes
surface as part of the loud return-from-dead event (readable memory, never
silently discarded, hand-back available as another ordinary succession).
Two *concurrent* unilateral successors are detectable by construction (two
records claiming `succeeded_from` the same C) and surface as a conflict
like any other; never-fork holds one level up.

## Vessels home at placement; convoy records succeed, work re-provisions

A convoy is coordination state plus running work spread across hosts, and
the two must not share a fate. "Home follows the work" applied honestly:

- **Vessels are home-bound to their placement host**, their records
  authored in that host's log and reconciled by that host's reconcilers
  (which manage the actual containers and PTYs — local anyway). The convoy
  record is coordination only — spec, progression, vessel refs — and the
  merged view assembles the cross-log picture. A convoy *spans* logs by
  construction.
- **A vessel's host dies** → that vessel is lost (truthful inventory for
  it); the convoy survives; the home re-provisions that one vessel
  elsewhere via ordinary requirements-first placement. Never convoy
  teardown because one node died.
- **The convoy's home dies** → vessels everywhere are untouched; their
  crews keep working. Progression pauses, visibly ("waiting: kiwi lost").
  Recovery is **succession, not re-provision**: a new home authors a
  successor convoy record (`succeeded_from` ref) and each vessel's host —
  alive, writing its own log — repoints its convoy ref. Nothing physical
  moves because nothing physical was lost.
- **Live handoff** (laptop leaving, healthy convoy) is the same succession
  done cooperatively: the old home writes a final handed-off pointer, the
  new home authors the successor — causally ordered, conflict-free, cheap
  enough to be routine.
- **Re-provisioning is the only way work itself changes hosts**: rebuild
  from durable inputs (last-pushed branch + Brief, which lives in the
  checkout and pushes with the work). There is no mechanism that pretends
  running state moves. Single-vessel convoys default their home to the placement
  host at birth, so for the common case succession and re-provision
  coincide.

## Conflict UX

- **Detection** is a property of the read path: any field whose
  causally-maximal value set is plural is a conflict, computed identically
  on every root. No detector component.
- **CLI**: `flotilla <kind> <name> conflicts` renders each conflicted field
  with all sibling values and provenance; `flotilla <kind> <name> resolve
  <field> --take <root>` (or `--value`) performs the ordinary superseding
  write. Generated for every definitions-class kind.
- **TUI**: a conflict badge on the affected row in existing tables (a
  small badge-with-actions extension to ADR 0012's column model), opening
  the same siblings+provenance view. **No
  new query family** — conflicted-ness is row data; a fleet-wide conflict
  roll-up is add-on-program territory (ADR 0014's ruling), likely never
  needed at this cardinality.

## Consequences

- The definitions class gets a disciplined little merge layer —
  CRDT-flavoured semantics for a small class of rarely-edited,
  human-authored objects — named honestly so nobody pattern-matches it
  away. "Federated, not CRDT" stands for fleet state at large: convergent
  facts and home-bound runtime need none of it.
- Every definitions-class consumer reads through the merge view instead of
  a raw store get; at tens of records the view is a trivially cheap layer
  maintained by the replicator machinery.
- `ObjectMeta` grows an optional `merge` block; the store's write admission
  stamps it for definitions-class kinds.
- The aggregator's fleet inputs move from ephemeral `FleetReplicaSnapshot`
  broadcasts to durable replicas; the snapshot path and the Plane-A peer
  transport beneath it become deletable.
- Vessel records move to placement-host logs; reconcilers already run where
  the vessel runs, so this aligns records with existing authority rather
  than redistributing it.
- "Home follows the work" is refined, not revised: a convoy record's home
  is fixed at admission and changes only by explicit succession.
