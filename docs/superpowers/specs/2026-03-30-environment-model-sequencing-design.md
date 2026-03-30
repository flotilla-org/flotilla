# Environment Model Sequencing

**Status:** Design
**Related:** 2026-03-28 environment model, 2026-03-28 node identity, 2026-03-29 phase-a real-hostid design

## Problem

The target environment model is coherent, but the current implementation still spreads execution-environment responsibilities across multiple layers:

- `InProcessDaemon` owns one ambient `host_bag` and treats discovery as a one-time host-level concern.
- `ExecutorStepResolver` owns provisioned environment lifecycle state (`environment_handles`, `environment_registries`).
- Step execution and workspace orchestration still primarily think in terms of `HostName`, with environments handled as a special-case extension.
- `QualifiedPath` and `host_identity` scaffolding now exist, but they do not yet sit on top of a unified runtime environment model.

This makes future evolution risky. Identity, pathing, discovery, and execution all want to change, but changing them together creates large cross-cutting migrations that are hard to reason about and hard to test.

## Goal

Sequence the work so that:

1. Execution environments become a first-class runtime concept with a single owner.
2. The daemon's ambient host execution context, provisioned environments, and static SSH targets all fit the same conceptual model.
3. Path identity and mesh identity migrations happen after runtime ownership is stable.
4. Each phase has a narrow enough surface area to verify independently.

## Current State

### What already exists

- Provisioned environments already exist via `EnvironmentProvider`, `ProvisionedEnvironment`, and executor steps such as `CreateEnvironment` and `DiscoverEnvironmentProviders`.
- `StepExecutionContext` already distinguishes `Host(HostName)` from `Environment(HostName, EnvironmentId)`.
- `QualifiedPath`, `PathQualifier`, `HostId`, and `host_identity.rs` exist as additive scaffolding.

### What is structurally wrong today

#### Provisioned environments are executor-owned

The current owner of provisioned environment runtime state is `ExecutorStepResolver`, which stores:

- `environment_handles`
- `environment_registries`

That is the wrong abstraction boundary. A step resolver should resolve steps, not own long-lived execution environments.

#### Discovery is ambient-host-first

`InProcessDaemon` computes one `host_bag` at startup and reuses it for repo discovery. That fits the current "one daemon machine = one execution context" model, but it is incompatible with:

- static SSH environments
- multiple direct environments under one daemon
- provisioned environments participating in the same discovery model

#### Environment discovery is bespoke

`DiscoverEnvironmentProviders` reconstructs a minimal `EnvironmentBag` from environment variables and probes factories ad hoc. It is not using the same discovery lifecycle as the daemon's ambient execution context.

#### Path migration is not yet on the critical path

`QualifiedPath` is useful and should remain, but it does not reduce the main architectural risk until environment ownership is unified. A better path key on top of fragmented ownership still leaves the hard part unsolved.

## Recommended Sequencing

### Recommendation

Implement the environment model in two broad stages:

1. **Ownership and runtime model first**
2. **Identity and addressing second**

The key decision is to prioritize **who owns execution environments and discovery** before changing **how paths and nodes are named**.

This avoids the earlier failure mode where too many dimensions changed at once.

## Target Runtime Model

Introduce an `EnvironmentManager` in `flotilla-core` as the single runtime owner of managed execution environments.

It is responsible for:

- registering managed environments
- returning environment-scoped runners and env vars
- running environment discovery
- caching environment-scoped provider registries
- tracking provisioned environment handles
- exposing environment summaries to higher layers

The manager should own both:

- **direct environments**
  - the daemon's own ambient execution context
  - static SSH targets
- **provisioned environments**
  - Docker containers today
  - other provisioners later

This lets the executor depend on a stable runtime service instead of embedding lifecycle state itself.

## Phases

### Phase 1: Extract environment ownership into `EnvironmentManager`

#### Scope

Move provisioned environment lifecycle state out of `ExecutorStepResolver` and into a dedicated manager without changing the visible model yet.

#### Changes

- Add `EnvironmentManager` and `ManagedEnvironment` runtime types.
- Move `environment_handles` and `environment_registries` into the manager.
- Make `CreateEnvironment`, `DiscoverEnvironmentProviders`, and `DestroyEnvironment` delegate to the manager.
- Keep `StepExecutionContext` unchanged for now.
- Keep the local ambient host using the existing discovery path for the moment.

#### Why this first

This is the highest-leverage cleanup. It introduces the right owner without requiring simultaneous protocol or identity changes.

#### Exit criteria

- The executor no longer owns long-lived environment state.
- Environment lifecycle tests target the manager directly.
- Existing provisioned-environment behavior remains intact.

### Phase 2: Model the daemon's ambient host as a direct managed environment

#### Scope

Bring the daemon's own execution context under the same manager.

#### Changes

- Represent the local ambient host execution context as a direct environment.
- Move ambient host discovery behind the manager.
- Replace `InProcessDaemon`'s singleton `host_bag` dependency with manager queries for the direct environment.
- Keep `HostSummary` and peer protocol largely unchanged, but populate them from the manager-backed local direct environment.

#### Why this second

Once the local ambient host is no longer special, the abstraction becomes credible. This is the point where the environment model becomes real rather than container-only.

#### Exit criteria

- Ambient discovery is environment-scoped, not globally host-scoped.
- Local repo/provider discovery flows through the same environment abstraction used for provisioned environments.

### Phase 3: Add static SSH direct environments

#### Scope

Support configured SSH targets as managed direct environments using the same runtime model as the local direct environment.

#### Changes

- Add SSH environment config to daemon configuration.
- Create SSH-backed direct environments with an injected remote `CommandRunner`.
- Run host/environment detection through the same manager APIs.
- Allow repo/provider discovery against SSH direct environments.
- Keep mesh peer semantics separate from SSH execution environments.

#### Why this third

This is the first major feature unlocked by the refactor. It validates that "environment" is the execution unit, not "daemon host".

#### Exit criteria

- The daemon can manage local and SSH direct environments uniformly.
- SSH execution environments do not require standing up a daemon on the target.

### Phase 4: Move summaries and attribution to environment scope

#### Scope

Make environment membership and environment-scoped discovery first-class in the data model.

#### Changes

- Evolve host summaries so they reflect environment-scoped capabilities more explicitly.
- Attribute provider instances and discovered data to `EnvironmentId`.
- Ensure checkout discovery records the environment it came from, even before full `QualifiedPath` normalization is complete.
- Refactor any "host-level provider" assumptions in model and merge code.

#### Why this fourth

This phase makes the runtime model observable, which is necessary before changing path identity and addressing more aggressively.

#### Exit criteria

- Provider attribution is environment-scoped.
- Environment inventories and summaries are coherent enough for UI/correlation follow-up.

### Phase 5: Complete path identity migration

#### Scope

Use the stabilized runtime model to finish the path model cleanly.

#### Changes

- Replace remaining `HostPath`-centric checkout identity usage with `QualifiedPath`.
- Wire real `HostId` through local and SSH direct environments.
- Normalize discovered paths to the most persistent qualifier.
- Introduce mount-table-based translation for provisioned environments where needed.

#### Why this fifth

At this point, each checkout already has a stable owning environment context, so path normalization becomes an isolated identity problem rather than an ownership problem.

#### Exit criteria

- `QualifiedPath` is the checkout identity key.
- Direct environments use real `HostId`.
- Environment-qualified paths are limited to truly environment-local storage.

### Phase 6: Node identity and addressing

#### Scope

After execution environments are stable, rekey mesh identity from `HostName` to `NodeId`.

#### Changes

- Rekey peer maps, vector clocks, routing, hello handshake, and mesh-owned summaries to `NodeId`.
- Retire `HostName` from mesh identity.
- Preserve display names as display-only metadata.

#### Why this last

This phase should operate on a system where execution context has already moved to `EnvironmentId`. That sharply limits the blast radius of the node identity migration.

#### Exit criteria

- Mesh identity and execution identity are fully separated.

## Why not do `QualifiedPath` first?

Because it addresses a downstream identity symptom before fixing the upstream ownership model.

`QualifiedPath` is necessary, but by itself it does not:

- remove executor-owned environment lifecycle state
- unify discovery for local, SSH, and provisioned execution contexts
- reduce the number of places that "environment" logic is spread across

Doing the path migration before ownership unification creates a risk of rewriting the same seams twice.

## Testing Strategy By Phase

### Phase 1

- Unit-test `EnvironmentManager` lifecycle and caching behavior.
- Keep existing executor tests, but shift assertions toward delegation.

### Phase 2

- Add in-process tests proving local direct-environment discovery uses manager-owned state.
- Assert ambient and provisioned environment discovery share the same detector/factory path.

### Phase 3

- Add SSH-runner-backed tests with mocked command runners and env vars.
- Verify static SSH environments can be discovered without peer-daemon setup.

### Phase 4

- Add model-level tests for environment-scoped attribution and summary rendering.

### Phase 5

- Add path identity contract tests covering direct host paths, bind-mounted provisioned paths, and environment-local paths.

### Phase 6

- Add protocol and peer-manager tests for `NodeId` rekeying and trust persistence.

## Rough Work Breakdown

Recommended delivery slices:

1. `EnvironmentManager` extraction for provisioned environments only
2. Local direct environment managed by the same runtime
3. Static SSH direct environments
4. Environment-scoped summaries and attribution
5. Full `QualifiedPath` and real `HostId` migration
6. `NodeId` mesh identity migration

Each slice should land with tests and leave the system in a coherent state.

## Non-Goals For Early Phases

The early phases should explicitly avoid:

- changing peer mesh identity
- rewriting all protocol structures at once
- introducing mount-path translation before environment ownership is stable
- collapsing SSH environments and peer daemons into one concept

Those are later-phase concerns.

## Summary

The correct preparation step is not "finish `QualifiedPath` first" and not "implement the entire environment spec in one pass."

The correct preparation step is:

- introduce a real owner for execution environments
- migrate the ambient host into that ownership model
- extend the model to static SSH direct environments
- only then finish path identity and mesh identity changes

That sequencing puts the highest-risk abstractions in place first and keeps later migrations narrower, more testable, and less entangled.
