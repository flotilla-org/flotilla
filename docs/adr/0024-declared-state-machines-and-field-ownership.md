# 0024 — Declared state machines, static field ownership, and a fold-based property engine

Date: 2026-07-29
Status: Accepted

## Context

A week of control-plane dogfooding produced a stream of state-transition bugs that
sort into three mechanism classes:

1. **Multi-writer fields with no declared owner.** An operator-applied
   `PlacementPolicy.spec.priority` was silently reverted twice: first by the local
   registration loop (#1236 / PR #1240), then — after that fix — by the remote-host
   registration path the fix didn't cover. Nothing in the system even *names* which
   writer owns which field.
2. **Restart/recovery paths that don't commute with in-flight lifecycle states.**
   Ghost `TerminalSession` resurrection (#1202), a material lease released while its
   holder was mid-deletion (caught in PR #1242 review), capabilities shed on daemon
   restart (#1225), convoys latching `Failed` on transient errors (#1218).
3. **Reads against the wrong store/authority** in the federated mesh (#1247, stale
   replica caches, tombstone-less ghosts). Ruled **out of scope here** — that class
   belongs to the #1188 federation contract — but the enforcement point built here
   deliberately leaves room for store-authority identity.

The underlying defect is that none of these state machines exist as artifacts: the
convoy/vessel/environment/lease lifecycles live as `if` statements scattered across
reconcilers, so there is nothing to check a property against, and every writer of a
resource is a peer of every other.

Prior-art research (see
`docs/superpowers/research/2026-07-29-state-transition-verification-prior-art.md`)
established: class 1 has **no reliable detector in the literature** — Anvil formally
assumes competing writers stop; Kubernetes server-side-apply made field ownership
advisory with a `force` escape and degraded to universal forcing. Class 2 is the
best-covered class: Sieve's differential oracles caught 45/46 controller bugs and
Acto's rollback oracle 10/10 recovery bugs, both portable as a few hundred lines
against a store we own. The "one generic engine + per-system property data"
factoring is unanimous across runtime verification, model checking, PBT, and event
sourcing; for safety properties the engine is literally a fold (monitor synthesis
yields a DFA whose δ steps over the event log), which our event-sourced store
supports natively. Liveness does not fold: finite traces check bounded convergence
only.

## Decision

### 1. State machines are declared as Rust data, co-located with the resource types

Each lifecycle-bearing resource in `flotilla-resources` declares its transition
table — phases and allowed edges — as plain data next to its phase enum. The table
is validated (reachability, termination) and is the single artifact reviewed in
diffs, rendered by legibility surfaces, and reused as the monitor δ. Machines are
not store resources and not derive-macro output: they are substrate, and substrate
is reviewable code.

### 2. Field ownership is static, declared per field, and enforced at the write path — with no force parameter

Every spec/status field is assigned exactly one owner role: **operator-authored**,
**loop-derived**, or **actuator-observed** (the #1188 owner/actuator vocabulary,
generalized). All writes go through one write helper that knows the writer's role
and refuses writes to fields the role does not own. There is deliberately **no
`force` escape hatch**: the k8s experience shows an advisory mechanism with a force
path degrades to universal forcing. Ownership conflicts are design bugs settled in
review, not runtime negotiations. Dynamic managedFields-style tracking is rejected
as serving an open writer population we do not have.

The write helper's identity parameter is designed to extend to store-authority
identity, making it the future enforcement point for #1188's single-writer rule.

### 3. Violations are refused loudly; rollout is observe-then-enforce, per machine

An undeclared transition or unowned-field write is, in the end state, refused with
a typed error (reconcilers treat it as a stale-view requeue) and recorded as a
visible event carrying writer identity, the attempted change, and the violated
rule. Because the initial tables will be reverse-engineered from code that was
never explicit, each machine ships in **observe mode** (violations logged and
fleet-visible, writes allowed) and flips to **enforce** per machine once the
adversarial harness passes it and dogfooding shows zero observed violations.

### 4. One generic property engine; per-resource property data

Generic infrastructure, written once:

- A **transition vocabulary** extending the existing liveness harness (#1176/#1214):
  `Reconcile`, `ExternalSpecWrite`, `Delete`, `RestartController`, `AdvanceClock`,
  `DropActuation`/`DeliverActuation`, `PartitionStore`. This makes all the bug
  classes *reachable* by tests; today none is.
- **Differential oracles**: Sieve's end-state diff and per-object write-count diff
  against an unperturbed reference run (a write-count divergence is a stomp even
  when end states match), and Acto's rollback oracle for recovery paths.
- **Generated sequences with shrinking** via `proptest-state-machine` over the same
  vocabulary — existing machinery; we write only the vocabulary and reference model.
  Stateright is reserved for one or two protocols where exhaustive beats random.
- A **safety-fold runner** over `resource_events`: one generic
  `fold(events, δ) → verdict`, sliced per object, using the declared transition
  tables as δ — the same fold usable as a test oracle now and a live monitor later.
  Accumulators must be reconstructible from snapshot + retained suffix, because
  event compaction truncates the prefix.
- A **per-read authority assertion** in the test backend (every read records which
  store resolved it; reads against non-authoritative stores fail the run) — built
  here, consumed by #1188 enactment.

Per-resource data, one small file each: the transition table, the field-ownership
table, safety predicates ("a lease is never released while its holder is pending
finalization", "a deleted object is never recreated by recovery", "a field owned by
role R is never written by role S"), and sometimes-style coverage assertions so
fixtures that never reach interesting states fail as vacuous.

### 5. Enrollment order

Machinery lands in the research's build order (ownership enforcement → vocabulary
with the week's bugs as hand-written failing sequences → differential oracles →
generated sequences → authority assertion → tables-as-monitor-δ). Resources enroll
convoy-first (most-transitioned, and #1232/#1233 will grow it — it must be a
checked machine before that growth), then Environment + MaterialPool lease (first
fully-enforced machine; violations destroy credentials), then TerminalSession, then
Checkout. PlacementPolicy is an ownership-first enrollee, refactoring #1236's
unified write helper into the declared form.

### 6. Explicitly rejected for now

Anvil-style formal verification (wrong order of magnitude at this size; we steal
its ESR statement, environment model, and proof-decomposition-as-test-decomposition
for free), madsim (determinism already achieved structurally), loom/shuttle/kani
for reconcilers (wrong granularity). Revisit when a protocol resists the harness.

## Consequences

- Every writer of resource state routes through the write helper; ad-hoc
  `update()` calls against lifecycle-bearing kinds become lint-able debt.
- The three dogfooding bug classes become regression *properties*, not regression
  tests: the exact sequences that produced #1236, #1242's lease review finding, and
  #1202 are the harness's first hand-written scenarios, and must fail before their
  fixes and pass after.
- Transition tables become the reference for design discussions (#1232's workflow
  growth extends the convoy table, not prose).
- Observe-mode telemetry gives the fleet a new legibility surface: "which writers
  attempted what against which rules" — the diagnosis that took a bait experiment
  this week becomes a query.
- Liveness claims stay bounded ("converges within N steps under this schedule"),
  and the harness says so rather than implying proof.
