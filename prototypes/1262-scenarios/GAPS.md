# Gaps exposed by the #1262 workflow scenarios

These files are candidate `WorkflowTemplate` resources for human reaction, not
documents accepted by the current `flotilla.work/v1` schema. The three proving
scenarios are joined by ruled-model translations of three code-owned builtins:
`single-agent-trusted`, `implement-review`, and `single-agent-shepherd`.

All six preserve the workflow-core and subscription-grill decisions:

- A workflow extends today's vessel, stance, credential, and crew topology with
  declared engagement rules.
- A rule has the form `when state or semantic transition + guards → engage role
  with brief template → expect completion condition`.
- Engagement-rule `when` clauses are the only workflow subscription surface.
  With no explicit widening, each rule sees only its convoy's resources.
- Rules reference declared convoy phases and derived semantic transitions. They
  do not add phases, add edges, emit events, or encode an imperative script.

The fixtures intentionally use one small candidate serialization:
`engagement_rules`, `when`, `engage`, and `expecting`. Exact Rust/YAML field
names are not yet decided.

## What the model cannot express yet

1. **Outcome-carrying engagement completion has no runtime shape yet.**
   `builtin-implement-review.yaml` adopts the refined candidate contract:
   completion is `Done` plus a typed outcome, and pure interpreters derive
   transitions such as `review.requested-changes` and `review.approved` from
   that stored fact. No resource or status yet records the engagement identity,
   attempt, admitted brief, typed outcome schema, report, or result. The
   producer authority and validation rules for each outcome kind also remain
   undefined.

   This exposes a sharper lifecycle gap. Today's bare
   `CrewWorkPhase::Done` unconditionally contributes to completing the vessel
   and moving the convoy to `Landing`. In the refined model,
   Done-with-requested-changes completes the review engagement but is not a
   settlement claim: the workflow remains active and re-engages the coder.
   Only Done-with-approved admits the existing `Work → Complete` and
   `Convoy → Landing` progression. The roll-up must therefore consume the
   pinned rule's accepted verdict, associate it with the current engagement
   attempt, and supersede it safely when a rule re-arms.

2. **Agent-directed early handoff still has no declaration.** The typed
   implementation/review outcomes now express the full normal loop: ready for
   review, requested changes, revised implementation, re-review, and approval.
   Today's `implement-review` coder chooses when to run `flotilla crew reviewer
   handoff`, may overlap implementation and review, and supplies a free-form
   message. The `diff-review` brief can hand findings back and re-review. The
   ruled model reserves adaptive engagements that write workflow data, but it
   does not define the declaration an agent writes for "engage reviewer now
   with this partial result." The file preserves the workflow's full settled
   behavior, but cannot preserve today's optional early/overlapping timing and
   free-form payload exactly without that declaration shape.

3. **Wake admission, coalescing, and re-arming are unspecified.**
   `rebase-on-conflict.yaml` states the desired cause and outcome but cannot
   express one wake per conflict episode, idempotent retry under one wake ID,
   suppression while a round is active, or a successor wake when relevant
   material arrives during that round. It also cannot say that a later
   mergeable-to-conflicting transition opens a new episode. These are general
   wake-semantics questions, not rebase-specific fields to add to the workflow.

4. **Target policy is absent.** `engage vessel: work, role: coder|shepherd`
   names the logical target but not what to do when its session is warm, cold,
   reaped, failed, or gone. Resume, re-provision, hold, escalate, and cancel
   policies need one general routing contract. Delivery acknowledgement must
   mean durable admission of the wake, not successful completion of the work.

5. **The review budget and loud hold do not fit.** #1216 requires a maximum
   number of rounds and a visible held condition when the budget is exhausted.
   The rule shape has no counter scope, reset rule, or `Demand`/hold action.
   Adding `max_turns` directly to one fixture would hide the larger policy
   question: whether budgets govern a rule, a workflow, a change request, or a
   convoy lifecycle interval.

6. **The closed semantic vocabulary is only partly named.**
   `change-request.went-conflicting` and phase-edge transitions are ruled.
   `change-request.review-arrived` and
   `change-request.checks-failed` are candidate dotted, past-tense names for the
   required review/check interpreters. Their typed payloads still need to
   define review identity, unresolved-thread state, check roll-up, observation
   freshness, and "feedback newer than the last crew activity." Definitions
   must live beside the ADR 0024 tables and carry the vocabulary generation.

7. **Brief and digest inputs are not declared.** The existing `crew`,
   `diff-review`, and `shepherd` templates cover the builtin turns, but the
   rebase brief named here does not yet exist. The model also does not say which
   typed transition digest, current resource facts, target/base refs, pinned CI
   gates, handoff result, and wake identity a template may consume. Canonical
   wake state should remain typed data; prompt prose must not become the event
   payload.

8. **Pinned definitions currently snapshot only vessels.** The core grill says
   the resolved builtin/fleet/project/repo source is recorded and definitions
   are pinned at admission, but `WorkflowSnapshot` currently contains only
   vessels. Engagement rules, their resolved source, the transition-vocabulary
   generation, and brief-template identity all need to be pinned and legible.

## What the model expresses differently from today's code

These are deliberate translations, not missing expressive power:

1. **Initial agent start becomes an engagement rule.** Today
   `VesselRequirement::starts_eagerly` starts the first agent when the vessel
   launches. The three builtin fixtures instead state
   `work.entered-running → engage role`. The topology still says who can run;
   the rule says why this turn exists.

2. **Brief selection moves from crew topology to each engagement.** Today
   `CrewSource::Agent.brief_template` is fixed per role, with `None` resolving
   to the default crew brief. The ruled files name `crew`, `diff-review`, or
   `shepherd` on `engage`, allowing later engagements of one role to use a
   different brief without cloning the role.

3. **The full implement-review loop is declared rather than prompted.** Today
   the coder learns about the latent reviewer from generated brief text and
   agents drive the loop with handoff commands. The ruled path stores typed
   Done-with-verdict outcomes: ready-for-review engages the reviewer,
   requested-changes re-arms review and engages the coder, and approved settles
   the loop into the declared work-completion and convoy-Landing progression.
   This removes lifecycle sequencing and settlement from prose. Preserving the
   optional early/overlapping handoff is the true gap described above, not a
   reason to keep the normal loop implicit.

4. **Follow-up shepherding becomes event-driven.** Today
   `single-agent-shepherd` runs one eager round, then a human or governor
   manually resumes it. `review-round-trip.yaml` and
   `rebase-on-conflict.yaml` make later rounds consequences of semantic
   transitions while retaining the real one-round `shepherd` brief.

5. **Builtins become ordinary layered data.** Today
   `builtin_workflow_templates()` constructs specs in Rust and the daemon
   reconciles them into resources labelled `flotilla.work/managed-by: builtin`.
   The ruled model treats builtin as the least-specific data source beneath
   fleet, project, and repo definitions. The three files retain the current
   names, labels, stances, roles, and capabilities while showing that future
   home.

## Behavior intentionally outside workflow data

- **PR adoption is admission.** `pr-adoption-turn.yaml` can guard on an
  already-bound change request, but `--pr` resolution, branch/base/title
  derivation, checkout adoption, and the atomic `ConvoySpec.change_request`
  write remain admission responsibilities. The missing contract is only the
  hand-off between eager bootstrap and the first declared engagement, so the
  same session is not engaged twice.
- **Stable review state is a derived condition or projection.** "Checks green
  and no unaddressed feedback" should surface merge readiness without waking a
  crew. Rules-only subscriptions deliberately have one effect—request an
  engagement—so readiness belongs in the condition/presentation substrate, not
  a second workflow subscription surface.
- **Transition interpretation stays substrate code.** Workflow data consumes
  stable dotted names; it does not define the pure `(old, new)` interpreters or
  alter ADR 0024 phase tables.
