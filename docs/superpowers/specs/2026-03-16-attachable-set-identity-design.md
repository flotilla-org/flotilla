# Attachable Set Identity Design

## Summary

Flotilla's current terminal/workspace model still relies on provider-local identifiers and correlation heuristics:

- terminals are identified by a composite provisioning key like `{checkout}/{role}/{index}`
- workspaces correlate to checkouts via `CheckoutPath(HostName::local(), ...)`
- remote workspaces remain presentation-local objects with no stable link back to the remote terminal state they present

This is sufficient for phase-one remote terminal execution, but it is not a durable model for:

- reopening the same logical workspace in multiple presentations
- correlating local workspaces to remote managed terminals
- giving terminals stable attach targets for `flotilla attach`
- decoupling Flotilla identity from shpool/cmux/tmux/zellij naming schemes

This design introduces a Flotilla-owned identity layer based on `AttachableSet` and `Attachable`.

Initial implementation scope remains terminal-only. The naming intentionally leaves room for future non-terminal members such as URLs or browser targets, but that future is not part of the first rollout.

## Problem

Three different concepts are currently conflated:

1. Logical intent
- "the shell/agent/build surfaces for checkout X"

2. Flotilla identity
- the durable thing the user should reopen, correlate, or attach to later

3. Provider bindings
- shpool session name
- tmux window ref
- zellij tab name
- cmux workspace ref

The current implementation partially encodes intent in `ManagedTerminalId`, partially encodes provider identity in string map keys, and partially recovers relationships from checkout path heuristics.

That creates concrete failures:

- remote workspaces correlate as local because workspace providers always emit `CheckoutPath(HostName::local(), ...)`
- provider-specific moves or renames have no canonical Flotilla identity to preserve intent
- `flotilla attach` has no stable target model yet
- future mixed-content workspace targets would have nowhere to live in the model except by overloading terminal semantics

## Goals

- Introduce a Flotilla-owned opaque identity for logical attachable groups and members
- Make workspaces correlate to attachable sets, not to local checkout-path guesses
- Keep provider-specific identifiers behind a binding layer rather than exposing them as primary user-facing ids
- Preserve a terminal-level opaque id suitable for environment variables, hooks, and future `flotilla attach`
- Support multiple workspaces pointing at the same attachable set
- Keep the first implementation terminal-only and compatible with current provider capabilities

## Non-goals

- Do not implement non-terminal attachables in the first pass
- Do not redesign all provider traits in one step
- Do not require that all members of an attachable set live on the same host forever
- Do not block the existing SSH-wrapped remote terminal path while the new identity model is introduced
- Do not expose provider refs as the main user-facing attach surface

## Terminology

### `AttachableSet`

A logical Flotilla-owned group of attachable members that a workspace can present.

Examples:

- "the standard terminal set for checkout `feat/login`"
- "the remote agent workspace target for checkout `feat/login` on host `desktop`"

An `AttachableSet` is the durable object a workspace points at. Multiple workspaces may present the same set.

### `Attachable`

A single member of an attachable set.

Initial member kind:

- `Terminal`

Future member kinds are intentionally left open, but not implemented in the first pass.

### `ProviderBinding`

A provider-specific reference associated with either an attachable set or an attachable.

Examples:

- shpool session name for a terminal attachable
- workspace-manager workspace ref for a presented attachable set

Bindings are implementation details, not primary Flotilla identity.

## Core Design

### 1. Opaque Flotilla-owned identity

Introduce opaque ids:

- `AttachableSetId`
- `AttachableId`

These should be the canonical Flotilla identifiers. They should not encode checkout name, role, host, or provider ref in their public representation.

This gives Flotilla a stable control surface that survives provider-local naming changes.

### 2. Desired state vs binding state

The model must distinguish between:

- what Flotilla intends to exist
- what providers currently expose

Suggested split:

- `AttachableSetSpec`
- `AttachableSpec`
- `ProviderBinding`

Conceptually:

- `AttachableSetSpec` describes the logical target
- `AttachableSpec` describes each desired member
- `ProviderBinding` records how that set/member is currently realized in a provider

This separation is important for later lifecycle work such as reprovisioning and drift detection.

### 3. Composite keys become provisioning metadata, not identity

The existing terminal purpose tuple:

- checkout
- role
- index

is still useful, but only as provisioning metadata.

It answers:

- which slot in the template this member fulfills
- how to decide whether an existing terminal can satisfy the desired state

It should not remain the durable external identity of a terminal.

### 4. Workspaces point at sets, not checkouts

A managed workspace should know which `AttachableSetId` it is presenting.

That is the key change needed to fix remote correlation:

- today workspaces correlate by checkout path, which is always emitted as local in workspace providers
- after this change workspaces correlate by the `AttachableSetId` they present

The checkout path may remain useful metadata, but it is no longer the primary correlation key for presentation objects.

### 5. Terminals belong to sets

A managed terminal should know:

- its `AttachableId`
- its parent `AttachableSetId`
- its provisioning metadata (`checkout`, `role`, `index`)

This allows:

- workspace <-> terminal correlation through the set
- terminal-specific attach and hook identity through the member id
- later extension to mixed member kinds without changing the parent model

## Proposed Data Model

The exact crate placement can be refined during implementation, but the logical model should look like this:

```rust
pub struct AttachableSetId(String);

pub struct AttachableId(String);

pub enum AttachableKind {
    Terminal,
}

pub struct TerminalPurpose {
    pub checkout: String,
    pub role: String,
    pub index: u32,
}

pub struct AttachableSet {
    pub id: AttachableSetId,
    pub host_affinity: Option<flotilla_protocol::HostName>,
    pub checkout: Option<flotilla_protocol::HostPath>,
    pub template_identity: Option<String>,
    pub members: Vec<AttachableId>,
}

pub struct Attachable {
    pub id: AttachableId,
    pub set_id: AttachableSetId,
    pub kind: AttachableKind,
    pub terminal_purpose: Option<TerminalPurpose>,
    pub command: String,
    pub working_directory: std::path::PathBuf,
    pub status: flotilla_protocol::TerminalStatus,
}

pub struct ProviderBinding {
    pub provider_category: String,
    pub provider_name: String,
    pub object_kind: BindingObjectKind,
    pub object_id: String,
    pub external_ref: String,
}

pub enum BindingObjectKind {
    AttachableSet,
    Attachable,
}
```

Notes:

- `host_affinity` is metadata, not identity
- `checkout` is optional because future attachable sets may not be checkout-bound
- `template_identity` is intentionally loose in the first design; later lifecycle work can define whether this is a template path, version hash, or rendered spec hash

## Correlation Model

### Current state

Current correlation is built from:

- checkout correlation keys
- workspace `CheckoutPath` keys
- managed terminal branch + working directory keys

This works only as long as:

- the terminal and workspace both live on the local host
- the workspace manager can surface the same checkout path that the terminal pool used

That assumption fails for remote prepared workspaces.

### Proposed state

Introduce explicit correlation through attachable sets:

- terminals correlate to their `AttachableSetId`
- workspaces correlate to their `AttachableSetId`
- checkouts may remain associated to a set through metadata or association keys

This gives a more accurate model:

- checkout is a domain object
- attachable set is a presentation/execution target
- workspace is one view onto that target

The work item model should eventually surface:

- zero or more `workspace_refs`
- zero or more terminal/member ids
- the associated `AttachableSetId` for debug/introspection and later commands

## Workspace Manager Integration

Workspace managers should remain presentation providers, but Flotilla needs a managed representation of the workspaces they create or observe.

That representation should record:

- provider workspace ref
- workspace name
- presented `AttachableSetId`
- optional local directory metadata for fallback/debugging

The critical point is that workspace managers do not need to own attachable identity. They only bind to it.

For local providers that cannot report attachable-set metadata directly from the underlying tool:

- Flotilla should persist the binding when creating the workspace
- subsequent list operations should enrich observed workspaces from that persisted binding state

This is similar to the existing tmux/zellij state enrichment, but the persisted data becomes identity-bearing rather than merely descriptive.

## Terminal Pool Integration

The terminal pool remains responsible for terminal lifecycle, but its public model should move from "terminal keyed by purpose tuple" toward:

- `Attachable` with opaque id
- provisioning metadata carried alongside it

For shpool specifically:

- the shpool session name becomes a provider binding
- it should not be treated as the primary Flotilla identity

The existing `{checkout}/{role}/{index}` tuple still matters for:

- matching template intent to an attachable member
- deciding whether to reuse or reprovision

But it no longer stands in for the terminal's durable identity.

## Multi-host Implications

`AttachableSet` is the correct place to avoid baking in "all members are on one host" as a permanent invariant.

For the first implementation:

- a set will effectively be host-local in practice
- terminals in the set will all come from one terminal pool on one host

But the model should not require that forever.

This matters because:

- the presentation host is already distinct from the execution host
- future workflows may want a set that includes members from more than one host

That future should not require another rename or model reset.

## `flotilla attach`

This design gives `flotilla attach` a clean future target model:

- `flotilla attach <attachable-id>` attaches to a single member

Notably:

- attach should target `AttachableId`, not provider refs
- attach should not require users to know shpool session names, tmux ids, or host-specific internal refs

`AttachableSetId` remains the better target for:

- workspace creation
- workspace reopening
- "show me the managed execution target for this work item"

## Environment Variables and Hooks

Terminal sessions should receive opaque Flotilla identity via environment variables.

The important additions are:

- attachable member id
- attachable set id

The current provisioning tuple may still be useful as secondary metadata, but the opaque ids should be the canonical hook surface.

That supports:

- later `flotilla attach` from inside a terminal
- hook correlation back to Flotilla
- avoiding dependence on provider naming schemes

## Storage and Binding Persistence

At least one persisted registry is needed outside individual providers.

That registry should own:

- attachable sets
- attachables
- provider bindings

This is the concrete realization of the need described in issue `#327`.

The first implementation can stay repo-local or config-local as long as it is:

- durable across daemon restarts
- independent of any single provider state file
- accessible when enriching provider list results

## Migration Strategy

### Phase 1: Add internal model without changing user-visible behavior

- define `AttachableSet` / `Attachable`
- add storage for ids and bindings
- continue exposing current terminal/workspace behavior

### Phase 2: Bind shpool terminals into the new model

- create attachable members for managed terminals
- record provider binding to shpool session name
- keep current provisioning tuple as metadata

### Phase 3: Bind workspace creation to `AttachableSetId`

- when Flotilla creates a workspace, persist which set it presents
- on refresh, enrich listed workspaces with that binding
- correlate workspaces to sets rather than checkout-path heuristics

### Phase 4: Move work-item correlation to attachable sets

- use set identity as the primary link between workspaces and managed terminals
- keep checkout-path correlation only as fallback/debugging metadata where useful

### Phase 5: Build lifecycle reconciliation on top

- desired set spec vs actual attachable members
- reprovisioning policy
- stale binding cleanup

### Phase 6: Add `flotilla attach`

- target `AttachableId`
- resolve route/provider bindings internally

## Issue Mapping

### `#177` — Add managed terminals to delta snapshot pipeline

Still independent and still worth doing first. It fixes propagation of terminal state changes and does not depend on the new identity model.

### `#327` — provider-id mapping for ephemeral host-tied resources

This becomes the foundation issue for:

- `AttachableSet`
- `Attachable`
- `ProviderBinding`

The issue should be rewritten away from a generic mapping concept and toward this concrete identity registry.

### `#360` — Remote workspace correlation

This becomes:

- workspaces bind to `AttachableSetId`
- terminals bind to `AttachableSetId`
- correlation flows through the set, not through local checkout-path heuristics

### `#239` — Managed terminal pool with identity and lifecycle management

This remains the broader lifecycle vision, but should be reframed as building on the attachable-set identity layer rather than inventing a separate one.

The issue is too broad to implement in one slice. It should explicitly depend on the `#327` foundation.

### `#368` — transport-agnostic `flotilla attach`

This should target `AttachableId`.

The issue should remain separate because it is primarily a control-surface problem, but its target model depends on this design.

## Recommended Implementation Order

1. `#177` — delta support for managed terminals
2. Add `AttachableSet` / `Attachable` / binding registry primitives
3. Bind terminal pool members into that model
4. Bind managed workspaces to `AttachableSetId`
5. Fix remote workspace correlation through set identity
6. Layer lifecycle reconciliation onto the new model
7. Add `flotilla attach` on `AttachableId`

## Open Questions

- Where should the attachable registry live: protocol-visible provider data, a separate internal store, or both?
- How much of the attachable model should be replicated across peers versus reconstructed locally from replicated provider data plus bindings?
- What is the right persisted representation of `template_identity` for later reprovisioning?
- Should work items eventually expose `AttachableSetId` directly, or should that remain an internal implementation detail until a command needs it?
- Do we want one binding table for all ephemeral provider-backed objects, or separate tables keyed by attachable sets and attachables?

## Recommendation

Write implementation against this design in small slices rather than creating a single umbrella refactor.

The most important architectural commitment is:

- `AttachableSet` is the durable logical target for workspace presentation
- `Attachable` is the durable logical target for attach operations
- provider refs are bindings, not identity

That is the model shift needed to make terminal pools, remote workspaces, and future attach semantics coherent.
