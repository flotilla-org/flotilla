# Landed records world terminals only; the vessel is a cache

**Status:** Accepted
**Date:** 2026-08-03
**Relates to:** Amends ADR 0021 (Landing/Landed/Anchored — supplies the phase
skeleton this builds on) and ADR 0027 (the verb surface and condition
leaves). ADR 0017 (claims/conditions split, unchanged), #1340 (the grill that
ruled everything here), #1321 (Bosun run 1 — the evidence source), #1322
(`flotilla wait`, re-scoped by this ADR into the leaf engine), #1341/#1342/
#1343 (the machinery defects the same grill traced).

## Context

On 2026-08-02 two convoys reached `Landed` and vessel teardown at the moment
of their sole crew's settlement claim while their PRs sat open — one silently
(the first Bosun run observed PR #1338 merge into a convoy already settled),
one destructively (a review-round brief for PR #1329 was written into a dying
session; `cleat send` returned success; recovery cost a manual re-dispatch).

The #1340 grill began as "design the split between turn-exit and convoy-exit"
and discovered mid-flight that ADR 0021 already rules the skeleton: claims
write only `Active → Landing`, the lifecycle reconciler alone condition-writes
`Landing → Landed`, teardown keys on terminal phases. The incidents were a
bug, not a gap — the settlement condition and the teardown gate both evaluate
vacuously true over an empty checkout list, and the daemon discards the
federated checkout list the reconciler resolves, so a cross-host convoy
settles with zero evidence (#1341, with #1342 and #1343 alongside).

What the grill ruled is therefore recorded as amendments layered on ADR 0021
and ADR 0027, not a new design. Bosun run 1 supplied the measurements: 13
observation passes over 47 minutes of which ~44 were pure sleep (the cost of
having no leaf engine), zero judgment leaks under the observation-only
discipline, and a shell needed only for `sleep`.

## Decision

### Exit tables: Landed records world terminals only

A workflow definition (pinned at admission, per ADR 0027) declares its exit
as a **table of named dispositions, each bound to one condition leaf that is
a world terminal** — an observation about the workflow's artifact that makes
the work genuinely over: `merged: cr.merged`, `closed-unmerged: cr.closed`.
The first leaf to fire writes `Landed` and records its disposition name.
Comma is OR; the table is the whole exit.

What may **not** appear in the table, and where it goes instead:

- **Budget** is a *delivery policy*, not an exit. Where a caller exists — a
  script, a resident driver — its budget stays in the caller's real language,
  exactly as ADR 0027 rules. But wakeup-delivered turns have no caller to own
  the count; there the bound is ADR 0027's own machinery-owned **episode
  escalation ladder** doing the counting: past the declared round budget the
  engine refuses to deliver another turn for the same episode and escalates
  loudly (the `convoy hold` of 0027's example, raised by the machinery). The
  convoy stays in `Landing`, context intact.
- **Staleness** is an *attention flag* in the fleet view, never an exit.
- **Give-up is always an explicit act** — the governor reaps. No automatic
  disposition silently discards a convoy whose PR might merge next week.
  An unreaped pile is an attention-degradation tradeoff the operators own;
  visibility problems beat resurrect-discarded-context problems.

`exit: claim` remains **declarable** for artifact-less work (probes,
instruction-only convoys with nothing observable). The pre-0021 conflation
thereby becomes an explicit degenerate case one opts into. An undeclared exit
means there is no `Landing` → `Landed` transition: the convoy is a standing
facility until an operator explicitly reaps it. Migration reading: every
current stock workflow declares the former hardwired "no change request
outstanding" condition as an explicit table; there is no magic default.

Consequence for the operator surface: **stuck = `Landing` + escalation
raised** (or past the staleness threshold) — cleanly distinguished from a
convoy that is merely idling toward its world terminal.

### The vessel is a cache

**No substrate semantics may depend on vessel identity across turns.** A
convoy's resumable essence is its durable records — agent session logs, the
change-request branch, the pinned workflow definition. Parking a vessel is a
**depth spectrum** — warm process → suspended vessel → no vessel at all,
logs archived to object storage — and depth changes only delivery latency
and how much re-provisioning must do, never what `Landing` means. ADR 0021's
"the vessel stays warm" is re-read as the shallowest depth of this spectrum;
its own follow-on note (per-vessel warmth by DAG reachability) is the same
instinct. Depth-tier machinery (eviction policy, archival) is deferred; the
invariant binds now: nothing being built may assume a live session name
survives between turns.

### The claim is a durability fence

A settlement claim carries the obligation that everything the convoy will
ever need again is durable at claim time: commits pushed to the
change-request branch, the agent session log flushed. Enforcement is
**flag, don't hard-reject**: dirty checkout or unpushed commits at claim time
flag the claim record (the audit trail for what was dropped) — a hard reject
could wedge a convoy whose agent died mid-claim. The delivery side is
unapologetic: work not durable at the fence is legitimately lost, by name —
unpushed commits, shell state, running processes, build caches, scratch
files. A warm same-vessel delivery finding them still present is the cache
hitting, never an entitlement.

This is the release-fence half of "handoff = make my work visible to
happens-after turns"; the delivered brief is the acquire half.

### Ensure-flavored delivery and its ladder

Delivering a turn to a convoy in `Landing` (or `Anchored`) is an *ensure*: it
re-provisions the turn's prerequisites from durable records rather than
failing on a dead session name. Agent-context restoration walks a
**delivery ladder**: warm session still alive → adapter resume from the
session log (a per-adapter capability) → **fresh agent with a reconstructed
brief, the never-fails floor**. The floor is load-bearing: delivery never
fails for want of context, and it forces round briefs to be self-sufficient
(point at the PR, the review, the records — never lean on a live session's
memory). No workflow may demand a minimum rung in v1.

Two obligations ride with the ladder:

- **Rung observability is first-class.** Every delivery records the rung it
  landed on; a fleet-wide silent fall-through to the floor is a defect
  signal, not a working system.
- **Rung selection is an experiment, not an edict.** The value of a warm
  session ties to LLM API prefix-cache economics — past the point where the
  provider re-ingests the prefix anyway, revive-from-brief may beat resume.
  The substrate records rungs and outcomes so those experiments have data;
  it does not hard-code the heuristic.

The ADR 0027 suspend ladder and this delivery ladder compose: suspend is how
a watcher sleeps, delivery is how a turn wakes.

### The leaf engine: one event-delivery mechanism, two watcher kinds

Once a convoy is claimed out, nothing is running to notice the world — so
leaf evaluation is **substrate machinery**: one daemon-side engine evaluates
condition leaves and delivers to watchers of two kinds:

- **Internal state** — the `Landing`/`Anchored` reconcilers: every parked
  convoy subscribes its armed leaves; an exit-table leaf firing writes
  `Landed` with its disposition, a wakeup leaf firing triggers
  ensure-flavored turn delivery.
- **Hanging commands** — a blocked `flotilla wait` caller is the same
  watcher whose delivery unblocks a process.

This is ADR 0027's "single subscription surface" given its engine, and the
"event-source design" ADR 0021 deferred for `Anchored`'s wiring — #1322 is
re-scoped from "a blocking CLI verb" to building it, and its leaf enumeration
is the shared legal vocabulary for exit tables, wakeups, waits, and
`Anchored`. Push transports (webhooks) are a later optimization of the same
engine; polling fallback survives per the disconnectable topology.

**Resident driver crews are demoted to sanctioned stopgap.** Run 1 measured
the residency cost and the circularity (the watcher needs a warm vessel
forever and dies with the vessel it should outlive). The Bosun's mechanical
half (leaf watching) moves into the engine; its judgment half (read the
review, compose the round brief) survives as a **driver turn** — delivered by
a wakeup like any other turn, running minutes, not resident hours.

### Vocabulary

`Landing` keeps its name — "claimed-out" is only the mechanism gloss for how
it is entered. `Anchored` survives as a distinct phase: both are "parked on
armed leaves," but `Anchored` is work incomplete waiting mid-flight, while
`Landing` is all completion claims filed awaiting a world terminal. Both are
park-eligible. The fleet view shows a `Landing` convoy's armed-leaf summary
with age ("awaiting `cr.merged` · 4h"), its last delivery rung, and the
attention flag; `Landed` rows show their disposition name.

## Amendments to prior rulings

| Ruling | What it said | What stands now |
|--------|--------------|-----------------|
| ADR 0021 (Landing→Landed writer) | The reconciler evaluates the hardwired "no change request outstanding" condition | Workflow-declared exit tables of world terminals; stock workflows transcribe the former condition explicitly, an undeclared exit makes `Landed` unreachable, and `exit: claim` is a declarable degenerate case |
| ADR 0021 (vessel warmth) | "The vessel stays warm" during Landing | The vessel is a cache; warmth is the shallowest park depth of a declared spectrum |
| ADR 0021 (Anchored wiring) | Entry/exit wiring deferred to a shared event-source design | That design is the leaf engine (#1322); Anchored and Landing subscribe to the same table |
| ADR 0027 (dispositions) | Declared per-brief, recorded on the claim | Unchanged at brief scope; exit tables reuse the same vocabulary at convoy scope, bound to world-terminal leaves |
| ADR 0027 (suspend ladder) | Caller-side suspend semantics | Composes with the delivery ladder: suspend is how a watcher sleeps, delivery is how a turn wakes |
| ADR 0027 (budgets live in the caller) | Counters and budgets in the caller's real language; exhaustion is a loud `convoy hold` | Unchanged where a caller exists. Wakeup-delivered turns have no caller, so their bound is 0027's machinery-owned episode escalation ladder, surfaced as delivery refusal plus the same loud hold |

## Deferred, with owners

- Park-depth machinery (eviction, object-storage archival of session logs)
  — after the leaf engine; the invariant already binds.
- Rung-selection heuristics (prefix-cache economics) — experiments over
  recorded rung/outcome data.
- Reopen semantics (a closed-then-reopened change request against a settled
  convoy) — #1343's ruling.
- Registered crew injection into a running convoy (the run-1 Bosun was a
  ghost — invisible to the inhibitor, teardown, and settlement) — adjacent
  to the #1071 adoption verb family.

## Consequences

- #1341/#1342 repair the machinery to ADR 0021's own rules; this ADR changes
  what the reconciler evaluates (declared tables) without changing who
  writes what.
- Bosun run 2 (#1321) runs resident by explicit stopgap sanction, validating
  the decision table that workflow definitions will encode.
- The shepherd stage of the standard coding workflow becomes: crew claims →
  `Landing` → leaf engine watches → review wakeups deliver driver/coder
  turns → `cr.merged` fires → `Landed { disposition: merged }` — retiring
  both the resident-poller shape and the hand-shepherding of PRs.
