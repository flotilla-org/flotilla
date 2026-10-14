# Prototype: Quint specs for the interacting machines (flotilla#3214, part of #3108)

Model-checked specifications of the four places in the primitives model where **ordering across hosts**
matters. They follow the research in #3109 (Quint as the language, TLC as the exhaustive back end) and take
their machine shapes from the engine prototype (PR #3216, ticket #3209) and the owner rulings on #3199,
#3209, #3210, #3211 and #3213. Like the engine prototype, they are artifacts to react to: the directory is
outside the Cargo workspace and no CI gate runs it.

| Spec | Interaction | What crosses a host boundary |
|---|---|---|
| [`convoy_deliver.qnt`](convoy_deliver.qnt) | the Convoy `deliver` gate over deliverable records (submission, retraction, acceptance) and change-request observations | CR observations replicate from a forge observer to the convoy's home; records arrive as messages |
| [`inbox_delivery.qnt`](inbox_delivery.qnt) | inbox delivery with receipts, link loss and consumer replacement | the inbox stream goes to the consumer's host; receipt rows come back on the AgentSession |
| [`ensure_bind.qnt`](ensure_bind.qnt) | Ensure `bind` / `unbind` / `retire` against a fulfiller that lives on another host and vanishes or is replaced | the fulfiller's lifecycle replicates to the ensure's home; `create` and `release` are routed calls |
| [`message_race.qnt`](message_race.qnt) | `drop` racing `settle`; `supersede` and expiry arriving after settle | receipt and reply rows replicate to the message's home; sender and timer requests arrive at any time |
| [`machines.qnt`](machines.qnt) | the declared edge tables (Message, shared lifecycle, Convoy, Ensure) | none: this is what a Rust `MachineDef` is compared with |

## Results in one screen

**222 checks, 0 deviating from expectation** (`results/results.json`; the race rows are from the rerun after the 2026-10-14 `settle`-from-`Pending` addition, the others from the original 202-check run in `results/run.txt`; 8 GiB container).
Every safety property holds in its declared design, every liveness property holds under its stated fairness, every mutant
is caught by the property aimed at it, and every witness is reachable.

| Spec | Checks | Largest base instance (distinct states) | Counterexamples that are findings |
|---|---:|---|---|
| convoy | 51 | 345,304 | vacuous delivery of an empty convoy |
| inbox | 62 | 7,873,022 (1 message, 2 sessions, every failure mode); 123,368 (2 messages, 1 session) | dropped-but-turned; drop races deliver at the home; double turn window; crash/receipt order; dead bound consumer |
| ensure | 34 | 27,392 | give-up livelock after `resume`; `Fulfilled` before the fulfiller can receive |
| race | 75 | 3,061,040 | an expiry loses a reply written in time; a turn can precede a settle-while-Pending |

Findings (details below): a counterexample is a result, so none of the properties was weakened to make a run pass.
Where a property fails in the declared design, the failure is expected in `cases.py` and a named variant shows the fix holding.

## Run it

```bash
# tools: Node (quint 0.33.x), a JRE 17+, python3. Quint downloads Apalache 0.62.1 (which carries TLC) to ~/.quint on first use.
npm i -g @informalsystems/quint@0.33.0
python3 check.py --list                    # every spec and case
python3 check.py convoy ensure race        # spec names (substring); about 5 minutes
python3 check.py inbox:base                # one case
python3 check.py inbox --props pendingLeaves,everyMessageEnds   # re-run chosen properties
python3 check.py --json results/results.json > results/run.txt  # everything: about one hour, 7 GiB of JVM heap
python3 tie/results_table.py results/results.json               # Markdown tables
# results/ and tie/pr3216-edges.jsonl are snapshots: regenerate them after any spec or case change.
# pr3216-edges.jsonl dumps the unmerged PR #3216 (13cc297d4); tie/dump_edges.rs only builds inside that crate.
quint test machines.qnt --main machines                         # the edge tables are well formed
python3 tie/compare_tables.py tie/pr3216-edges.jsonl            # tables vs the Rust engine of PR #3216
./deep_check.sh 8                                               # Apalache, 2 messages x 2 sessions, to a step bound
```

`check.py` writes one small instance module per case (`build/<spec>_<case>.qnt`, constants only), compiles it
with `quint compile --target tlaplus`, and runs TLC once **per property** with a generated `.cfg`
(`INIT q_init / NEXT q_step / INVARIANT|PROPERTY <property>`), so a mutant is attributed to exactly the property
it breaks. The manifest [`cases.py`](cases.py) holds the bounds, every mutant and the expected verdict of
every property; the run fails when any verdict deviates. TLC counterexamples land in `build/traces/`;
`python3 tie/trace_actions.py build/traces/<file> <field>...` condenses one to its action sequence.

## How to read a result

* **Safety properties** are state invariants (`val`). **Liveness** is TLC `PROPERTY` checking under explicit weak
  fairness. "Bounded" means bounded by the model: the bounds below, and "eventually" over a finite state graph, not a
  step counter. `quint verify` with Apalache (`deep_check.sh`) gives a true step bound at a larger size.
* **Witnesses** (`witness*`) are invariants that are *meant to fail*. A violated witness proves the situation is
  reachable in the bounds, so a `hold` on a safety property is not vacuous. A witness that stops failing is a bug in
  the model.
* **Mutants** (`mut_*`, and the named variants) switch one rule off through a `const`. Each must make exactly the
  property aimed at it fail. A property that no mutant can break is not evidence.
* **Fairness is stated, not assumed silently.** Each liveness property names the actions it needs to be weakly fair
  (a controller that can act does act eventually) and the environment assumption (for example "the link is eventually
  up for good"). Failure steps (a host dying, a fulfiller vanishing) are never forced.
* **A control for liveness.** Each liveness result has a negative control that must fail: remove the fairness or add
  the forbidden rule, and the property fails (`convergeWithoutReplicationFairness`, `mut_link_gate`,
  `mut_no_unbind_on_*`, `mut_no_release_from_requested`).

## Modelling seams shared by all four

* **Replication is an ordered stream applied as a prefix.** A replica holds a prefix of its home's history. It may
  skip versions (a coalesced delta or a fresh snapshot after compaction), stalls while the link is down, and resumes.
  In the inbox spec each consumer host has a queue of not-yet-applied versions, and applying k > 1 versions skips the
  rest. In the convoy and race specs the home applies a prefix of the observer's log. (#3213: one sequence per
  home; replicas apply rows and do not re-run machines.)
* **No host writes another host's resources.** The inbox home writes the inbox; the consumer's host writes receipt,
  failure and end rows on its own AgentSession. A remote *request* (sender's supersede, an ensure's `release`) is a
  routed call that is applied or refused on the target home's write path; its refusal is returned and recorded as an
  outcome.
* **Gates read only local, versioned inputs** (#3209): the replica prefix and the home's own records. No gate reads
  link age; `mut_link_gate` shows why (below).
* **Level-triggered reactions.** Controllers decide from their (possibly stale) view and the effect lands on the
  target's truth, so decision and effect can disagree. That race is what several findings come from.
* **Time is discrete ticks** where it matters (deadline, no-consumer threshold), synchronised across hosts. Clock skew
  is not modelled.

## 1. Convoy deliver gate (`convoy_deliver.qnt`)

**Scope.** `Open --deliver--> Delivered`, `Open --abandon--> Abandoned`. Deliverable records are folded per promise
from submission (names a change request), retraction and acceptance; they arrive at the convoy's home as messages
from two senders (the crew host, a governor) in arbitrary order. Change-request observations (`Open`, `Merged`,
`Closed`, reopenable, `Merged` final) are written by a forge observer host and reach the home only through its
replicated stream. "Kept" is a judgement over the home's view (latest submission names a CR the replica shows
`Merged`) and is never stored. The edge records what it relied on: the verdict per promise and the stream
version it read (`dIn`, `dVer`).

**Properties.**

| Property | Kind | Statement |
|---|---|---|
| `deliverOnlyWhenSettled` | safety | `Delivered` implies every promise present was kept or retracted-with-acceptance **and the recorded inputs are true**: the version the edge names really says `Merged` (not just the current replica) |
| `deliveredNonVacuous` | safety | a convoy is not delivered with zero promises |
| `endFreezesRecords` | safety | after `Delivered` or `Abandoned` the deliverable records no longer change (late writes are refused) |
| `waivedOnlyByGovernor` | safety | a retraction settles a promise only through an acceptance by an authorised party (the crew cannot accept its own retraction, #3094) |
| `abandonedByDecision` | safety | `Abandoned` always carries a decision by owner, governor or declared policy; nothing fails a convoy automatically |
| `endsAreFinal` | temporal | no edge leaves `Delivered` or `Abandoned` |
| `gateIsLive` | liveness (WF `deliver`) | a convoy cannot stay `Open` from some point on while the declared gate holds from that point on: the guard of `deliver` *is* the gate, with no hidden extra condition |
| `observationsConverge` | liveness (WF `deliver`, `replicate`) | if from some point on every promise is submitted against a CR that is truly `Merged` and the link stays up, the convoy leaves `Open` |

**Bounds.** 2 promise slots (1 declared at admission, 1 addable), 2 change requests, 2 submissions per promise, 4
observation changes, a link that may flap freely: **345,304 distinct states** (3.6 million for the
writes-after-end mutant).

**Mutants and the property that must catch each.** `mut_bare_retraction` (a retraction settles without acceptance) and
`mut_submission_is_kept` and `mut_self_accept`: `deliverOnlyWhenSettled`; `mut_self_accept` also
`waivedOnlyByGovernor`; `mut_writes_after_end`: `endFreezesRecords`; `mut_deliver_from_end`: `endsAreFinal`;
`mut_auto_fail`: `abandonedByDecision`; `vacuous` (no promises declared, empty-set gate): `deliveredNonVacuous`;
`mut_link_gate` (the gate also reads link liveness): `gateIsLive` fails, because a link that flaps for ever never
leaves `deliver` continuously enabled. That is the executable reason for the #3209 ruling "link age in gates: forbidden".

**Witnesses** (reachable): delivered, abandoned, delivered through a waiver, and a lagging view (a CR is merged in the
world, the home does not know yet).

## 2. Inbox delivery with receipts across two hosts (`inbox_delivery.qnt`)

**Scope.** One inbox with a consumer relation to at most one AgentSession at a time; two sessions, each on its own host
and link. The inbox home writes messages and the consumer relation; the consumer host pulls from *its replica*, hands
the agent the turn, then appends a receipt row on its AgentSession. The home records `deliver` only after it has applied
that row. Sessions can lose their link, end (an end row), have their adapter fail (a failure row), crash between turn
and receipt, or vanish with their host. The owner can replace a healthy consumer. Unsettled messages pass to the
replacement (`unbind` then `bind` of the other session; each session binds once). Messages expect nothing, so `deliver`
settles; Awaiting, settle, expiry and supersede are in `message_race.qnt`.

**Properties.**

| Property | Kind | Statement |
|---|---|---|
| `deliverAfterReceipt` | safety | a message is `Settled` only if a receipt from the recorded receiver exists |
| `deliveredImpliesTurned` | safety | and that session really handed the agent the turn (the receipt is not a lie) |
| `deliverOnce` | safety | the `deliver` edge fires at most once per message |
| `receiverTurned` | safety | the recorded receiver is the session that turned it, when only one did |
| `oneConsumer` | safety | at most one consumer is bound and each session binds once |
| `noDuplicateTurnPerSession` | safety | no session is handed the same message as a turn twice |
| `noDoubleTurnAfterObservedReceipt` | safety | **when is a second consumer allowed to receive the same message as a turn?** Only if the home had not yet observed the first consumer's receipt when the replacement bound. See finding 5 |
| `noDropWithObservedReceipt` | safety | the home never drops as undeliverable a message whose receipt it has observed |
| `droppedImpliesNeverTurned` | safety | a message dropped as undeliverable was never handed to any consumer (**fails by design**: finding 4) |
| `endsAreFinal` | temporal | no edge leaves `Settled` or `Dropped` |
| `pendingLeaves` | liveness | with the link eventually up for good and a live consumer bound for good, every `Pending` message eventually leaves `Pending` (**the bounded liveness the ticket asks for**) |
| `everyMessageEnds` | liveness | with no health assumption except that no link flaps for ever (up, or its host dead, from some point on), every message that was sent ends `Settled` or `Dropped` |

**Fairness.** weak fairness of: `deliver`, `dropUndeliverable`, `tick`, the two unbind reactions, `pull`, `pushRows`, the
adapter's `begin` and `finish`, and `writeEnd`. Not fair: link failures, host death, crashes, owner replacement.

**Bounds.** 2 sessions, **1 message** with every failure mode on (link flaps, host death, session end, adapter failure,
owner replacement, no-consumer threshold of 1 tick): **7,873,022 distinct states** (up to 24.9 million for the `crash` variant). A second instance, `two_messages` (123,368 states), has 2 messages and 1 consumer (ordering, drop of one while the
other settles). With one message and two sessions, replacement while unsettled is covered; with two messages and two
sessions plus every failure mode TLC exceeds the 8 GiB the crew container has, so that size is covered by Apalache
(`deep_check.sh`) to a step bound instead.

**Design options and mutants** (the verdicts are in the results table):
`mut_deliver_without_receipt` (deliver on bind, the push assumption): `deliverAfterReceipt`;
`mut_ends_not_final`: `endsAreFinal`, `deliverOnce`;
`no_drain` (`bind` does not wait for observed receipts to be recorded): `noDoubleTurnAfterObservedReceipt`;
`crash` (adapter dies between turn and receipt): `noDuplicateTurnPerSession`;
`receipt_first_crash` (the other order): `deliveredImpliesTurned`;
`drop_checks_receipt` (the fix for `noDropWithObservedReceipt`);
`requires_binding` (a receipt from a session that is no longer bound is ignored) and
`requires_binding_and_drop_check`;
`mut_no_unbind_on_end` and `mut_no_unbind_on_silence` (no reaction unbinds a consumer whose AgentSession ended, or
whose host is gone): `everyMessageEnds`.

## 3. Ensure bind and unbind against a fulfiller's lifecycle (`ensure_bind.qnt`)

**Scope.** Ensure: `Unfulfilled --bind--> Fulfilled --unbind{gone|replaced}--> Unfulfilled`, `--give up--> Held
--resume--> Unfulfilled`, any `--retire--> Retired`. Fulfillers are resources with their own home: the shared lifecycle
`Requested --realise--> Realised --vanish--> Gone`, `release` from `Requested` or `Realised`, `give up` from `Requested`;
each fulfiller identity is one resource and an end never reopens (no incarnations; replacement is a new resource). The
ensure sees a fulfiller only through a replica and acts on it by routed `create` / `release` calls, decided on the view and applied
on the truth. The ensure records every fulfiller it created before calling, so retire can release one that was never
bound. The owner can replace a healthy fulfiller; a fulfiller can vanish or give up spontaneously.

**Properties.**

| Property | Kind | Statement |
|---|---|---|
| `boundConsistent` | safety | `Fulfilled` implies a bound fulfiller the ensure created; only `Fulfilled` or `Retired` keep a binding |
| `atMostOneLive` | safety | at most one live (`Requested` or `Realised`) fulfiller per ensure: old out, new in, never both |
| `noOrphanAfterUnbind` | safety | a fulfiller that was bound and is not any more is not live (unbind never orphans it) |
| `noCreateAfterRetire` | safety | a `Retired` ensure creates nothing |
| `fulfilledImpliesRealised` | safety | **the open question**: `Fulfilled` means the fulfiller can receive (was `Realised` when bound) |
| `endsAreFinal` | temporal | `Retired`, `Gone`, `GaveUp` never change |
| `retireReleases` | liveness | a `Retired` ensure eventually has no live fulfiller **even if provisioning hangs** (realise and give-up are not assumed fair) |
| `fulfilledGoneUnbinds` | liveness | a bound fulfiller that is `Gone` or `GaveUp` is eventually unbound |
| `fulfilmentConverges` | liveness | if failures stop, links stay up and the ensure is neither retired nor replacing, it ends up `Fulfilled` with a fulfiller that can receive |

**Bounds.** 4 fulfiller identities (enough for 2 failures, 1 replacement and the survivor), at most 2 spontaneous
failures, 1 replacement, give-up after 2 failed fulfillers, per-fulfiller links that flap: **27,392 distinct states**.

**Variants and mutants.** `bind_on_creation` (the open question's other answer): `fulfilledImpliesRealised`;
`resume_keeps_history`: `fulfilmentConverges` (finding 2);
`mut_no_release_from_requested`: `retireReleases`; `mut_retire_releases_bound_only`: `retireReleases`;
`mut_replace_without_release`: `noOrphanAfterUnbind`; `mut_double_create`: `atMostOneLive`;
`mut_create_after_retire`: `noCreateAfterRetire`.

## 4. Message: drop racing settle (`message_race.qnt`)

**Scope.** One message with the four-state machine and `deliver`, `settle`, `drop{undeliverable | expired | superseded}`.
`settle` has two forms (owner ruling, 2026-10-14): *answered* (from `Awaiting`, gate: the reply was observed) and *outcome
observed* (from `Pending` or `Awaiting`, gate: the expected outcome was observed at the home; for example a demand to fix a
failing build, where the build goes green first). The outcome is a home-local observation (it needs no link). From
`Pending` the consumer is never handed the message after the settle is applied.
The consumer's host writes a receipt, a reply and a tick mark per tick to its stream; the home applies a prefix of it.
Six request kinds (`deliver`, `settle`, `observe`, `expire`, `supersede`, `undeliverable`) reach the home at any time, each
applied or **refused with a recorded reason** (`RefusedEnded` for a late request on an end, `RefusedGate` when the gate
does not hold, retried level-triggered). A newer message from the sender exists or not; delivery evidence toggles.

**Properties.**

| Property | Kind | Statement |
|---|---|---|
| `oneWinner` | safety | exactly one end: `Settled` has no drop reason, `Dropped` has exactly one |
| `arrivedAreResolved` | safety | every request that arrived has a recorded outcome: a late one is refused visibly, never silently ignored |
| `outcomesTruthful` | safety | `RefusedEnded` only on an end; `Applied` matches the message state |
| `dropReasonSound` | safety | each drop reason was justified by what the home had applied: undeliverable only from `Pending`; expired only after the deadline and with neither reply nor outcome observed; superseded only with a newer message naming this one |
| `settledHasEvidence` | safety | `Settled` is justified by the edge that took it there: the receipt (expects nothing), the reply (answered, from `Awaiting`) or the observed outcome (only a message that expects one) |
| `noDeliverOnPendingSettled` | safety | a message settled while `Pending` by an observed outcome is never recorded as delivered, whatever receipt arrives later: the late `deliver` is refused with `RefusedEnded` |
| `noInTimeReplyLost` | safety | a reply written in time is never lost to an expiry (**fails in the declared design**: finding 1) |
| `endsAreFinal`, `whyIsFinal` | temporal | no edge leaves an end; a drop reason never changes |
| `everyMessageEnds` | liveness | with the link eventually up for good, the message ends |
| `endsDespitePartition` | liveness | the same without any link assumption |

**Bounds.** deadline at tick 1, 2 ticks (3 ticks with the outcome variable is 8.6M states and made the temporal checks impractical; the one-tick-later deadline passes within the bound, so every gate is exercised), link that flaps, newer message and delivery evidence toggling freely: **3,061,040
distinct states**.

**Mutants.** `mut_drop_from_any` (drop not refused from an end): `endsAreFinal`; `mut_silent_refusal`:
`arrivedAreResolved`; `mut_expire_ignores_deadline`, `mut_expire_ignores_reply`, `mut_undeliverable_anywhere`,
`mut_supersede_unchecked`: `dropReasonSound`; `mut_expire_ignores_outcome` (expiry although the outcome was observed):
`dropReasonSound`; `mut_deliver_from_any` (a late receipt re-opens an ended message): `endsAreFinal`, `oneWinner`,
`noDeliverOnPendingSettled`; `mut_observe_from_any`: `endsAreFinal`, `oneWinner`; `mut_observe_unexpected` (outcome
settle for a message that expects nothing): `settledHasEvidence`. The witnesses `witnessLate{Deliver,Settle,Expire,Supersede}Refused` show the
late-refusal paths are reachable: **supersede and expiry arriving after settle are refused, with a visible reason**.


## Findings for the design

Ranked by how much a design decision depends on them. Each is a counterexample TLC printed, minimised (TLC reports the
shortest path for a safety property; liveness lassos were shortened by hand, noted below).

**1. Expiry judged on the home's view can lose a reply written in time** (`race:base`, `noInTimeReplyLost` fails; 6 steps).

```text
tick                  deadline D = 1: the replier's stream gets mark 1
cReceipt, cReply      the consumer writes receipt and reply at tick 1: in time
tick                  now = 2 > D
request(expire)       the home has applied none of the stream (spos = 0): reply not observed -> Dropped(expired)
```

The `expired` gate reads only the home's replica, so replication lag alone beats a reply that was written before the
deadline. Visible, not silent (the later `settle` is refused `RefusedEnded`), but the sender is told "expired" for a message
that was answered. The variant `race:watermark` gates `expired` on having applied the replier's stream **past the
deadline** (the replier's tick mark `D+1`, the keepalive the stream already carries, so it is a local versioned input and
allowed under the "no link age in gates" ruling). Then `noInTimeReplyLost` holds. The price is stated by
`endsDespitePartition`, which then **fails**: during a permanent partition the message never expires. Decision for the
owner: accept the lost-reply window (and say so in the Dropped reason), or take the watermark gate and let a partitioned
Awaiting message escalate by another route (a Demand, a human `drop`).

**2. `give up` after `resume` livelocks if the gate counts all history** (`ensure:resume_keeps_history`,
`fulfilmentConverges` fails). Lasso, from the printed trace: two fulfillers give up, the ensure becomes `Held`, `resume`
returns it to `Unfulfilled`, and the give-up gate still sees two `GaveUp` fulfillers, so it goes straight back to `Held`
without trying a new one; `create` is never continuously enabled, so fairness cannot force it. The attempt history has to
start a **new window at each `resume`** (`RESUME_STARTS_NEW_WINDOW`, the base). This follows from "attempts are a history";
the window boundary is the part the rulings leave unstated.

**3. A convoy with no deliverables is deliverable** (`convoy:vacuous`, `deliveredNonVacuous` fails; 2 steps: `init`,
`deliver`). "All promises settled" over an empty set is true. Either admission must declare at least one deliverable or the
gate must refuse an empty convoy.

**4. `Dropped(undeliverable)` does not mean "never delivered"** (`inbox:base`, `droppedImpliesNeverTurned` fails; 8 steps).

```text
send; bind s1; unbindReplace      owner replaces s1 (nothing was turned yet)
tick; dropUndeliverable           no consumer for the threshold: Dropped
pull (the intermediate version)   s1's host applies the version where it was still bound and the message Pending
begin                             s1 hands the agent the turn for a message the home dropped
```

The consumer acts on a replica that is behind, so a turn can follow a drop. The late receipt is refused (the message is
an end). The sender's "undeliverable" and the dead letter must read "no receipt was confirmed within the policy", not "the
agent never saw it". A consumer-side fence ("act only on a replica whose link is fresh") narrows the window but cannot close
it without a synchronous check, which the pull design deliberately avoids.

**5. Related race at the home: a message can be dropped after its receipt was observed** (`noDropWithObservedReceipt` fails
in `inbox:base`; 9 steps: a delivery-failure row, a retry that succeeds and writes a receipt, both rows applied, then
`dropUndeliverable` wins over `deliver`). The drop gate must also require that no receipt was observed
(`DROP_NEEDS_NO_RECEIPT`; `inbox:drop_checks_receipt` holds all properties).

**6. When may two consumers be handed the same message as a turn?** Never, except in a window whose edge the model shows:
the first consumer's receipt was **not yet observed by the home** when the replacement bound (`noDoubleTurnAfterObservedReceipt`).
The double turn is reachable (witness, 9 steps: `bind s1`, `unbindReplace`, `bind s2`, both hosts `pull`, both `begin`)
and is acceptable because the home cannot tell "s1 received it and died" from "s1 never did": delivery is at-least-once
across a replacement, with the `deliver` edge recording exactly one receiver and a duplicate refused. Two things keep the
window to that and no wider:
`bind` must wait until every observed receipt has its `deliver` edge (`BIND_DRAINS`; without it `inbox:no_drain` fails in 11
steps: s1 turns, the home *sees* the receipt, but the owner's unbind/bind overtakes the pending `deliver` and s2 turns it
too), and `deliver` should accept a receipt from a session that is no longer bound (`DELIVER_REQUIRES_BINDING = false`;
`inbox:requires_binding` shows the alternative drops a message whose receipt it saw, and combined with finding 5's fix it
leaves a message stuck for ever). The adapter must dedupe by message id.

**7. Receipt order inside the consumer** (`inbox:crash`, `inbox:receipt_first_crash`). Turn then receipt (the #3199 order)
duplicates the turn to the same session when the adapter crashes between them (`noDuplicateTurnPerSession` fails; 7 steps)
but never records a delivery that did not happen. Receipt then turn never duplicates, but records `deliver` **before
the turn happens**, and loses the turn if the adapter dies (`deliveredImpliesTurned` fails; 7 steps, no crash needed).
The ruled order is the right one; it needs idempotent turns.

**8. A bound consumer that died blocks the inbox for ever unless something unbinds it** (`mut_no_unbind_on_end`,
`mut_no_unbind_on_silence`; `everyMessageEnds` fails in 8-9 steps). The declared machines give `drop(undeliverable)` two
evidence paths, "no consumer bound" and "a failure row"; neither fires for a bound consumer whose AgentSession ended or whose
host is gone. The base passes only because two reactions exist and are fair: **unbind when the AgentSession's end row is
observed** and **unbind when the consumer's host link is silent** (a reaction on a judgement, never a gate). They have to be
specified as engine reactions. `pendingLeaves`, the ticket's bounded liveness, holds under its stated condition without them.

**9. Ensure: bind waits for Realised, release must work from Requested, retire must release every created fulfiller.**
* `bind_on_creation` (the open question's other answer): only `fulfilledImpliesRealised` fails (4 steps: create, observe
  `Requested`, bind). Safety and liveness are otherwise unchanged, so this is about what `Fulfilled` promises, and the proposed
  "bind when the fulfiller can receive" is the one that keeps `Fulfilled` honest.
* `mut_no_release_from_requested`: a retired ensure whose fulfiller hangs in `Requested` keeps it (hand-minimised lasso:
  `create`, `retire`, `observe`, then nothing happens for ever; `retireReleases` fails). This confirms the owner ruling that
  `release` is also allowed from `Requested`.
* `mut_retire_releases_bound_only`: a created-but-never-bound fulfiller leaks. The ensure must record what it created, before
  calling, and retire releases that set.
* `mut_replace_without_release` and `mut_double_create` show that replacement is "release old, see it gone, then create new".

**10. No link age in gates** (`convoy:mut_link_gate`): a `deliver` gate that also needs a live link never fires on a link
that flaps, although the declared gate holds throughout (`gateIsLive` fails). This is the executable form of the #3209 ruling.

**12. Settle-while-Pending (owner, 2026-10-14): a late receipt cannot record `deliver`, but a turn can precede the settle** (`race:base`).
A message that expects an outcome settles straight from `Pending` when the outcome is observed at the home.
`noDeliverOnPendingSettled` holds: the late `deliver` request is refused (`RefusedEnded`) and the message is never recorded
as delivered (`witnessLateReceiptRefused`, 6 steps: `cReceipt`, `outcomeObserved`, settle request, deliver request refused,
`replicate`). `mut_deliver_from_any` (deliver not refused from an end) breaks it, and also `endsAreFinal` and `oneWinner`.
The claim "the consumer never sees a turn for a message settled while Pending" holds only **once the settle has replicated to
the consumer**. `witnessTurnBeforeSettle` (4 steps: the consumer writes its receipt, the outcome is observed, the home settles)
shows a turn that predates the settle; the home cannot know about it, and the receipt is then ignored with a visible refusal.
The design must read "never" as "never after the settle is visible to the consumer", and the consumer-side fence (act only on
a replica where the message is not an end) narrows the window without closing it. Two gates were needed: expiry is
refused once the expected outcome is observed (`mut_expire_ignores_outcome` breaks `dropReasonSound`), and an outcome settles
only a message that expects one (`mut_observe_unexpected` breaks `settledHasEvidence`). A first model that let any observed
outcome block expiry stranded a message that expects nothing (`endsDespitePartition` failed in `expects_nothing`): the
outcome gate must be conditional on the expectation.

**11. The Rust prototype predates the `release`-from-`Requested` and `settle`-from-`Pending` rulings** (see "Tying the specs", below).

## Tying the specs to the Rust step functions

The research (#3109) recommends two links; this prototype builds the first at small scale and specifies the second.

**1. Full-graph edge conformance (built, for the declared tables).** [`machines.qnt`](machines.qnt) states the edge
tables of Message, the shared lifecycle, Convoy and Ensure as data, and checks them against the legibility rules the
engine's `validate()` enforces (no edge leaves an end, every state reachable, every non-end has an exit, names disjoint;
`quint test machines.qnt --main machines`, including a negative control).
[`tie/compare_tables.py`](tie/compare_tables.py) compares those tables with a JSON dump of a Rust `MachineDef`
([`tie/dump_edges.rs`](tie/dump_edges.rs) is the dump program, run as an `example` of the PR #3216 crate). Run against
PR #3216 at `13cc297d4`:

```text
message: DIFFERS
  spec has, Rust lacks : Pending --settle--> Settled
lifecycle(Clone): DIFFERS
  spec has, Rust lacks : Requested --release--> Gone
lifecycle(ImageBuild): DIFFERS
  spec has, Rust lacks : Requested --release--> Gone
convoy: no Rust machine yet (spec table only)
ensure: no Rust machine yet (spec table only)
```

This is the check working as designed: the prototype engine predates two owner rulings, that `release` is also allowed from
`Requested` and (2026-10-14) that `settle` is also allowed from `Pending` when the expected outcome is observed before
delivery, and the comparison finds exactly that drift, in the direction a replay-only check cannot see (an edge the
spec has and the code lacks is found by replay; an edge only the code has is not). The `compare` script reports both
directions. In the real engine the dump is `MachineDef::edges()` over each registered machine, run as a `cargo test`;
`compare_tables.py` evaluates the tables with `quint repl`, so no one copies them by hand.

**2. The interaction properties become checks on the Rust side in two ways** (to build when the engine lands):

* *Gate and guard parity.* Every spec action is one engine edge whose guard is the gate in the declaration
  (`deliver` reads the replica prefix; `drop(undeliverable)` reads a failure row or the no-consumer threshold). The
  mutants are the test plan: each is a gate the Rust code must not have, and the property that catches it names the
  engine test that must fail when the gate is removed (mutation check).
* *Trace replay for the interactions.* `quint run --mbt --out-itf` over these specs generates traces of the multi-host
  scenarios; `quint-connect` replays them through the `InProcessDaemon`-level harness (the same kind of scenario the
  router tests use), comparing the abstract state after each step. Replay is the secondary check because it cannot
  see an over-permissive implementation (#3109); the table comparison above is the primary one. The specs' action names
  (`deliver`, `settle`, `drop`, `bind`, `unbind`, `retire`, `realise`, `release`, `vanish`) are the engine's edge names, so
  the driver maps one-to-one.

The abstraction function for the interaction specs is small and stated in each spec header: state of the machine, the
replica prefix the home has applied, and the outcome recorded for each request.


## What is not modelled

* **Bounds, not proofs for all sizes.** TLC is exhaustive inside the bounds above: 1 message with 2 sessions; 2 messages
  with 1 session; 1 convoy with 2 promises and 2 CRs; 4 fulfiller identities. Apalache (`deep_check.sh`) checks the
  inbox at 2 messages and 2 sessions to a step bound; in the crew container it reached depth 7 for all seven invariants and
  depth 8 for six of them within the five minutes allowed, with no violation. That is a smoke test of the larger size, not a result.
* **Symmetry is not exploited.** Sessions are bound in id order (`ORDERED_BINDS`) which removes mirrored duplicates; the
  other specs make no symmetry reduction.
* **One consumer per inbox, one home per resource.** Consumer pools, role inboxes with dynamic names and wildcard
  delivery are later (#3199). Convoy membership changes (old member out, new in) are an input to Ensure in the real
  system; the convoy spec treats the promise set and the CR observations only.
* **Delivery modes (turn / notify / mute) change the transport, not the machine** (#3210); the model has one receipt
  kind. A `muted` receipt is written immediately by the same actor, so it adds no new interleavings.
* **No clock skew; no stale-keepalive time.** Tick marks assume the replier's tick equals the home's tick. The link is
  a boolean; a keepalive past a threshold (staleness) is a judgement and, per the rulings, never a gate input.
* **Not modelled:** the per-promise result cache keyed by input version in the convoy home (a cache key that misses the
  promise *set* would deliver on a stale aggregate; the spec re-evaluates the gate on the write path as the ruling
  says), authorisation beyond "crew vs governor", the dead-letter message to the sender's supervisor (a reaction to
  `drop(undeliverable)`, outside the engine), and archival of receipts.
* **Fairness assumptions are mine.** Each liveness property lists the actions assumed weakly fair. The open question
  from #3109 ("which fairness assumptions can flotilla state?") is answered here for these four machines, per property,
  and not for the daemon as a whole.


## Results (every check)

Generated by `python3 tie/results_table.py results/results.json`. Every row matches its expectation.

#### convoy

| case | property | expected | result | states | seconds |
|---|---|---|---|---:|---:|
| `base` | `deliverOnlyWhenSettled` | hold | hold (ok) | 345,304 | 3.3 |
| `base` | `deliveredNonVacuous` | hold | hold (ok) | 345,304 | 3.2 |
| `base` | `endFreezesRecords` | hold | hold (ok) | 345,304 | 3.2 |
| `base` | `waivedOnlyByGovernor` | hold | hold (ok) | 345,304 | 2.3 |
| `base` | `abandonedByDecision` | hold | hold (ok) | 345,304 | 2.6 |
| `base` | `endsAreFinal` | hold | hold (ok) | 345,304 | 20.0 |
| `base` | `gateIsLive` | hold | hold (ok) | 345,304 | 10.1 |
| `base` | `observationsConverge` | hold | hold (ok) | 345,304 | 6.5 |
| `base` | `witnessDelivered` | violated | violated (ok) | 983 | 0.6 |
| `base` | `witnessAbandoned` | violated | violated (ok) | 24 | 0.5 |
| `base` | `witnessDeliveredByWaiver` | violated | violated (ok) | 987 | 0.4 |
| `base` | `witnessLaggingView` | violated | violated (ok) | 68 | 0.4 |
| `base` | `convergeWithoutReplicationFairness` | violated | violated (ok) | 179,810 | 3.4 |
| `vacuous` | `deliverOnlyWhenSettled` | hold | hold (ok) | 381,632 | 2.9 |
| `vacuous` | `deliveredNonVacuous` | violated | violated (ok) | 192 | 0.4 |
| `vacuous` | `endFreezesRecords` | hold | hold (ok) | 381,632 | 2.5 |
| `vacuous` | `waivedOnlyByGovernor` | hold | hold (ok) | 381,632 | 2.7 |
| `vacuous` | `abandonedByDecision` | hold | hold (ok) | 381,632 | 3.4 |
| `mut_bare_retraction` | `deliverOnlyWhenSettled` | violated | violated (ok) | 316 | 0.4 |
| `mut_bare_retraction` | `deliveredNonVacuous` | hold | hold (ok) | 398,740 | 3.4 |
| `mut_bare_retraction` | `endFreezesRecords` | hold | hold (ok) | 398,740 | 3.7 |
| `mut_bare_retraction` | `waivedOnlyByGovernor` | hold | hold (ok) | 398,740 | 2.9 |
| `mut_bare_retraction` | `abandonedByDecision` | hold | hold (ok) | 398,740 | 2.5 |
| `mut_submission_is_kept` | `deliverOnlyWhenSettled` | violated | violated (ok) | 113 | 0.4 |
| `mut_submission_is_kept` | `deliveredNonVacuous` | hold | hold (ok) | 413,264 | 2.5 |
| `mut_submission_is_kept` | `endFreezesRecords` | hold | hold (ok) | 413,264 | 2.6 |
| `mut_submission_is_kept` | `waivedOnlyByGovernor` | hold | hold (ok) | 413,264 | 2.9 |
| `mut_submission_is_kept` | `abandonedByDecision` | hold | hold (ok) | 413,264 | 3.9 |
| `mut_self_accept` | `deliverOnlyWhenSettled` | violated | violated (ok) | 948 | 0.4 |
| `mut_self_accept` | `deliveredNonVacuous` | hold | hold (ok) | 587,812 | 4.6 |
| `mut_self_accept` | `endFreezesRecords` | hold | hold (ok) | 587,812 | 4.4 |
| `mut_self_accept` | `waivedOnlyByGovernor` | violated | violated (ok) | 137 | 0.4 |
| `mut_self_accept` | `abandonedByDecision` | hold | hold (ok) | 587,812 | 5.9 |
| `mut_writes_after_end` | `deliverOnlyWhenSettled` | hold | hold (ok) | 3,661,522 | 32.2 |
| `mut_writes_after_end` | `deliveredNonVacuous` | hold | hold (ok) | 3,661,522 | 31.9 |
| `mut_writes_after_end` | `endFreezesRecords` | violated | violated (ok) | 204 | 0.4 |
| `mut_writes_after_end` | `waivedOnlyByGovernor` | hold | hold (ok) | 3,661,522 | 30.8 |
| `mut_writes_after_end` | `abandonedByDecision` | hold | hold (ok) | 3,661,522 | 28.3 |
| `mut_deliver_from_end` | `endsAreFinal` | violated | violated (ok) | 2,051 | 0.5 |
| `mut_auto_fail` | `deliverOnlyWhenSettled` | hold | hold (ok) | 425,296 | 3.7 |
| `mut_auto_fail` | `deliveredNonVacuous` | hold | hold (ok) | 425,296 | 2.6 |
| `mut_auto_fail` | `endFreezesRecords` | hold | hold (ok) | 425,296 | 3.1 |
| `mut_auto_fail` | `waivedOnlyByGovernor` | hold | hold (ok) | 425,296 | 4.4 |
| `mut_auto_fail` | `abandonedByDecision` | violated | violated (ok) | 356 | 0.4 |
| `mut_link_gate` | `gateIsLive` | violated | violated (ok) | 156,279 | 3.5 |
| `mut_link_gate` | `endsAreFinal` | hold | hold (ok) | 345,304 | 19.8 |
| `mut_link_gate` | `deliverOnlyWhenSettled` | hold | hold (ok) | 345,304 | 2.8 |
| `mut_link_gate` | `deliveredNonVacuous` | hold | hold (ok) | 345,304 | 2.3 |
| `mut_link_gate` | `endFreezesRecords` | hold | hold (ok) | 345,304 | 2.9 |
| `mut_link_gate` | `waivedOnlyByGovernor` | hold | hold (ok) | 345,304 | 2.2 |
| `mut_link_gate` | `abandonedByDecision` | hold | hold (ok) | 345,304 | 3.5 |

#### inbox

| case | property | expected | result | states | seconds |
|---|---|---|---|---:|---:|
| `base` | `deliverAfterReceipt` | hold | hold (ok) | 7,873,022 | 67.8 |
| `base` | `deliveredImpliesTurned` | hold | hold (ok) | 7,873,022 | 65.2 |
| `base` | `deliverOnce` | hold | hold (ok) | 7,873,022 | 60.7 |
| `base` | `noDuplicateTurnPerSession` | hold | hold (ok) | 7,873,022 | 65.6 |
| `base` | `noDoubleTurnAfterObservedReceipt` | hold | hold (ok) | 7,873,022 | 67.3 |
| `base` | `receiverTurned` | hold | hold (ok) | 7,873,022 | 64.6 |
| `base` | `oneConsumer` | hold | hold (ok) | 7,873,022 | 64.9 |
| `base` | `endsAreFinal` | hold | hold (ok) | 7,873,022 | 516.5 |
| `base` | `pendingLeaves` | hold | hold (ok) | 7,873,022 | 406.7 |
| `base` | `everyMessageEnds` | hold | hold (ok) | 7,873,022 | 430.9 |
| `base` | `droppedImpliesNeverTurned` | violated | violated (ok) | 4,188 | 0.6 |
| `base` | `noDropWithObservedReceipt` | violated | violated (ok) | 20,577 | 0.7 |
| `base` | `witnessDelivered` | violated | violated (ok) | 8,440 | 0.7 |
| `base` | `witnessDropped` | violated | violated (ok) | 327 | 0.4 |
| `base` | `witnessDoubleTurn` | violated | violated (ok) | 12,616 | 0.7 |
| `base` | `witnessZombieTurn` | violated | violated (ok) | 607 | 0.4 |
| `base` | `witnessReplacementDelivery` | violated | violated (ok) | 30,591 | 0.8 |
| `drop_checks_receipt` | `deliverAfterReceipt` | hold | hold (ok) | 6,708,229 | 56.6 |
| `drop_checks_receipt` | `deliveredImpliesTurned` | hold | hold (ok) | 6,708,229 | 58.3 |
| `drop_checks_receipt` | `deliverOnce` | hold | hold (ok) | 6,708,229 | 52.0 |
| `drop_checks_receipt` | `noDuplicateTurnPerSession` | hold | hold (ok) | 6,708,229 | 52.9 |
| `drop_checks_receipt` | `noDoubleTurnAfterObservedReceipt` | hold | hold (ok) | 6,708,229 | 57.6 |
| `drop_checks_receipt` | `receiverTurned` | hold | hold (ok) | 6,708,229 | 59.2 |
| `drop_checks_receipt` | `oneConsumer` | hold | hold (ok) | 6,708,229 | 54.0 |
| `drop_checks_receipt` | `endsAreFinal` | hold | hold (ok) | 6,708,229 | 445.9 |
| `drop_checks_receipt` | `pendingLeaves` | hold | hold (ok) | 6,708,229 | 360.5 |
| `drop_checks_receipt` | `everyMessageEnds` | hold | hold (ok) | 6,708,229 | 384.3 |
| `drop_checks_receipt` | `noDropWithObservedReceipt` | hold | hold (ok) | 6,708,229 | 52.4 |
| `drop_checks_receipt` | `droppedImpliesNeverTurned` | violated | violated (ok) | 4,075 | 0.5 |
| `two_messages` | `deliverAfterReceipt` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `deliveredImpliesTurned` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `deliverOnce` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `noDuplicateTurnPerSession` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `receiverTurned` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `oneConsumer` | hold | hold (ok) | 123,368 | 1.4 |
| `two_messages` | `endsAreFinal` | hold | hold (ok) | 123,368 | 19.3 |
| `two_messages` | `pendingLeaves` | hold | hold (ok) | 123,368 | 8.1 |
| `two_messages` | `everyMessageEnds` | hold | hold (ok) | 123,368 | 8.2 |
| `two_messages` | `witnessDelivered` | violated | violated (ok) | 730 | 0.5 |
| `two_messages` | `witnessDropped` | violated | violated (ok) | 78 | 0.4 |
| `mut_deliver_without_receipt` | `deliverAfterReceipt` | violated | violated (ok) | 161 | 0.4 |
| `mut_ends_not_final` | `endsAreFinal` | violated | violated (ok) | 120,961 | 3.3 |
| `mut_ends_not_final` | `deliverOnce` | violated | violated (ok) | 21,581 | 1.0 |
| `no_drain` | `noDoubleTurnAfterObservedReceipt` | violated | violated (ok) | 89,510 | 1.3 |
| `no_drain` | `deliverAfterReceipt` | hold | hold (ok) | 9,631,629 | 73.2 |
| `crash` | `noDuplicateTurnPerSession` | violated | violated (ok) | 2,454 | 0.6 |
| `crash` | `deliveredImpliesTurned` | hold | hold (ok) | 24,880,260 | 212.6 |
| `crash` | `deliverAfterReceipt` | hold | hold (ok) | 24,880,260 | 215.7 |
| `receipt_first_crash` | `deliveredImpliesTurned` | violated | violated (ok) | 2,454 | 1.1 |
| `receipt_first_crash` | `noDuplicateTurnPerSession` | hold | hold (ok) | 16,366,221 | 119.4 |
| `receipt_first_crash` | `deliverAfterReceipt` | hold | hold (ok) | 16,366,221 | 123.3 |
| `requires_binding` | `deliverAfterReceipt` | hold | hold (ok) | 6,602,062 | 46.8 |
| `requires_binding` | `noDoubleTurnAfterObservedReceipt` | hold | hold (ok) | 6,602,062 | 48.8 |
| `requires_binding` | `noDropWithObservedReceipt` | violated | violated (ok) | 15,918 | 0.6 |
| `requires_binding` | `pendingLeaves` | hold | hold (ok) | 6,602,062 | 395.4 |
| `requires_binding_and_drop_check` | `noDropWithObservedReceipt` | hold | hold (ok) | 5,437,269 | 59.2 |
| `requires_binding_and_drop_check` | `pendingLeaves` | hold | hold (ok) | 5,437,269 | 333.9 |
| `requires_binding_and_drop_check` | `everyMessageEnds` | violated | violated (ok) | 1,753,009 | 69.2 |
| `mut_no_unbind_on_end` | `everyMessageEnds` | violated | violated (ok) | 84,791 | 3.5 |
| `mut_no_unbind_on_end` | `pendingLeaves` | hold | hold (ok) | 7,873,022 | 424.7 |
| `mut_no_unbind_on_silence` | `everyMessageEnds` | violated | violated (ok) | 55,312 | 3.5 |
| `mut_no_unbind_on_silence` | `pendingLeaves` | hold | hold (ok) | 7,873,022 | 428.3 |

#### ensure

| case | property | expected | result | states | seconds |
|---|---|---|---|---:|---:|
| `base` | `boundConsistent` | hold | hold (ok) | 27,392 | 0.9 |
| `base` | `atMostOneLive` | hold | hold (ok) | 27,392 | 0.7 |
| `base` | `noCreateAfterRetire` | hold | hold (ok) | 27,392 | 0.7 |
| `base` | `fulfilledImpliesRealised` | hold | hold (ok) | 27,392 | 0.9 |
| `base` | `noOrphanAfterUnbind` | hold | hold (ok) | 27,392 | 0.8 |
| `base` | `endsAreFinal` | hold | hold (ok) | 27,392 | 3.1 |
| `base` | `retireReleases` | hold | hold (ok) | 27,392 | 2.0 |
| `base` | `fulfilledGoneUnbinds` | hold | hold (ok) | 27,392 | 1.7 |
| `base` | `fulfilmentConverges` | hold | hold (ok) | 27,392 | 1.5 |
| `base` | `witnessFulfilled` | violated | violated (ok) | 295 | 0.4 |
| `base` | `witnessReplaced` | violated | violated (ok) | 404 | 0.4 |
| `base` | `witnessHeld` | violated | violated (ok) | 1,265 | 0.5 |
| `base` | `witnessRetiredFromFulfilled` | violated | violated (ok) | 450 | 0.4 |
| `base` | `witnessRealisedAfterRetire` | violated | violated (ok) | 161 | 0.4 |
| `base` | `witnessStaleFulfilled` | violated | violated (ok) | 397 | 0.4 |
| `resume_keeps_history` | `fulfilmentConverges` | violated | violated (ok) | 24,896 | 1.4 |
| `resume_keeps_history` | `boundConsistent` | hold | hold (ok) | 24,896 | 0.7 |
| `resume_keeps_history` | `atMostOneLive` | hold | hold (ok) | 24,896 | 0.7 |
| `resume_keeps_history` | `retireReleases` | hold | hold (ok) | 24,896 | 1.8 |
| `bind_on_creation` | `fulfilledImpliesRealised` | violated | violated (ok) | 242 | 0.4 |
| `bind_on_creation` | `boundConsistent` | hold | hold (ok) | 136,784 | 1.7 |
| `bind_on_creation` | `atMostOneLive` | hold | hold (ok) | 136,784 | 1.6 |
| `bind_on_creation` | `retireReleases` | hold | hold (ok) | 136,784 | 8.3 |
| `bind_on_creation` | `fulfilmentConverges` | hold | hold (ok) | 136,784 | 5.3 |
| `mut_no_release_from_requested` | `retireReleases` | violated | violated (ok) | 25,040 | 1.6 |
| `mut_no_release_from_requested` | `atMostOneLive` | hold | hold (ok) | 25,040 | 0.7 |
| `mut_no_release_from_requested` | `fulfilmentConverges` | hold | hold (ok) | 25,040 | 1.4 |
| `mut_replace_without_release` | `noOrphanAfterUnbind` | violated | violated (ok) | 443 | 0.4 |
| `mut_replace_without_release` | `boundConsistent` | hold | hold (ok) | 12,080 | 0.7 |
| `mut_double_create` | `atMostOneLive` | violated | violated (ok) | 78 | 0.4 |
| `mut_retire_releases_bound_only` | `retireReleases` | violated | violated (ok) | 19,536 | 1.4 |
| `mut_retire_releases_bound_only` | `atMostOneLive` | hold | hold (ok) | 19,536 | 0.6 |
| `mut_create_after_retire` | `noCreateAfterRetire` | violated | violated (ok) | 180 | 0.4 |
| `mut_create_after_retire` | `boundConsistent` | hold | hold (ok) | 138,608 | 1.7 |

#### race

| case | property | expected | result | states | seconds |
|---|---|---|---|---:|---:|
| `base` | `oneWinner` | hold | hold (ok) | 3,061,040 | 38.9 |
| `base` | `arrivedAreResolved` | hold | hold (ok) | 3,061,040 | 38.5 |
| `base` | `outcomesTruthful` | hold | hold (ok) | 3,061,040 | 37.9 |
| `base` | `dropReasonSound` | hold | hold (ok) | 3,061,040 | 37.0 |
| `base` | `settledHasEvidence` | hold | hold (ok) | 3,061,040 | 37.9 |
| `base` | `noDeliverOnPendingSettled` | hold | hold (ok) | 3,061,040 | 38.4 |
| `base` | `endsAreFinal` | hold | hold (ok) | 3,061,040 | 338.2 |
| `base` | `whyIsFinal` | hold | hold (ok) | 3,061,040 | 305.1 |
| `base` | `everyMessageEnds` | hold | hold (ok) | 3,061,040 | 152.3 |
| `base` | `endsDespitePartition` | hold | hold (ok) | 3,061,040 | 153.3 |
| `base` | `noInTimeReplyLost` | violated | violated (ok) | 13,425 | 0.8 |
| `base` | `witnessSettled` | violated | violated (ok) | 315 | 0.5 |
| `base` | `witnessExpired` | violated | violated (ok) | 1,071 | 0.5 |
| `base` | `witnessSuperseded` | violated | violated (ok) | 329 | 0.5 |
| `base` | `witnessUndeliverable` | violated | violated (ok) | 300 | 0.4 |
| `base` | `witnessLateSupersedeRefused` | violated | violated (ok) | 1,218 | 0.6 |
| `base` | `witnessLateExpireRefused` | violated | violated (ok) | 1,228 | 0.5 |
| `base` | `witnessLateSettleRefused` | violated | violated (ok) | 1,232 | 0.5 |
| `base` | `witnessLateDeliverRefused` | violated | violated (ok) | 1,170 | 0.5 |
| `base` | `witnessSettledWhilePending` | violated | violated (ok) | 307 | 0.4 |
| `base` | `witnessSettledFromAwaiting` | violated | violated (ok) | 14,291 | 0.7 |
| `base` | `witnessLateOutcomeRefused` | violated | violated (ok) | 1,112 | 0.5 |
| `base` | `witnessLateReceiptRefused` | violated | violated (ok) | 17,020 | 0.7 |
| `base` | `witnessTurnBeforeSettle` | violated | violated (ok) | 1,180 | 0.5 |
| `watermark` | `oneWinner` | hold | hold (ok) | 2,871,824 | 31.8 |
| `watermark` | `arrivedAreResolved` | hold | hold (ok) | 2,871,824 | 32.7 |
| `watermark` | `outcomesTruthful` | hold | hold (ok) | 2,871,824 | 34.3 |
| `watermark` | `dropReasonSound` | hold | hold (ok) | 2,871,824 | 34.0 |
| `watermark` | `settledHasEvidence` | hold | hold (ok) | 2,871,824 | 34.6 |
| `watermark` | `noDeliverOnPendingSettled` | hold | hold (ok) | 2,871,824 | 31.5 |
| `watermark` | `endsAreFinal` | hold | hold (ok) | 2,871,824 | 310.0 |
| `watermark` | `whyIsFinal` | hold | hold (ok) | 2,871,824 | 279.6 |
| `watermark` | `everyMessageEnds` | hold | hold (ok) | 2,871,824 | 151.9 |
| `watermark` | `noInTimeReplyLost` | hold | hold (ok) | 2,871,824 | 32.1 |
| `watermark` | `endsDespitePartition` | violated | violated (ok) | 1,527,415 | 63.9 |
| `expects_nothing` | `oneWinner` | hold | hold (ok) | 638,480 | 6.7 |
| `expects_nothing` | `arrivedAreResolved` | hold | hold (ok) | 638,480 | 7.6 |
| `expects_nothing` | `outcomesTruthful` | hold | hold (ok) | 638,480 | 6.8 |
| `expects_nothing` | `dropReasonSound` | hold | hold (ok) | 638,480 | 6.8 |
| `expects_nothing` | `settledHasEvidence` | hold | hold (ok) | 638,480 | 6.9 |
| `expects_nothing` | `noDeliverOnPendingSettled` | hold | hold (ok) | 638,480 | 6.5 |
| `expects_nothing` | `endsAreFinal` | hold | hold (ok) | 638,480 | 58.2 |
| `expects_nothing` | `whyIsFinal` | hold | hold (ok) | 638,480 | 53.5 |
| `expects_nothing` | `everyMessageEnds` | hold | hold (ok) | 638,480 | 33.1 |
| `expects_nothing` | `endsDespitePartition` | hold | hold (ok) | 638,480 | 32.8 |
| `mut_deliver_from_any` | `endsAreFinal` | violated | violated (ok) | 40,411 | 1.4 |
| `mut_deliver_from_any` | `oneWinner` | violated | violated (ok) | 14,527 | 0.7 |
| `mut_deliver_from_any` | `noDeliverOnPendingSettled` | violated | violated (ok) | 16,502 | 0.7 |
| `mut_observe_from_any` | `endsAreFinal` | violated | violated (ok) | 14,977 | 0.9 |
| `mut_observe_from_any` | `oneWinner` | violated | violated (ok) | 4,759 | 0.6 |
| `mut_observe_from_any` | `noDeliverOnPendingSettled` | hold | hold (ok) | 3,016,328 | 38.3 |
| `mut_expire_ignores_outcome` | `dropReasonSound` | violated | violated (ok) | 3,692 | 0.6 |
| `mut_expire_ignores_outcome` | `oneWinner` | hold | hold (ok) | 3,184,808 | 40.0 |
| `mut_expire_ignores_outcome` | `endsAreFinal` | hold | hold (ok) | 3,184,808 | 340.2 |
| `mut_observe_unexpected` | `settledHasEvidence` | violated | violated (ok) | 294 | 0.4 |
| `mut_observe_unexpected` | `oneWinner` | hold | hold (ok) | 757,064 | 8.3 |
| `mut_observe_unexpected` | `endsAreFinal` | hold | hold (ok) | 757,064 | 68.1 |
| `mut_drop_from_any` | `oneWinner` | hold | hold (ok) | 2,426,792 | 34.3 |
| `mut_drop_from_any` | `endsAreFinal` | violated | violated (ok) | 15,323 | 0.8 |
| `mut_drop_from_any` | `arrivedAreResolved` | hold | hold (ok) | 2,426,792 | 32.9 |
| `mut_silent_refusal` | `arrivedAreResolved` | violated | violated (ok) | 1,128 | 0.5 |
| `mut_silent_refusal` | `oneWinner` | hold | hold (ok) | 3,061,040 | 33.3 |
| `mut_silent_refusal` | `endsAreFinal` | hold | hold (ok) | 3,061,040 | 310.9 |
| `mut_expire_ignores_deadline` | `dropReasonSound` | violated | violated (ok) | 12 | 0.4 |
| `mut_expire_ignores_deadline` | `oneWinner` | hold | hold (ok) | 3,323,328 | 37.6 |
| `mut_expire_ignores_deadline` | `endsAreFinal` | hold | hold (ok) | 3,323,328 | 369.3 |
| `mut_expire_ignores_reply` | `dropReasonSound` | violated | violated (ok) | 38,242 | 0.8 |
| `mut_expire_ignores_reply` | `oneWinner` | hold | hold (ok) | 3,112,880 | 36.2 |
| `mut_expire_ignores_reply` | `endsAreFinal` | hold | hold (ok) | 3,112,880 | 332.4 |
| `mut_undeliverable_anywhere` | `dropReasonSound` | violated | violated (ok) | 13,260 | 0.7 |
| `mut_undeliverable_anywhere` | `oneWinner` | hold | hold (ok) | 3,280,712 | 38.4 |
| `mut_undeliverable_anywhere` | `endsAreFinal` | hold | hold (ok) | 3,280,712 | 342.5 |
| `mut_supersede_unchecked` | `dropReasonSound` | violated | violated (ok) | 16 | 0.4 |
| `mut_supersede_unchecked` | `oneWinner` | hold | hold (ok) | 3,152,032 | 33.5 |
| `mut_supersede_unchecked` | `endsAreFinal` | hold | hold (ok) | 3,152,032 | 332.7 |
