# Workflow orchestration prior art for crewed, conversational steps

**Date:** 2026-07-29

**Issue:** [#1263](https://github.com/flotilla-org/flotilla/issues/1263), part of
[#1262](https://github.com/flotilla-org/flotilla/issues/1262)

**Method:** Primary-source comparison of Argo Workflows, Argo CD, GitHub
Actions, Temporal, AWS Step Functions, and Golem. The question is deliberately
narrow: what survives when a “step” is a stateful conversation with an agent
crew in a warm workspace, rather than an idempotent pod or job?

## Executive conclusion

No surveyed system should replace Flotilla's `WorkflowTemplateSpec`.

The useful synthesis is:

- **Argo Workflows** validates the static DAG, explicit dependency, artifact,
  suspend, and terminal-handler vocabulary.
- **Argo CD** validates level-triggered reconciliation, health-gated ordering,
  and coarse phase/wave hooks, but it is not a conversational workflow engine.
- **GitHub Actions** validates declarative event matching, reusable definitions,
  and first-class approval policy, while also showing why “an event starts
  another run” is not re-entry into a warm crew.
- **Step Functions** is the clearest state-machine-as-data and callback-token
  model, but a token completes one waiting task; it is not a mailbox for a
  repeatedly re-engaged actor.
- **Temporal** is the strongest model for a durable workflow identity that
  receives messages. Its Signal model is the closest prior art for waking a
  convoy, but its deterministic workflow code must keep LLM calls and other
  non-deterministic work in Activities.
- **Golem** is the closest runtime model for the crew itself: a named, durable,
  stateful agent can be suspended, rehydrated, and invoked again with ordinary
  in-memory context reconstructed from an operation log. That is still a
  durable-imperative runtime, not a replacement for Flotilla's
  level-triggered, resource-authority-aware control plane.

The recommended cut is therefore:

1. retain vessels, stances, repository scope, credential declarations, and
   crew roles as the **actor/placement topology**;
2. extend the workflow snapshot with a declarative **turn, gate, and
   subscription program** that can request only transitions already admitted
   by the resource's declared state machine;
3. add a durable, auditable **event inbox and engagement records** to the convoy
   instance; and
4. keep reconcilers level-triggered and authoritative for observed conditions,
   while engagements are imperative, re-entrant turns sent to a selected crew
   role.

The semantic unit is not “run this agent pod once.” It is:

> Maintain this vessel and crew identity; when a declared event matches while
> the convoy is in a declared state, record one engagement and deliver its
> prompt to the appropriate role, resuming or materializing that role as
> policy requires.

## The local shape that prior art must fit

`WorkflowTemplateSpec` already declares inputs and a DAG of
`VesselRequirement`s. Each vessel has `depends_on`, repository and credential
scope, a `Stance`, and ordered `CrewSpec`s; a crew source is either a tool
command or an agent capability selector plus prompt/brief template
(`crates/flotilla-resources/src/workflow_template.rs:10-108`).

Those are not cosmetic differences from a job DAG:

- a vessel is the shared placement/workspace boundary, not the executable
  command;
- `Stance::{Trusted, WorkspaceWrite, Contained}` is a minimum isolation
  requirement (`workflow_template.rs:64-84`);
- all tools start eagerly, but only the first agent starts eagerly; later agent
  roles are latent until a handoff (`workflow_template.rs:41-62`);
- convoy status separately records vessel work and per-role crew work with
  `Pending`, `Working`, `Interrupted`, `Done`, `HandedBack`, and `Failed`
  (`crates/flotilla-resources/src/convoy.rs:128-143,243-262`);
- a stopped active agent session interrupts the vessel rather than proving that
  the work succeeded or failed
  (`crates/flotilla-controllers/src/reconcilers/vessel.rs:829-855`);
- a terminal session carries a crew ID, adapter, model, stance,
  attention, and delivered-message ID
  (`crates/flotilla-resources/src/terminal_session.rs:120-170,240-252`); and
- ADR 0010 already freezes `AgentAdapter.re_prompt(session, msg)` as the
  harness-neutral session verb, with the Bosun as caller; workflow delivery
  should use that seam rather than absorb harness-specific driving mechanics
  (`docs/adr/0010-crew-provisioning.md:9-24,103-112`); and
- the accepted lifecycle deliberately keeps a vessel warm in `Landing` for
  review feedback, and reserves `Anchored` for re-entrant waits on external
  events (`docs/adr/0021-convoy-lifecycle-landing-landed-anchored.md:26-43`).

The existing model is consequently already richer than a generic state
machine's `Task`. Replacing it would discard placement, authority, isolation,
and conversational identity exactly where the new workflow substrate needs
them.

There is also an accepted outer safety envelope. ADR 0024 requires every
lifecycle-bearing resource to declare one transition table as Rust data beside
its phase enum, and assigns every field an operator, loop, or actuator owner.
It explicitly says the #1232 workflow growth extends the convoy table rather
than bypassing it
(`docs/adr/0024-declared-state-machines-and-field-ownership.md:44-72,139-142`).
A workflow template may therefore select or request an already-declared edge
through that edge's proper writer. It may never add a `ConvoyPhase`, invent an
edge, or write a phase directly.

## Comparison

| System | How work is declared | How an external event re-enters a running instance | Human gate | What fails for a crew conversation | Useful inheritance for Flotilla |
|---|---|---|---|---|---|
| **Argo Workflows** | YAML templates compose either ordered step groups or DAG tasks with explicit dependencies; templates ultimately invoke containers or other template types. [Steps](https://argo-workflows.readthedocs.io/en/latest/walk-through/steps/), [DAG](https://argo-workflows.readthedocs.io/en/latest/walk-through/dag/) | General external events do not address an arbitrary running step. A workflow may stop at a declared `suspend` node and resume manually or after a duration. [Suspending](https://argo-workflows.readthedocs.io/en/latest/walk-through/suspending/) | A suspend node can solicit supplied intermediate parameters, including an approval choice, which downstream steps consume. [Intermediate parameters](https://argo-workflows.readthedocs.io/en/latest/intermediate-inputs/) | Pod completion is the unit of progress; file/directory artifacts cross node boundaries; exit handling is terminal. A warm workspace, transcript, role handoff, and repeated wake-up do not fit that lifecycle without hiding the real actor behind one indefinitely running pod. | Keep explicit `depends_on`, output references, suspend/gate vocabulary, and terminal outcome handlers. Do not inherit pod identity or completion semantics. |
| **Argo CD** | Kubernetes resources carry hook-phase and integer sync-wave annotations. Argo CD orders by phase, wave, kind, and name, then repeatedly applies the first out-of-sync/unhealthy wave. [Sync phases and waves](https://argo-cd.readthedocs.io/en/stable/user-guide/sync-waves/) | A Git/cluster-state change causes another reconciliation/sync assessment. It does not deliver a typed message into a running hook; hooks are Kubernetes resources whose health/completion gates the sync. [Sync phases and waves](https://argo-cd.readthedocs.io/en/stable/user-guide/sync-waves/) | No general human-conversation gate exists in phases/waves. A manual sync boundary or a custom resource/controller can supply one, but that gate is outside the hook/wave abstraction. [Automated sync policy](https://argo-cd.readthedocs.io/en/stable/user-guide/auto_sync/) | A hook is normally a Pod, Job, or Workflow and has creation/deletion policy. Treating a crew as a hook confuses desired-state convergence with a long-lived actor whose next prompt depends on accumulated dialogue. | Inherit level-triggered evaluation, health/condition gates, coarse phases, and “first unsatisfied wave” scheduling. Do not model engagements as sync hooks. |
| **GitHub Actions** | Repository YAML maps `on` events to jobs; jobs use `needs`, and steps run scripts or actions. Reusable workflows are whole YAML workflows declared with `on: workflow_call` and invoked at job level. [Workflows](https://docs.github.com/en/actions/concepts/workflows-and-actions/workflows), [Reuse workflows](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows) | Repository, external `repository_dispatch`, schedule, or manual events select and start workflow runs. A matching event starts a distinct run; it does not address a suspended step in an existing run. [Events that trigger workflows](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows) | A job that references an environment waits until its deployment protection rules pass; required reviewers can approve it, and environment secrets are unavailable until approval. [Deployments and environments](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments) | New runs have runner/job identity, not a stable conversational identity. Outputs and artifacts bridge finite jobs; a repeated review comment would naturally create another run. That loses the warm crew, transcript, and “same role, next round” semantics. | Inherit the declarative event-filter surface, reusable/pinned definitions, explicit job dependencies, and policy-rich approvals. Keep events attached to an existing convoy instead of starting a new convoy. |
| **AWS Step Functions** | ASL is a JSON-based state-machine definition. Named states include `Task`, `Choice`, `Wait`, `Parallel`, and `Map`, linked by `Next` or terminal `End`; JSON state output normally becomes the next state's input. [State machines](https://docs.aws.amazon.com/step-functions/latest/dg/concepts-statemachines.html), [ASL](https://docs.aws.amazon.com/step-functions/latest/dg/concepts-amazon-states-language.html) | A `.waitForTaskToken` task passes a capability token outward and pauses until `SendTaskSuccess` or `SendTaskFailure` returns that token and a payload. Only then does the execution continue. [Callback integration](https://docs.aws.amazon.com/step-functions/latest/dg/connect-to-resource.html#connect-wait-token) | AWS's official human-approval sample pauses at a callback task and resumes when an approve/reject URL causes the token callback. [Human approval tutorial](https://docs.aws.amazon.com/step-functions/latest/dg/tutorial-human-approval.html) | The callback is one outstanding completion edge for one task. It does not model an open-ended, ordered stream of comments, CI changes, handoffs, and human replies addressed to one persistent crew. Tokens also require an adapter and careful timeout/retry handling. | Inherit state-machine-as-data, typed transition outcomes, durable callback correlation, and explicit timeouts. Generalize the single callback into inbox + engagement records. |
| **Temporal** | Workflow Definitions are ordinary SDK code. A Workflow Execution has exclusive local state, issues commands, waits on SDK awaitables, and recovers by replaying Event History. [Workflow execution](https://docs.temporal.io/workflow-execution) | Signals are asynchronous messages to an open Workflow Execution that can mutate state/control flow; the accepted Signal is recorded in Event History. Signal-With-Start atomically signals the running ID or starts and signals a new execution. [Go message passing](https://docs.temporal.io/develop/go/workflows/message-passing) | The documented Signal example blocks until an `approve` Signal arrives. Updates add synchronous acceptance/validation and a returned result when the gate needs an acknowledgement. [Go message passing](https://docs.temporal.io/develop/go/workflows/message-passing) | Workflow code must produce the same SDK commands on replay; Temporal explicitly puts API calls, LLM/AI invocations, and other nondeterministic operations in Activities. The agent conversation therefore cannot itself be replayed as Workflow code; it remains an external activity/actor. [Deterministic constraints](https://docs.temporal.io/workflow-definition#deterministic-constraints) | Inherit stable workflow addressing, durable signals, recorded message acceptance, waiting without polling, and signal-with-start semantics. Keep crew execution outside the deterministic transition evaluator. |
| **Golem** | A versioned agent type is code compiled to a WebAssembly component; named agent instances have durable identity and in-memory state. Durable agents record side effects in an oplog and recover by replay, without application persistence code. [Agents](https://learn.golem.cloud/v1.5/concepts/agents), [Durability](https://learn.golem.cloud/v1.5/how-to-guides/ts/golem-configure-durability-ts) | An invocation targets the same named agent. A Golem promise can suspend an agent until an external source completes the promise through the REST API; the payload is returned when execution continues. [Promises](https://learn.golem.cloud/v1.5/develop/promises) | Human input can be modeled as promise completion; an agent awaiting only promises is suspended and consumes no execution resources. [Promises](https://learn.golem.cloud/v1.5/develop/promises) | Golem best matches the stateful actor, but adopting it as the workflow authority would move orchestration into opaque imperative code and oplogs. It also cannot make an arbitrary external endpoint exactly-once: outside Golem the last request is generally at-least-once unless the endpoint cooperates with a durably generated idempotency key. [Reliability limits](https://learn.golem.cloud/v1.5/concepts/reliability) | Inherit named durable actors, suspend-to-zero, operation/inbox history, and mediated capabilities as design evidence. Do not replace declarative resources and reconcilers with WASM-resident workflow code. |

## System-by-system findings

### Argo Workflows: the DAG is useful; the node lifecycle is not

Argo offers two equivalent composition idioms. Step groups provide sequential
rows with parallel steps inside a row, while DAG tasks declare dependencies
directly; DAGs may have multiple roots and nest other DAG/steps templates.
[Steps](https://argo-workflows.readthedocs.io/en/latest/walk-through/steps/)
[DAG](https://argo-workflows.readthedocs.io/en/latest/walk-through/dag/)

Artifacts are explicit step outputs and downstream inputs, normally files or
directories stored through an artifact repository. They are excellent for
immutable deliverables, but they are the wrong abstraction for a live checkout,
terminal session, transcript, or crew memory: those are the identity-bearing
state of a vessel, not a blob passed to its successor.
[Artifacts](https://argo-workflows.readthedocs.io/en/latest/walk-through/artifacts/)

Argo's external re-entry surface is narrow but instructive. A declared
`suspend` node prevents new steps from being scheduled until manual resume or a
duration expires; intermediate parameters let a human supply a value while the
node is suspended. This is a useful gate, not a general message inbox.
[Suspending](https://argo-workflows.readthedocs.io/en/latest/walk-through/suspending/)
[Intermediate parameters](https://argo-workflows.readthedocs.io/en/latest/intermediate-inputs/)

An `onExit` template always runs after the primary workflow regardless of
success or failure and can branch on workflow status. Flotilla should inherit
the *outcome-handler* idea for settlement, extraction, notification, and
reclaim policy, but not make exit handlers responsible for ordinary review
rounds: a review round means the convoy is still live.
[Exit handlers](https://argo-workflows.readthedocs.io/en/latest/walk-through/exit-handlers/)

### Argo CD: reconciliation is the control-plane half, not the conversation

Argo CD hooks annotate Kubernetes resources with phases such as `PreSync`,
`Sync`, `PostSync`, and `SyncFail`; waves are integer annotations ordered low to
high. Within the resulting order, Argo CD repeatedly finds the first wave with
an out-of-sync or unhealthy resource, applies it, and waits for convergence.
[Sync phases and waves](https://argo-cd.readthedocs.io/en/stable/user-guide/sync-waves/)

That gives Flotilla two good constraints:

1. “ready” should be a continuously evaluated standing condition, not an event
   testimony that directly writes success; and
2. ordering metadata and outcome hooks can remain declarative.

It does **not** provide a wake-up model. A hook's lifecycle is Kubernetes
resource health/completion, and hook deletion policies explicitly treat the
hook as a run artifact. A crew role is instead a durable address that may be
working, interrupted, handed back, or resumed. The “argocd-ish” part should
therefore be the reconciler and declared condition table, not “crew as hook.”

### GitHub Actions: excellent trigger syntax, wrong run identity

GitHub Actions workflow files declare one or more trigger events under `on`,
jobs, and steps; event activity types and filters decide whether a run is
created. External systems can use `repository_dispatch`, but it still triggers
a workflow run. Multiple simultaneous matching events may create multiple
runs. [Triggering a workflow](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)

Reusable workflows provide a clean definition/call split: `workflow_call`
declares typed inputs and secrets, and a caller uses the workflow directly as a
job. That supports pinned, reusable workflow assets, but the called workflow is
still a finite run, not a persistent actor.
[Reuse workflows](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows)

Environments are the strongest surveyed declarative approval-policy surface:
protection rules can require a reviewer, wait timer, branch restriction, or
custom GitHub App rule; the job and its environment secrets remain blocked
until the rules pass.
[Deployments and environments](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments)

Flotilla should copy the separation between:

- a workflow's declaration that a gate applies;
- reusable policy attached to the gate/environment; and
- an authenticated approval record.

It should not copy the “new event, new run” identity rule. Review comments,
check failures, and merge conflicts are new **engagements of the same convoy
and usually the same vessel/role**.

### Step Functions: callback correlation is not a mailbox

Step Functions most literally demonstrates workflows as data. ASL names states
inside a `States` object and links them through transitions; Task states point
to a service integration or resource ARN, while flow states express choice,
wait, parallelism, and mapping.
[State machines](https://docs.aws.amazon.com/step-functions/latest/dg/concepts-statemachines.html)
[Task state](https://docs.aws.amazon.com/step-functions/latest/dg/state-task.html)

The callback pattern is a capability-token protocol: the state publishes
`$$.Task.Token`, pauses, and continues only after an external principal calls
`SendTaskSuccess` or `SendTaskFailure` with that token. A timeout generates a
new random token, and task-token callbacks must come from a principal in the
same AWS account.
[Integration patterns](https://docs.aws.amazon.com/step-functions/latest/dg/connect-to-resource.html)

That is strong prior art for:

- an unguessable correlation capability;
- durable recording of “which wait did this response satisfy?”;
- explicit success/failure payloads; and
- timeout/invalidation behavior.

It is insufficient for recurring crew conversations. One event completes one
wait; a crew needs an ordered inbox in which several facts may arrive while it
is asleep or already working, be coalesced into one round, and be acknowledged
without completing the vessel itself.

### Temporal: copy Signals, not workflow-as-code

A Temporal Workflow Execution is a durable function execution with exclusive
local state. The service persists Event History; replay regenerates commands
and compares them with that history, recovering after worker/service failure.
[Workflow execution](https://docs.temporal.io/workflow-execution)

Signals are the closest surveyed primitive to the requested “wake a long-lived
thing with an event.” They are asynchronous messages sent to an open Workflow
Execution to change its state or control flow. The server records acceptance in
Event History; a Signal addressed only by Workflow ID reaches the current
running execution. Signal-With-Start eliminates the race between looking up an
instance and creating it.
[Workflow message passing](https://docs.temporal.io/develop/go/workflows/message-passing)

Temporal also makes the boundary crisp. Workflow code must be deterministic:
given the same history it must issue the same SDK commands in the same order.
The official documentation explicitly names LLM/AI invocations, APIs, and
database queries as nondeterministic work to put in Activities outside the
replay path.
[Deterministic constraints](https://docs.temporal.io/workflow-definition#deterministic-constraints)

For Flotilla, the declarative transition evaluator plays Temporal's
deterministic workflow role. A crew engagement plays the Activity/actor role.
The evaluator may record “engagement requested/delivered/completed”; it must
never attempt to reproduce a conversation to prove state.

### Golem: the closest crew runtime, but not the control-plane authority

Golem's unit is a named, stateful agent instance compiled as a WebAssembly
component. Durable agents record effects in an operation log and rebuild
in-memory state by replay; snapshots may bound replay time. The following
host-boundary contrast with Temporal is an architectural inference from the
documented mechanics: ordinary Golem agent code does not have Temporal's
requirement to route every nondeterministic operation through an SDK-level
Activity abstraction because Golem mediates WASI/host effects at the component
boundary and replays recorded results.
[Agent durability](https://learn.golem.cloud/v1.5/how-to-guides/ts/golem-configure-durability-ts)
[Reliability/WASI boundary](https://learn.golem.cloud/v1.5/concepts/reliability)
[Outgoing HTTP replay](https://learn.golem.cloud/v1.5/how-to-guides/moonbit/golem-make-http-request-moonbit)

That distinction is material:

- **Temporal:** application authors preserve determinism in Workflow code and
  move LLM/tool/API work to Activities.
- **Golem:** guest code is re-executed while the runtime's WASM host boundary
  records and substitutes external effects, so an agent's ordinary in-memory
  control flow can be durable.

Golem promises are also closer to a stateful crew wait than Step Functions
tokens: an agent creates and awaits a promise, is suspended without consuming
execution resources while only waiting, and continues with the external
completion payload.
[Promises](https://learn.golem.cloud/v1.5/develop/promises)

The important guarantee boundary must be stated conservatively. Golem's
official reliability reference says local code and agent-to-agent
communication are exactly-once, but a remote external operation is generally
at-least-once around the last uncertain request. Exactly-once effect semantics
require the remote API to honor an idempotency key that Golem durably
generates/commits.
[Reliability limitations](https://learn.golem.cloud/v1.5/concepts/reliability)

This is valuable prior art for a future durable crew-session runtime. It does
not justify moving Flotilla's workflow into an oplog-backed WASM program:

- Flotilla resources already encode multiple authorities and federation;
- controllers must continuously repair actual toward desired state;
- observed forge and workspace facts may change independently of a sleeping
  program; and
- operators need a queryable declarative explanation of why a crew is eligible
  to wake.

Durable imperative execution and level-triggered reconciliation solve
different halves:

```text
resource store + observations
        │
        ▼
level-triggered reconciler ── computes current declared transition/gate
        │
        ▼
durable event/inbox record ── selects or creates one crew engagement
        │
        ▼
stateful imperative crew turn ── conversation, tools, side effects, report
        │
        └──────────── claims/results/observations return to resource store
```

For #1268, the sharp rule is: **reconciliation owns eligibility and verified
conditions; imperative execution owns the engagement once admitted.** Neither
should impersonate the other.

Golem's capability-scoped sandbox and automatic MCP exposure are adjacent,
secondary evidence. Capabilities map conceptually to a vessel's stance plus
credential leases, while “agent as MCP server” is a possible distribution
surface for crew/tool invocation, not a workflow model.
[Golem concepts](https://learn.golem.cloud/v1.5/concepts)
[MCP invocation](https://learn.golem.cloud/v1.5/invoke/mcp)

The current Golem plugin surface documents oplog processors, not a generic
pre/post tool-call middleware chain. That leaves policy enforcement around
tool calls, credential leases, and audit as a Flotilla design opportunity
rather than a facility this comparison can inherit directly.
[Golem plugins](https://learn.golem.cloud/v1.5/concepts/plugins)

## Recommended shape: extend `WorkflowTemplateSpec`

The following sketch is intentionally a question-generating contract, not a
premature Rust type:

It is also constrained by ADR 0008: recurring semantics should be harvested
from working orchestration rather than growing a speculative YAML language,
and its current extraction target is programs
(`docs/adr/0008-agentic-first-orchestration.md:7-27`). The three acceptance
scenarios below provide recurring evidence, but #1268 still has to decide
whether their small closed vocabulary belongs in template data, a program SDK,
or both. The YAML is illustrative of semantics, not a decision to embed a
general workflow engine.

### Two related machines, not one

The distinction that #1268 must preserve is:

- **Outer resource lifecycle:** the fixed ADR 0024 transition table over
  `ConvoyPhase`, `WorkPhase`, and `CrewWorkPhase`. This is the safety and field
  ownership boundary. Workflow data cannot extend its edge set.
- **Inner workflow program:** named turns, subscriptions, gates, and outcome
  actions. It may loop and re-engage a role many times while the outer convoy
  remains `Landing`, or request a declared `Anchored → Active`-style edge
  through the authorized claim writer.

An inner turn name such as `shepherd-round` is not a new `ConvoyPhase`.

```yaml
inputs:
  - name: target

vessels:                         # existing actor/placement topology
  - name: work
    stance: contained
    credential_refs: [forge]
    crew:
      - role: coder
        selector: { capability: code }
      - role: reviewer
        selector: { capability: code-review }

turns:                           # inner logical program, not ConvoyPhase
  - name: initial
    engage:
      vessel: work
      role: coder
      prompt_template: initial
  - name: shepherd-round
    engage:
      vessel: work
      role: coder
      prompt_template: shepherd-round

subscriptions:
  - accept_while:
      convoy_phase: [landing, anchored]  # existing outer phases
    match:
      source: change-request
      subject: { bound_to_convoy: true }
      changed: [reviews, checks, mergeability]
    coalesce:
      key: review-round
      quiet_period: 20s
    request:
      turn: shepherd-round
      mode: resume-or-deliver
      # If a phase move is needed, select one edge from the fixed ADR 0024
      # table; do not define `from`/`to` here.
      transition_ref: EXISTING_RESUME_EDGE  # illustrative symbolic ID

gates:
  - name: destructive-action
    policy_ref: human-review/default
    demand:
      addressee: dispatching-principal
    satisfies:
      event: approval-recorded
      principal: { permission: approve-convoy }
    transition_ref: EXISTING_APPROVAL_EDGE  # illustrative symbolic ID

outcomes:
  - on: [landed, failed, cancelled, abandoned]
    actions: [extract-session-records, evaluate-reclaim]
```

### Why extension is the sound cut

`vessels` answers **who/where/with what isolation and credentials**.
`turns` answers **which repeatable conversation to engage**.
`subscriptions` answer **which changing facts can request another turn**.
The ADR 0024 table continues to answer **which resource phase moves are legal
and who may write them**.
An instance-side inbox answers **what actually arrived and what happened to
it**. These concerns are related but not substitutable.

The runtime shape should include at least:

```text
WorkflowSnapshot
  vessels[]                 existing, pinned at admission
  turns[]
  subscriptions[]
  gates[]
  outcomes[]

ConvoyStatus
  phase                     existing outer ConvoyPhase
  program_position          inner turn/gate position, if one is needed
  event_cursor(s)
  inbox[event_id]           observed, matched, coalesced, delivered, acked
  engagements[id]          target role, prompt digest, attempt, result
  work / crew_work          existing operational rollups
```

Every delivery needs a stable event ID and engagement ID. Delivery should be
idempotent, but the design should assume at-least-once observation/delivery
rather than claim magical end-to-end exactly-once behavior. A terminal's
existing `delivered_message_id` is useful evidence, but one scalar cannot
represent a queue, batching, retries, or multiple in-flight engagement rounds.

Human approval should use the same durable event path but remain a distinct
typed record with principal, policy, decision, reason, and time. A pending
human gate raises a `Demand`, which ADR 0018 defines as queue-shaped attention,
never itself a phase
(`docs/adr/0018-presentation-attention-demands-regards-projection.md:19-33`).
Approval is not merely `true`; it is evidence satisfying a declared gate. It
may let the gate's authorized writer request an existing phase edge, but must
not silently invent an edge or write a verified external condition, preserving
the claims/conditions split in ADR 0017 and ADR 0021.

## Acceptance scenarios the shape must express without bespoke reconcilers

### Rebase on conflict (#1150)

1. The bound change request's observed mergeability changes to conflicting.
2. The workflow subscription matches the convoy-bound subject while in
   `Landing`.
3. One coalesced engagement targets the warm shepherd/coder role with conflict,
   review, and check deltas.
4. The crew reports the round complete; observations later prove the conflict
   cleared.

No “rebase episode” reconciler or conflict-specific phase is allowed.

### Review round trip (#1216)

Review submissions, requested changes, and check completion become inbox
events. The workflow's coalescing and concurrency policy decides whether to
append to the active round or create the next engagement. Crew completion ends
the **round**, not the vessel or convoy. Landing remains open until the
integration condition becomes true.

### PR-first adoption (#1071)

Admission binds the PR and starts the existing vessel/crew topology. The same
subscriptions apply because their subject selector is “the convoy's bound
change request,” independent of whether Flotilla created or adopted the branch.

## Concrete questions for the G-tickets

### G1 — What is the workflow semantic unit?

- Is a vessel strictly an actor/placement boundary referenced by turns, or can
  it also carry an inner program position distinct from its outer work phase?
- Is an engagement/turn the disposable execution unit beneath a durable vessel?
- Can two roles on one vessel have concurrent engagements, and who serializes
  writes to the shared checkout?

### G2 — How does the current DAG extend into a turn program?

- Does `depends_on` compile into initial turn readiness, remain a separate
  topology relation, or disappear?
- Are inner turn positions convoy-wide, per-vessel, or hierarchical?
- How are turn loops represented without adding edges to the fixed ADR 0024
  machine or making the original DAG validation meaningless?

### G3 — What is an event?

- Is the canonical input an immutable observed-fact delta, a resource status
  transition, or a domain event emitted by a source adapter?
- What stable source-qualified ID, subject, old/new value, observation time,
  owner/actuator authority, and provenance must every envelope contain?
- Which facts may be recomputed from current state, and which events must never
  be reconstructed after compaction?

### G4 — Where do subscriptions live and how are they pinned?

- Workflow definition, convoy override, fleet rule, project, or some layered
  composition?
- Does admission snapshot the resolved program exactly as it snapshots vessels?
- How do running convoys receive safe definition fixes without silently
  changing their past transition program?

### G5 — What are match and scope semantics?

- How does “my bound PR only” compile to a source-qualified subject selector?
- Can a subscription join across repositories/vessels without becoming an
  unbounded query?
- Is matching edge-triggered on a delta, level-triggered on current truth, or
  explicitly one of the two?

### G6 — What are delivery, ordering, and coalescing semantics?

- At-least-once delivery plus deduplication, or another stated contract?
- Ordering per source, subject, subscription, vessel, or role?
- What is the coalescing key and quiet period for review + checks + conflict?
- What happens to an event arriving while the target role is already working?
- When is an event acknowledged: persisted, message delivered, crew turn
  started, or crew turn completed?

### G7 — Who gets woken?

- Deliver to a live session, resume a completed role, materialize a latent role,
  or provision a new vessel?
- Is that choice declarative policy, placement policy, or governor judgment?
- What happens when the named role was removed, failed, or its vessel was
  reclaimed?

### G8 — What is an engagement record?

- Which prompt rendering inputs and source events are snapshotted?
- How do retries distinguish “message not delivered” from “agent acted but did
  not report”?
- Can a human or governor supersede, cancel, or merge pending engagements?

### G9 — How do human gates differ from crew prompts?

- Which resource defines approval policy and eligible principals?
- Are approve/reject/expire/escalate separate outcomes?
- Can the requester self-approve?
- Is approval a claim that admits an action, never evidence that the external
  action succeeded?

### G10 — Who may write each transition?

- Which edges are claims, which are continuously verified conditions, and which
  are controller-owned failure/cancellation?
- Can an event directly change phase, or only make a declared transition
  eligible?
- How does owner-authored versus actuator-observed authority survive
  federation?

### G11 — What are timeout, retry, and poison-event rules?

- Timeout of a gate, an engagement, message delivery, or whole state?
- Is a retry another attempt on the same engagement ID or a new engagement?
- Where do unmatched, malformed, permanently unauthorized, or repeatedly
  failing events surface for operators?

### G12 — What is terminal and what runs on exit?

- Which outcomes are guaranteed settlement/extraction attempts?
- Can an outcome action block terminality, or is it separately observable
  cleanup?
- Which warm vessels remain reachable from `Landing`/`Anchored`, and which may
  be reclaimed independently?

### G13 — How declarative must the language remain?

- Is the transition language a closed typed schema, CEL-like predicate
  language, or embedded code?
- What is the smallest language that expresses #1150, #1216, and #1071 without
  bespoke controller branches?
- How are predicates inspected, validated, versioned, and explained in the TUI?

### G14 — What belongs in a future durable crew runtime?

- Would oplog/snapshot durability preserve agent conversation and tool context,
  or is transcript + harness-native resume the durable source?
- Which effects cross a boundary that can only promise at-least-once?
- How do vessel stance, credential leases, and tool/MCP capabilities constrain
  a resumed role?
- Can such a runtime remain an execution substrate beneath resource
  reconciliation rather than becoming a second workflow authority?

## Recommendation to carry into design

Adopt **Argo CD outside, Temporal Signals in the middle, Golem-like durable
actors inside**:

- outside: level-triggered reconcilers continuously evaluate declared desired
  state and verified conditions;
- middle: durable, addressed, auditable event delivery creates idempotent crew
  engagements; and
- inside: a vessel/role retains conversation and workspace identity across
  engagements, whether today's warm terminal or a future suspend-to-zero
  runtime.

Keep the workflow definition declarative and pinned. Keep nondeterministic agent
behavior out of the transition evaluator. Treat every external side effect and
delivery as retryable/deduplicated at a named boundary, never as globally
exactly-once. If the three acceptance scenarios require a bespoke reconciler
branch after this substrate exists, the model is not yet general enough.
