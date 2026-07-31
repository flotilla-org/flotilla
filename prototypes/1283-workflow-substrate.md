# Workflow substrate exploration (#1283 and beyond)

Status: exploration record, 2026-07-31. Distilled from an eight-round HITL
session (Robert + Claude) that started on #1283's syntax question and ended
somewhere else. This file records conclusions as **proposals awaiting
rulings**, not rulings — the ADR-carry candidates are listed at the end.

## Where it started and where it ended

#1283 asked which of three spellings — terse rule strings, KDL, or derived
views — should make the #1269 ruled-model YAML glanceable. The exploration
falsified the premise: the ruled model was not the substrate. What emerged
instead:

> **The substrate is a verb surface plus the turn log. Workflow "definitions"
> are any deterministic driver of the verbs — a shell script, a rules
> function, a restricted agent, or a human at the CLI — and all of them
> resume the same way: read the log, continue.**

## The arc

Each round's reaction falsified something the previous round assumed.

| Round | Reaction | Finding |
|-------|----------|---------|
| 1 | Produce the three spelling candidates | Terse strings and derived views converge on the same notation (parse vs print); the ticket's own example conflated a fact with a transition — first sign of trouble |
| 2 | "Where did `implementation.ready-for-review` come from?" | The dotted namespace hid three provenances; outcome kinds (`implementation`, `review`) were defined by nobody — the vocabulary was reverse-engineered from the coding loop, bounding the system to re-skins of implement-review |
| 3 | "Stick to the real mechanisms" | Real primitives: our own machine (ADR 0024 phases), turn **claims** (crew says it fulfilled its brief, with a disposition the *brief* declares), and observations (honest only with `observed-at`). Everything else was predicates in an objecty costume. Rearm/settles/expecting dissolve into freshness comparisons and completion predicates |
| 4 | Stress-test flappy mergeability | Two machinery obligations survive any surface: **the postdating rule** (a turn is judged only against observations newer than the turn; read-set tracking + targeted re-observation) and **three-valued honesty** (observations may be Unknown; Unknown triggers nothing, closes nothing). Levels beat edges on flap; most of the wake-admission gap collapses into these |
| 5 | "DSL tragedy; real embeddings; or give an agent the tools" | Restraint belongs in the **capability surface** (want-turn / complete / hold / escalate), not in a crippled grammar. Reconciler-style pure policy gets Temporal-class durability with zero replay machinery because the store already holds all durable state |
| 6 | "That's not imperative — could anyone drive it manually?" | The program-counter taxonomy: derived (rules), stored (Temporal), or **reconstructed by memoized replay over the turn log** — which is the mechanization of how a human resumes: read the log, continue. Linear workflows want the sequential costume; standing reactions want the rules costume; neither is general. Dataflow between turns becomes function arguments |
| 7 | The command thread | Grounded in `flotilla-commands`: manipulation verbs mostly exist (`crew … handoff` is already ensure-flavored), reads exist as queries, **synchronization is entirely absent**. The missing keystone is `flotilla wait` — which-condition select over a leaf algebra. The CLI's poverty forces explicit `--step` keys, the honest answer to step identity |
| 8 | "Wait could do weird tricks; do we have unified reads?" | The typed resource watch already exists at the store layer (`flotilla-resources` backend: cursors, provenance) but is not the client surface — exposing resource get/list/watch is the **shared prerequisite** for wait, Bosun, and the post-reshape TUI. `wait` is an environment-aware seam with a suspend ladder; a parked script at `wait --for X` is literally an engagement-rule row — the costumes converge in the daemon's tables |

## The proposal

### Primitives (what is real)

1. **Our own machine** — convoy/vessel phases and declared transitions
   (ADR 0024). Authored by us; its history is the log.
2. **Turns** — started with a brief; ended by the crew's *claim* of
   fulfilment, carrying a disposition **declared by the brief** (the brief is
   the question; the disposition is the answer). Claims are not world-facts
   and never masquerade as them.
3. **Observations** — projections of external systems, carrying `observed-at`
   and permitted to be Unknown.

There are no outcome kinds, no semantic-transition objects, no second event
bus. Conditions are predicates over the three primitives.

### The verb surface (the ruled contract)

- **Reads**: resource get/list/watch exposed through the client (the store
  layer already has typed watch with cursors and provenance).
- **Manipulation** (largely existing): ensure-and-deliver a brief to crew,
  re-task, mark work complete, hold (loud, with reason), escalate,
  delete/abandon.
- **Synchronization** (new): `flotilla wait --for <leaf> [--for <leaf>]…
  [--fresher-than turn:<id>] [--timeout] --json` — level-triggered (checks
  current state before blocking), returns *which* condition(s) triggered plus
  a snapshot, Kleene-honest (Unknown never triggers), postdating via
  `--fresher-than`. Prior art: `inotifywait`.
- **Idempotency**: mutating verbs accept `--step <key>`; re-running a driver
  finds the recorded result. Step keys are explicit and author-named.

Restraint lives here: arbitrary logic upstream of these verbs is harmless
because the verbs bound authority. Scope defaults to the convoy's own
resources per the subscription grill.

### Conditions: leaves only

The condition language is deliberately tiny — field comparison, latest-turn
disposition/phase, before/after freshness — and stays *leaf-level*. OR is
built into `wait` (multiple `--for`); AND-chains, counters, loops, budgets,
and variables live in the caller's real language. The #1216 review budget is
`for round in 1 2 3` in bash. Named condition definitions (layered data, like
everything else) can restore shared vocabulary without fictional objects.

The same leaves get two bindings: **scripts block on them; agent crew is
woken by them**. An engagement rule was always condition + engage, so the
rules costume and the script costume share one vocabulary.

### The suspend ladder (wait as a seam)

| Caller | Suspend semantics | Requires |
|--------|-------------------|----------|
| Plain shell | blocks on the watch stream | nothing |
| Crew script, parked | daemon registers (condition → resume turn), SIGSTOPs the process | ambient crew identity (pattern exists: `resolve_with_crew_id`) |
| Crew script, evicted | process killed; re-run from the top on trigger | step-keyed verbs (opt-in; keyless scripts degrade to parked) |
| Agent | end the turn; resume = new engagement with which-condition digest | the wake binding — same seam |

A parked script is an engagement-rule row in the daemon's condition table.
Coalescing, migration (driver state is in the store), and legibility ("who is
waiting on what" as visible rows) follow. Discipline: `wait` must be the
script's only blocking point against flotilla state for evict to be sound.

### Machinery obligations (from the flappiness stress test)

- **Postdating**: episode continuation/escalation decisions evaluate only
  against observations newer than the turn they would judge; the evaluator
  tracks read sets; turn end triggers targeted re-observation.
- **Three-valued honesty**: Unknown observations trigger nothing and close
  nothing; episodes hold.
- **Episode identity**: one open turn per (rule/wait, convoy, vessel, role);
  a predicate that holds again after a completed, freshly-judged turn opens a
  new episode and climbs the machinery-owned escalation ladder.
- **Pinning at admission** (already ruled) is what defuses
  replay/versioning: a driver only ever replays a log it wrote.

## Build order

Motivation: until now the complex workflows have been run by Robert or by a
governor agent doing too many things at once — undisciplined, judgment-based
exits, no transcripts to learn from. The order below is chosen so each step
falsifies the previous round's speculation cheaply and no costume is
foreclosed while evidence accumulates.

0. **Expose resource get/list/watch through the client** (+ script-friendly
   `--json`). Shared prerequisite for everything below and for the
   post-reshape TUI; zero decisions deferred, control-plane work.
1. **Bosun-over-verbs experiment**: a Bosun brief driving today's
   manipulation verbs plus the new unified read — no `wait`, the agent polls
   or is nudged. Wasteful but shippable immediately; starts the transcript
   corpus; tests "is an agent with verbs enough?"
2. **`flotilla wait`** — shipped as "block" only, but with the contract
   *written* for the full suspend ladder so park/evict/wake are compatible
   deepenings. `--step` keys land here. Script crew members become
   economical.
3. **Extract what recurs**: TypeScript/Python bindings over the
   command/entity object model; library combinators (`race`, `until`,
   `with_budget`); "code mode" agents.
4. **Only now rule on costumes**; compiled Rust/WASM workflow functions as
   hardening of patterns the transcripts prove recur.

## Proving experiments

Run each proving scenario **twice** — as a script crew member (a turn in a
cleat session: recorded, supervised, restartable, credentialed by existing
machinery) and as a Bosun brief over the same commands. The transcripts are
the empirical corpus that steps 3–4 read.

### The inside-out shepherd

The sharpest experiment: today's pr-shepherd runs agent-outside — the coding
agent drives the loop and exits when *it judges* it has no more feedback to
address. Inverted, a script crew member drives and calls back into the agent
as another crew member:

```bash
while true; do
  hit=$(flotilla wait --for "cr.merged" \
                      --for "cr.checks == failing" \
                      --for "cr.review.unaddressed" \
                      --for "cr.mergeability == conflicting" \
                      --fresher-than "turn:$last" --json)
  case "$(jq -r .triggered <<<"$hit")" in
    cr.merged) exit 0 ;;
    *) last=$(flotilla crew coder handoff \
                --step "round-$((++n))" \
                --message "$(brief_for "$hit")") ;;
  esac
  [ "$n" -ge "$BUDGET" ] && flotilla convoy hold --reason "shepherd budget exhausted"
done
```

What inverts: the **exit condition moves from claim to observation** (merged
/ closed / checks green — world-facts), the **exhaustion condition becomes a
loop bound with a loud hold** (not agent fatigue), and residual judgment
("is this thread actually addressed?") shrinks to a narrow claim inside a
narrow turn, composable with world-checks instead of controlling the loop.
Honest gap carried forward: `cr.review.unaddressed` is itself partly a
judgment — defining that leaf (thread state + freshness vs last crew
activity) is the old GAPS #6 problem resurfacing at leaf scale, which is
where it is tractable.

## Effect on standing material

### GAPS disposition (from prototypes/1262-scenarios/GAPS.md)

| GAPS item | Disposition under the proposal |
|-----------|-------------------------------|
| 1 — outcome shape/authority | **Dissolved**: no outcome kinds; dispositions are declared per-brief; results are turn records |
| 2 — early handoff declaration | **Dissolved**: drivers pass data between turns as arguments; overlapping turns are the driver's choice |
| 3 — wake admission/coalescing | **Replaced** by postdating + three-valued holds + episode identity |
| 4 — target policy | **Open, relocated**: verb-level behavior of ensure/handoff against warm/cold/dead sessions |
| 5 — review budget & loud hold | **Dissolved**: a loop bound and `convoy hold` in the caller's language |
| 6 — semantic vocabulary | **Shrunk** to defining condition leaves (and named definitions as layered data) |
| 7 — brief/digest inputs | **Dissolved** into function arguments / the which-condition payload |
| 8 — pinned definitions | **Unchanged and load-bearing**: pinning at admission is what makes driver replay sound |

### ADR-carry candidates (each needs its own ruling)

- **#1265 (event vocabulary)**: dotted semantic transitions → condition
  leaves over the three primitives + named definitions as layered data;
  interpreters survive only where external reality must be projected into
  observed fields.
- **#1268 (workflow model core)**: "no imperative scripts" → "no durable
  hidden control state competing with the convoy machine" (which drivers
  satisfy: all durable state is in the store); "adaptive workflows =
  engagements that write declarations" → the Bosun over the same verbs, with
  recurring behavior compiled down.
- **#1266 (subscription surface)**: rules-only survives *structurally* — the
  daemon's condition table remains the one subscription surface — but its
  rows are now written by `wait` parkings and agent wake registrations, not
  only by workflow data.
- **#1263 (prior art)**: "Temporal Signals middle" → reconcile-style pure
  policy / memoized-replay drivers; the store is the durability, not
  suspended stacks.

## Open questions

1. Enumerate the condition-leaf list against all six #1269 files (the
   round-2-style smuggling check, aimed at the leaf language).
2. `Interrupted` semantics at each verb when the world overtakes the plan
   (human merges mid-loop); default propagate-and-end, but walk an
   adversarial timeline.
3. Abandoned/failed turns: what does a step-keyed ensure return when its
   turn died without a claim — retry under the same key, or a driver-visible
   error?
4. Step-key conventions (per-role ordinals vs author names) and collision
   behavior.
5. Multi-convoy standing workflows (governors, #650): the verb surface must
   not foreclose a Bosun watching many convoys; scope-widening rules.
6. Whether `wait`'s contract declares the full suspend ladder on day one
   (proposed: yes, ship "block" first).
