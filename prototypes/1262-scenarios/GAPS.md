# Gaps exposed by the #1262 workflow scenarios

These files are candidate `WorkflowTemplate` resources for human reaction, not
documents accepted by the current `flotilla.work/v1` schema. They preserve the
resolved model from the workflow-core and subscription grills:

- A workflow extends today's vessel, stance, credential, and crew topology with
  declared engagement rules.
- A rule has the form `when state or semantic transition + guards → engage role
  with brief template → expect completion condition`.
- Engagement-rule `when` clauses are the only workflow subscription surface.
  With no explicit widening, each rule sees only its convoy's resources.
- Rules reference declared convoy phases and derived semantic transitions. They
  do not add phases, add edges, emit events, or encode an imperative script.

The three fixtures intentionally use one small candidate serialization:
`engagement_rules`, `when`, `engage`, and `expecting`. Exact Rust/YAML field
names are not yet decided.

## What the model could not express

1. **An engagement round has no runtime shape.** The model names an engagement
   as the idempotent unit of crew work, but no resource or status records its
   identity, target, attempt, admitted brief, report, or result. In particular,
   `review-round-trip.yaml` cannot distinguish "this round completed" from the
   existing `CrewWorkPhase::Done`, which currently contributes to completing
   the vessel and moving the convoy to `Landing`. Re-engaging a warm, completed
   crew needs a contract that does not confuse an inner round with an outer
   lifecycle edge.

2. **Wake admission, coalescing, and re-arming are unspecified.**
   `rebase-on-conflict.yaml` states the desired cause and outcome but cannot
   express one wake per conflict episode, idempotent retry under one wake ID,
   suppression while a round is active, or a successor wake when relevant
   material arrives during that round. It also cannot say that a later
   mergeable-to-conflicting transition opens a new episode. These are the open
   questions in the wake-semantics grill, not rebase-specific fields to add to
   the workflow.

3. **Target policy is absent.** `engage vessel: work, role: coder|shepherd`
   names the logical target but not what to do when its session is warm, cold,
   reaped, failed, or gone. Resume, re-provision, hold, escalate, and cancel
   policies need one general routing contract. Delivery acknowledgement must
   mean durable admission of the wake, not successful completion of the work.

4. **The review budget and loud hold do not fit.** #1216 requires a maximum
   number of rounds and a visible held condition when the budget is exhausted.
   The ruled engagement-rule shape has no counter scope, reset rule, or
   `Demand`/hold action. Adding `max_turns` directly to this one fixture would
   hide the larger policy question: whether budgets govern a rule, a workflow,
   a change request, or a convoy lifecycle interval.

5. **Stable review state is not an engagement.** "Checks green and no
   unaddressed feedback" should surface merge readiness without waking a crew.
   Rules-only subscriptions deliberately have one effect—request an
   engagement—so the model needs a separate derived condition or presentation
   projection for readiness. It must not smuggle a second workflow-subscription
   surface into the template.

6. **PR adoption crosses the admission boundary.**
   `pr-adoption-turn.yaml` can guard on the already-bound change request, but it
   cannot declare the `--pr` resolver, derive the branch/base/title, adopt or
   provision the checkout, or atomically write `ConvoySpec.change_request`.
   Those operations belong to admission rather than the inner workflow. What is
   missing is the explicit hand-off: whether `convoy.entered-active` creates the
   first engagement, claims an eagerly started crew turn, or would double-wake
   the session that vessel bootstrap already launched.

7. **The closed semantic vocabulary is only partly named.**
   `change-request.went-conflicting` and phase-edge transitions are ruled.
   `change-request.review-arrived` and
   `change-request.checks-failed` are candidate dotted, past-tense names for the
   required review/check interpreters. Their typed payloads still need to
   define review identity, unresolved-thread state, check roll-up, observation
   freshness, and the comparison needed for "feedback newer than the last crew
   activity." The definitions must live beside the ADR 0024 tables and carry
   the vocabulary generation stamp.

8. **Brief and digest inputs are not declared.** The existing `shepherd`
   template covers one round, but the rebase brief named here does not yet
   exist. The workflow model also does not say which typed transition digest,
   current resource facts, target/base refs, pinned CI gates, and wake identity
   a brief template may consume. Canonical wake state should remain typed data;
   rendered prompt prose must not become the event payload.

9. **Pinned definitions currently snapshot only vessels.** The core grill says
   the resolved builtin/fleet/project/repo source is recorded and definitions
   are pinned at admission, but `WorkflowSnapshot` currently contains only
   vessels. Engagement rules, their resolved source, the transition-vocabulary
   generation, and brief-template identity all need to be part of the pinned,
   legible definition seen by a running convoy.
