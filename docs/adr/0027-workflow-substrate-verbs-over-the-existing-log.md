# The workflow substrate is a verb surface over records we already keep

**Status:** Accepted
**Date:** 2026-08-01
**Relates to:** ADR 0023 (histories are evidence; drivers read them, the machine
stays authoritative), ADR 0024 (the convoy/vessel machine and its declared
transitions are one of the three primitives), #1262 (wayfinder map),
#1283/#1270 (the exploration and its exit),
[prototypes/1283-workflow-substrate.md](../../prototypes/1283-workflow-substrate.md)
(the full exploration record this ADR compresses). Amends the recorded rulings
on #1263, #1265, #1266, and #1268 — see the amendment table.

## Context

The #1262 grills produced a ruled workflow model (declarative YAML per
workflow, semantic transition events, rules-only subscriptions). The #1283
exploration, asked only to make that model glanceable, falsified its premise
instead: the vocabulary of "outcome kinds" and dotted semantic transitions had
been reverse-engineered from one coding loop, and every mechanism it needed
already existed in a simpler form. What survives is recorded here; what was
overturned is recorded as amendments, not silently replaced.

Workflow runs until now were driven by a human or an over-tasked governor
agent: undisciplined exits, judgment where observation should rule, no
transcripts to learn from. The substrate exists to fix that without inventing
a workflow engine.

## Decision

**The substrate is a verb surface plus the records we already keep. A
workflow "definition" is any deterministic driver of the verbs — a shell
script, a rules function, a restricted agent, or a human at the CLI — and all
of them resume the same way: read the log, continue.**

### The three primitives (nothing else is real)

1. **The machine** — convoy/vessel phases and declared transitions per
   ADR 0024. Authored by us; its history is already in the event log.
2. **Agent turns** — not a new concept and not a new resource: a turn is one
   round of an agent loop as it already works. A **brief** (a concrete,
   normally textual prompt) is delivered into a crew session; the crew works;
   it ends with the existing completion, which is a **claim** of fulfilment —
   never a world-fact. The record is what we already keep: `crew_work`,
   the event history behind it, and the cleat transcript.
3. **Observations** — projections of external systems (PR state, checks,
   containers), carrying `observed-at`, permitted to be **Unknown**.

There are no outcome kinds, no semantic-transition objects, and no second
event bus. Conditions are predicates over these three.

### Schema footprint (deliberately minimal)

Two additions to existing records; nothing else changes shape:

- **Dispositions.** A brief may declare its answer vocabulary (a shepherd
  brief: `merged | changes-pushed | blocked`; a review brief:
  `approve | request-changes`; a smoke contract: `pass | fail`). The agent
  knows the valid answers before starting. `crew complete` records the chosen
  disposition beside the free-text message; drivers branch on the word, never
  by parsing prose. Briefs that declare nothing keep today's free-form
  completions.
- **Step keys.** Mutating verbs accept `--step <key>` (author-named, e.g.
  `round-2`). Re-running a driver finds the recorded result for a key instead
  of re-issuing the action. Step keys are the whole answer to driver
  idempotency; they are explicit because the CLI cannot see a program counter.

Richer completion payloads (reports, measurement bundles, collection packets)
are explicitly **not** dispositions. If transcripts show the need, that is a
future *artifact* concept hanging off completions — out of scope here.

### The verb surface

- **Reads**: resource get/list/watch through the client with script-grade
  `--json` and cursors (delivered by #1288). "Read the log" means these plus
  transcripts — no privileged driver API exists.
- **Manipulation** (largely existing, ensure-flavored): deliver a brief
  (handoff / re-task), complete, hold (loud, with reason), escalate,
  delete/abandon.
- **Synchronization** (the missing keystone): `flotilla wait --for <leaf>
  [--for <leaf>]… [--fresher-than <ref>] [--timeout] --json` —
  level-triggered (it evaluates current state before blocking, so a condition
  that already holds returns immediately), returns *which* condition fired
  plus a snapshot, honest about ignorance (an Unknown observation never
  triggers), and refuses stale evidence via `--fresher-than` (see
  *postdating* below).

Restraint lives in this surface, not in a restricted grammar: arbitrary
caller logic is harmless because the verbs bound authority. Verb scope
defaults to the convoy's own resources; multi-convoy callers (a Bosun, a
governor over a project) widen scope through the existing
dispatching-principal model, not through new workflow ACLs — the exact rules
land with the Bosun contract.

### Conditions: leaves only

The condition language is deliberately tiny: field comparison, latest
completion disposition/phase, before/after freshness. It stays leaf-level —
a *leaf* is one atomic predicate, a single comparison with no connectives.
OR is built into `wait` (multiple `--for`); AND-chains, counters, loops,
budgets, and variables live in the caller's real language — a review budget
is `for round in 1 2 3` in bash, and exhaustion is a loud `convoy hold`, not
agent fatigue. Named condition definitions are layered data restoring shared
vocabulary without new objects.

The same leaves get two bindings: **scripts block on them; agent crew is
woken by them**. The daemon's condition table remains the single
subscription surface; its rows are written by `wait` parkings and wake
registrations as well as by standing rules.

### The suspend ladder (declared whole, shipped incrementally)

| Caller | Suspend semantics |
|--------|-------------------|
| Plain shell | blocks on the watch stream |
| Crew script, parked | daemon registers condition → resume, SIGSTOPs the process |
| Crew script, evicted | process killed; re-run from the top; step keys make this sound |
| Agent | turn ends; resume is a new engagement carrying the which-condition digest |

`wait` ships block-only first, but its contract declares the full ladder so
park/evict/wake are compatible deepenings. Discipline: `wait` must be a
script's only blocking point against flotilla state for evict to be sound. A
parked script *is* an engagement-rule row — visible, coalescible, migratable.

### Machinery obligations

- **Postdating** (plainly: never judge work by evidence older than the
  work): continuation and escalation decisions are evaluated only against
  observations newer than the completion they would judge; the evaluator
  tracks what it read and triggers targeted re-observation at completion.
- **Three-valued honesty**: Unknown triggers nothing and closes nothing;
  episodes hold.
- **Episode identity** (plainly: one firing, one engagement — and a
  re-fire is a new, escalated round, not a re-trigger): one open engagement
  per (condition source, convoy, vessel, role); a predicate that holds again
  after a completed, freshly-judged engagement opens a new episode and
  climbs a machinery-owned escalation ladder.
- **Pinning at admission** (unchanged from the #1268 grill; load-bearing):
  definitions are pinned when the convoy is admitted, so a driver only ever
  replays a log it wrote.

### Exit conditions move from claims to observations

The proving inversion (the inside-out shepherd): the loop's exit is
`cr.merged` — a world-fact — not the agent's judgment that it is finished.
Residual judgment shrinks to narrow claims inside narrow turns, composable
with world-checks instead of controlling the loop. This inversion is the
acceptance posture for every workflow built on this substrate.

## Amendments to prior rulings

| Ruling | What it said | What stands now |
|--------|--------------|-----------------|
| #1265 (event vocabulary) | Dotted semantic transitions as the workflow event language | Condition leaves over the three primitives, plus named definitions as layered data. Interpreters survive only where external reality must be projected into observed fields |
| #1268 (workflow model core) | "No imperative scripts"; adaptive workflows = engagements that write declarations | "No durable hidden control state competing with the machine" — which drivers satisfy, since all durable state is in the store. Adaptivity = a Bosun over the same verbs; recurring behavior compiles down later |
| #1266 (subscription surface) | Rules-only subscriptions, defined in workflow data | Structurally intact — one condition table — but its rows now also come from `wait` parkings and agent wake registrations |
| #1263 (prior art posture) | "Temporal Signals middle" as the durability model | Reconcile-style pure policy and memoized replay over existing records; the store is the durability, never suspended stacks |

Unamended and folded in: pinning-at-admission (#1268), machine authority
(ADR 0024), episode identity and the escalation ladder, derived-only events,
level-triggered wake over the existing log (no second bus).

## Deferred, with owners

- Leaf-language enumeration against the six #1269 scenario files — the
  `wait` contract.
- `Interrupted` semantics when the world overtakes the plan (human merges
  mid-loop; default: propagate and end the engagement) and step-key
  collision/dead-turn behavior — the `wait` contract.
- Multi-convoy scope-widening rules — the Bosun contract.
- Costume rulings (sequential embeddings, compiled workflow functions) —
  only after the transcript corpus from the proving experiments (build-order
  step 4).
- Artifacts (structured completion cargo) — future concept, separate ruling.

## Consequences

Build order (from the exploration, step 0 already delivered as #1288):
Bosun-over-verbs experiment → `flotilla wait` block-mode with the full-ladder
contract → extraction of recurring combinators → costume rulings. Each
proving scenario runs twice — as a script crew member and as a Bosun brief —
and the transcripts are the empirical corpus the later steps read. The map
(#1262) closes when the implementation contracts exist.

What we give up: a declarative workflow artifact that promises to be the
whole truth of a run. What we get: drivers in real languages with zero replay
machinery, durability inherited from the store, workflows that a human can
resume by reading the same records the drivers read, and exit conditions
that are observations rather than assertions.
