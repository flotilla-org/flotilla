# Workspace Config Daemon Phases — Design Spec

**Issue:** #486 (`bug: WorkspaceConfig spans execution- and presentation-daemon phases`)
**Date:** 2026-03-25
**Related:** #464 / `cb4bb59` (`feat: phase 1 step-level remote routing for mutations (#513)`)

## Goal

Replace the current two-command TUI-managed remote workspace flow with one mutation command planned locally and executed as explicit mixed-host steps.

Make the phase boundary explicit by removing the current cross-phase `WorkspaceConfig` usage and replacing it with two phase-specific types:

- execution-side `PreparedWorkspace`
- presentation-side `WorkspaceAttachRequest`

## Context

`#486` was originally blocked on per-step host routing because the system could not express "prepare on the checkout host, attach on the presentation host" within one command lifecycle.

That prerequisite is now delivered in `origin/main`:

- mixed-host plans are stamped with `StepHost`
- the local stepper dispatches remote step segments
- checkout plans already keep workspace creation local while earlier steps may be remote

What remains is to migrate the workspace flow to consume that capability instead of continuing to use the older TUI choreography:

1. TUI sends `PrepareTerminalForCheckout`
2. execution host returns `TerminalPrepared`
3. TUI sends `CreateWorkspaceFromPreparedTerminal`

That path keeps the phase boundary implicit and requires both daemons to reconstruct parts of the same conceptual workspace state.

## Problems To Solve

### Hidden multi-phase state

`WorkspaceConfig` currently mixes concerns from two different phases:

- checkout-host preparation
- presentation-host workspace attachment

The fields `working_directory`, `template_yaml`, `template_vars`, and `resolved_commands` do not all mean the same thing on both sides of the boundary.

### TUI-owned orchestration

The current remote path is a TUI hack. The executor no longer owns the whole workflow, so correctness depends on UI follow-up behavior rather than one authoritative step plan.

### Two read paths for workspace YAML

The system currently reconstructs workspace information on both sides. For `#486`, the desired behavior is:

- read `.flotilla/workspace.yaml` on the checkout host
- if absent or invalid there, use the fallback/default there
- carry the result forward
- never reopen the workspace template during the attach phase

## Design Summary

Use one unified workspace-creation command for both local and remote checkouts.

That command always plans two logical steps:

1. `PrepareWorkspace`
2. `AttachWorkspace`

For local checkouts:

- `PrepareWorkspace` is `StepHost::Local`
- `AttachWorkspace` is `StepHost::Local`

For remote checkouts:

- `PrepareWorkspace` is `StepHost::Remote(target_host)`
- `AttachWorkspace` is `StepHost::Local`

The TUI dispatches one command only. It does not enqueue any follow-up command from a result handler.

## Phase Boundary

### Execution-side type: `PreparedWorkspace`

`PreparedWorkspace` is the only artifact that crosses from the prepare step into the attach step.

It contains:

- `label`
- `target_host`
- `checkout_path`
- `attachable_set_id`
- `template_yaml`
- `prepared_commands`

This type contains only data the checkout host is authoritative for:

- which checkout is being prepared
- which host owns that checkout
- what template content applies there
- which prepared terminal/session commands were produced there
- which attachable set those commands belong to

### Presentation-side type: `WorkspaceAttachRequest`

`WorkspaceAttachRequest` is the attach-phase input consumed by local workspace-manager code.

It contains:

- `label`
- `working_directory`
- `template_yaml`
- `attach_commands`
- binding metadata needed to persist workspace-to-attachable-set correlation

This phase treats direct command launch as a degenerate attach. The presentation host is responsible for turning prepared execution-side facts into locally runnable attach-oriented commands.

## Authority Rules

### Prepare phase authority

`PrepareWorkspace` runs on the checkout host and is responsible for:

- finding the checkout by host/path
- reading `.flotilla/workspace.yaml` from that checkout
- applying the current fallback/default template behavior there
- preparing terminal/session commands there
- allocating or reusing the attachable set there
- returning `PreparedWorkspace`

It must not know how the presentation host reaches the target host.

### Attach phase authority

`AttachWorkspace` runs on the presentation host and is responsible for:

- consuming `PreparedWorkspace`
- choosing the local fallback working directory for remote-only repos using the current behavior (`repo_root`, else home/cwd/config base)
- resolving attach commands from the presentation host's point of view
- constructing `WorkspaceAttachRequest`
- calling the local `WorkspaceManager`
- persisting workspace binding against the attachable set

It must not reopen `.flotilla/workspace.yaml`, rerun terminal preparation, or reconstruct missing execution-side facts from ambient state.

## Why Hop Resolution Stays Local

Hop-chain resolution should remain on the presentation host.

The hop chain is defined from the launcher's point of view, not the checkout host's point of view. The current hop builder and SSH resolver already assume:

- comparison against the local host at the point of launch
- launcher-local SSH config loaded from the launcher-local config base

If hop resolution moved to the execution host, the execution phase would need additional presentation-host transport context or a serialized "hop prefix" contract. That would reintroduce cross-phase coupling in a different form.

Therefore:

- execution host prepares attachable commands
- presentation host wraps them for local attachment

## Local And Remote Unification

Local and remote workspace creation should use the same logical workflow.

The difference is only `StepHost`, not command shape and not TUI behavior. This keeps the model simple:

- one user action
- one command
- one planner path
- one pair of phase-specific types

## Command/Step Shape

The current command surface can stay user-facing if desired, but the planned workflow must become:

1. build plan for "create workspace"
2. emit `PrepareWorkspace`
3. emit `AttachWorkspace`
4. let `AttachWorkspace` consume prior `PreparedWorkspace`

The existing standalone commands and results used by the TUI hack should be retired from this user flow:

- `PrepareTerminalForCheckout`
- `CreateWorkspaceFromPreparedTerminal`
- `TerminalPrepared`

They may remain temporarily as compatibility internals during migration, but they should no longer be the primary orchestration model.

## Error Handling

- If `PrepareWorkspace` fails, the command fails before any local workspace is created.
- If `AttachWorkspace` fails, the command surfaces that failure in the same command timeline.
- Prior prepared data may exist as step output, but there is no second user-visible command to recover or replay.
- The "earlier meaningful result wins if present" behavior from the step runner should remain unchanged unless the implementation proves it is misleading for this flow.

## Remote-Only Repo Working Directory

For now, preserve the current attach-phase fallback behavior for the local workspace working directory:

1. use `repo_root` if it exists locally
2. else use `HOME`
3. else use current working directory
4. else use config base

Future work may move this to a dedicated temp directory or attachable-set-scoped directory. That is explicitly deferred.

## Testing

### Planner coverage

- local create-workspace builds `PrepareWorkspace` + `AttachWorkspace`, both local
- remote create-workspace builds the same two logical steps, with only `PrepareWorkspace` remote

### Resolver/stepper coverage

- `AttachWorkspace` consumes `PreparedWorkspace` from prior step output
- remote prepare results feed local attach correctly
- attach phase does not reopen workspace YAML

### Regression coverage

- remote workspace creation reads template on the checkout host
- remote workspace attachment uses presentation-host hop resolution
- local workspace creation still follows the same staged path

### TUI coverage

- `Intent::CreateWorkspace` emits one command for both local and remote work items
- result handling no longer enqueues a follow-up workspace command from `TerminalPrepared`

## Scope

### In scope

- one-command workspace creation
- explicit prepare/attach step boundary
- replacement of cross-phase `WorkspaceConfig` usage for this flow
- unified local/remote planner path

### Out of scope

- attachable-set working-directory directories
- agent-produced workspace payload transfer
- moving hop resolution to the execution host
- a broader redesign of workspace manager provider interfaces beyond what this change requires
