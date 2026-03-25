# Step-Level Remote Routing — Design Spec

**Issue:** #464 (step-level remote routing: plan locally, execute steps on target hosts)
**Date:** 2026-03-25
**Related specs:** `docs/superpowers/specs/2026-03-21-all-symbolic-step-execution-design.md`, `docs/superpowers/specs/2026-03-25-commands-unify-and-ambient-context-design.md`

## Goal

Replace whole-command forwarding for mutation commands with a single local orchestration model: the presentation host always builds the step plan, stamps each step with its target host, and executes local and remote steps in one ordered command timeline.

The first implementation should optimize for correctness and architectural alignment, not preserving every current progress or cancellation detail. Batch execution of consecutive remote steps is part of the design, but lands after the initial naive version.

## Problem

Today multi-host mutations are routed by forwarding the entire `Command` to a remote daemon. That remote host plans and executes everything as if it were local. This creates two structural problems:

1. Mixed-host plans are awkward. The current checkout plus workspace flow is a TUI-managed two-command dance because checkout and terminal preparation belong on the target host, while workspace creation belongs on the presentation host.
2. The single execution model introduced by all-symbolic steps now exposes the mismatch directly. `build_create_checkout_plan()` always appends a workspace step, so a forwarded checkout can now create a workspace on the remote host instead of locally.

`StepHost::Remote` already exists in core, but `run_step_plan()` ignores it. The execution architecture still assumes "command host" rather than "step host."

## Design Summary

For mutation commands:

- The presentation host always calls `build_plan()`.
- `build_plan()` stamps each step with `StepHost::Local` or `StepHost::Remote(host)`.
- `run_step_plan()` dispatches each step segment based on `Step.host`.
- Remote hosts execute symbolic steps they are given; they do not rebuild or reshape the plan.

For query commands:

- Keep the existing whole-command forwarding path.
- Queries do not use step routing because they do not build plans.

This yields a clean execution split:

- Queries: whole-command forwarding.
- Mutations: local planning plus step-level routing.
- Coalescing: optional optimization layered on top of the mutation path.

## Execution Model

### Local orchestration

The presentation host owns the command lifecycle. It is the only place that:

- builds the plan,
- owns the user-visible command id,
- determines global step ordering,
- broadcasts user-visible progress events,
- interprets cancellation at the command level.

For a mutation command, `run_step_plan()` iterates the plan in order:

- `StepHost::Local` resolves through the existing executor-backed resolver.
- `StepHost::Remote(host)` dispatches that step, or a consecutive segment for the same host, to the remote daemon and waits for ordered step outcomes.

Remote step outcomes are appended to the same `prior` outcome list used by local steps. That preserves the existing symbolic step contract where later steps consume data produced by earlier ones, regardless of where those earlier steps ran.

### Remote execution

The target daemon executes the symbolic steps it receives against its own local providers, filesystem, terminal pool, and workspace manager.

It does not call `build_plan()`, and it does not reinterpret `Command.host`. The plan shape and host routing decisions stay exclusively on the presentation host. This avoids plan drift between hosts and makes step routing a transport concern rather than a second planning system.

### Coalescing

The end state includes batching consecutive remote steps targeting the same host. Coalescing is not a semantic feature; it is a transport optimization. The local orchestrator still sees one ordered global plan with one canonical result stream.

Phase 1 may dispatch only a single remote step at a time. Phase 2 can scan ahead for consecutive `Remote(same_host)` steps and send them as one batch without changing command semantics.

## Protocol Boundary

The remote step transport should target the batched end state immediately, even if phase 1 only sends one step per request. That avoids baking a single-step-only RPC into the wire format.

### Step types move to protocol

Move the symbolic step data model into `flotilla-protocol`:

- `StepHost`
- `StepAction`
- `Step`
- `StepOutcome`

`StepOutcome` already composes with `CommandValue`, so it belongs naturally on the wire. Core remains responsible for planning and resolving steps, but protocol becomes the serialization boundary for remote step execution.

### New routed peer RPC

Add a routed peer message family for remote step execution, distinct from `CommandRequest` and `CommandResponse`.

The request carries:

- the presentation host request id,
- the repo execution context needed to find the tracked repo on the target host, specifically the repo identity plus the repo root/path the presentation host planned against,
- a batch of symbolic steps that all target the same remote host,
- the batch's global step offset so progress can be remapped cleanly on the way back.

The response returns ordered `StepOutcome` values for the batch. Returning only a final success value is insufficient because local follow-on steps may depend on intermediate remote outputs such as checkout paths or prepared terminal results.

The request does not carry provider registry or provider snapshot data. The remote daemon uses the repo identity/context from the request to locate its own tracked repo state, then rebuilds resolver dependencies locally.

### Event flow

Remote execution emits batch-local progress, but the presentation host remains responsible for user-visible command events.

The remote host sends started/succeeded/skipped/failed events for substeps within the batch. The presentation host remaps those batch-local indices into the original global plan indices and emits normal `DaemonEvent::CommandStepUpdate` events.

The TUI therefore continues to observe a flat step timeline. It does not need to understand remote command ids or nested batches to remain usable in phase 1.

## Command Semantics

`Command.host` changes meaning for mutation commands:

- Today: "forward this whole command to that daemon."
- After `#464`: "this host is an input to plan stamping and remote step dispatch."

That semantic shift already aligns with the command unification and ambient context work from `#510`:

- query commands with `Command.host` still use whole-command forwarding,
- mutation commands with `Command.host` are planned locally and step-routed remotely.

The dispatch decision point should be explicit:

- `RemoteCommandRouter::dispatch_execute()` may still use `Command.host` to whole-forward query commands,
- mutation commands must stay on the local daemon so `build_plan()` can run there,
- after plan stamping, execution follows `Step.host`, not `Command.host`.

`build_plan()` reads `Command.host` when it needs to stamp steps, and the `_originating_host` parameter becomes unnecessary. The planner already runs on the presentation host, so `StepHost::Local` naturally means "run back here." No extra originating-host parameter is needed to express the local workspace step.

## Phase Plan

### Phase 1: naive remote step routing

Deliver the architectural change with the simplest correct transport:

- mutation commands stop using whole-command forwarding,
- mixed-host plans are stamped correctly in `build_plan()`,
- remote segments are dispatched through a new step RPC,
- phase 1 may send exactly one step per remote request,
- remote outcomes feed back into the same local outcome stream,
- remote progress is flattened into the existing global step timeline.

This phase fixes the known regression where a remote checkout can incorrectly create a workspace on the remote host.

The target host should execute remote steps through its own normal executor-backed resolver shape, built from its local repo context and local daemon dependencies. This should be a standard `ExecutorStepResolver` constructed on the target host, not a stripped-down resolver populated from wire data.

### Phase 2: coalesced remote batches

Optimize the naive transport:

- detect consecutive remote steps for the same host,
- send them as one batch request,
- improve cancellation behavior for in-flight remote batches if needed,
- optionally enrich TUI progress to surface batch interior state more explicitly.

No TUI protocol changes are required to ship phase 1. Richer visualization can remain follow-up work if the flattened events prove insufficient.

## Error Handling and Cancellation

Cancellation remains command-scoped from the TUI's point of view. The presentation host owns the cancellation token and routes cancellation to the currently active remote segment when necessary.

In phase 1, cancellation semantics are best-effort and may match current behavior for in-flight work:

- if the command is cancelled before dispatching the next segment, execution stops immediately,
- if a remote segment is already in progress, cancellation is forwarded to that remote execution path,
- failures from remote steps surface exactly like local step failures, preserving the existing "earlier meaningful result wins if present" behavior.

Batching must not weaken correctness: a failed substep in a remote batch fails the batch, and the presentation host stops the global plan at that point.

## Known Phase-1 Limitation: Cross-Host State Freshness

The presentation host plans against its current snapshot, but a remote host resolves its steps against its own current provider state. Those views can differ.

Phase 1 should treat the remote host's execution-time state as authoritative. That means some plans may be built from slightly stale information and then resolve differently on the target host. This is acceptable as long as step resolution remains the source of truth and steps stay resolve-time and idempotent where practical. The design does not attempt to solve cross-host snapshot synchronization as part of `#464`.

## Testing

Focus tests on plan behavior and cross-host composition rather than transport plumbing alone.

### Planner tests

- `build_plan()` stamps mixed-host commands with the correct `StepHost`s.
- Remote checkout / terminal preparation steps target the remote host named in `Command.host`.
- Local workspace steps remain `StepHost::Local`.

### Stepper tests

- `run_step_plan()` composes local and remote outcomes into one ordered `prior` list.
- A local step can consume a value produced by a remote step.
- Failures and cancellations stop execution at the correct point.

### Peer/daemon tests

- Remote step execution requests route to the correct host through the existing peer mesh.
- Remote batch progress is remapped to global step indices on the presentation host.
- Cancellation is forwarded to the active remote segment.

### Regression coverage

- A checkout targeted at remote host `B` creates the checkout and prepares the terminal on `B`.
- The workspace step for that same command runs on the presentation host, not on `B`.
- Query commands still use whole-command forwarding and remain unaffected.

## Crate Boundaries

| Change | Crate |
|---|---|
| Serializable step types and remote step peer messages | `flotilla-protocol` |
| Step planning and local/remote step dispatch | `flotilla-core` |
| Remote step execution service on the daemon side | `flotilla-daemon` |
| Whole-command forwarding retained for queries | `flotilla-daemon` |
| Minimal or no changes for flattened progress handling | `flotilla-tui` |

## Scope

### Delivers

- Local planning for all mutation commands
- Correct `StepHost` stamping for mixed-host plans
- Remote execution of symbolic steps without remote replanning
- Flat, presentation-host-owned progress events across local and remote steps
- Correct checkout-plus-workspace behavior across hosts

### Defers

- Coalescing consecutive remote steps into one transport request
- Rich nested batch progress in the TUI
- Any broader capability-aware host selection beyond the existing `Command.host` resolution model
