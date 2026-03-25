# Workspace Config Daemon Phases Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current two-command remote workspace hack with one mixed-host step plan that prepares workspace state on the checkout host and attaches locally from an explicit prepared payload.

**Architecture:** Introduce phase-specific workspace types and route both local and remote create-workspace flows through the same two-step symbolic plan. Preparation reads template data and prepares commands on the checkout host; attachment wraps those commands from the presentation host's perspective and launches the local workspace manager without reopening template files.

**Tech Stack:** Rust, `flotilla-core`, `flotilla-protocol`, `flotilla-tui`, symbolic step plans, hop-chain resolution, workspace-manager providers

---

## File Map

- Modify: `crates/flotilla-protocol/src/step.rs`
- Modify: `crates/flotilla-protocol/src/commands.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/workspace.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`
- Modify: `crates/flotilla-core/src/step.rs`
- Modify: `crates/flotilla-core/src/providers/types.rs`
- Modify: `crates/flotilla-core/src/providers/workspace/mod.rs`
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/intent/tests.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

## Chunk 1: Introduce Explicit Prepare/Attach Types

### Task 1: Add phase-specific protocol and provider types

**Files:**
- Modify: `crates/flotilla-protocol/src/step.rs`
- Modify: `crates/flotilla-protocol/src/commands.rs`
- Modify: `crates/flotilla-core/src/providers/types.rs`

- [ ] **Step 1: Add a failing serialization/unit test for the new prepared workspace payload**

Add or update protocol tests so a `PreparedWorkspace`-carrying step result round-trips through serde and preserves:
- `label`
- `target_host`
- `checkout_path`
- `attachable_set_id`
- `template_yaml`
- `prepared_commands`

- [ ] **Step 2: Run the targeted protocol test to verify the new payload is missing**

Run: `cargo test -p flotilla-protocol --locked prepared_workspace`
Expected: FAIL because the new payload/types are not defined yet.

- [ ] **Step 3: Add `PreparedWorkspace` and new workspace step actions**

Define explicit step-facing types and actions for:
- `PrepareWorkspace`
- `AttachWorkspace`

Keep phase-specific naming in the code:
- execution artifact: `PreparedWorkspace`
- presentation attach input: `WorkspaceAttachRequest`

- [ ] **Step 4: Split provider-facing workspace input from the old cross-phase struct**

Refactor provider types so workspace-manager entry points consume the attach-phase structure instead of a cross-phase config that still tries to represent execution-side state.

- [ ] **Step 5: Run the targeted protocol/provider tests**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS for protocol tests covering the new types.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/step.rs crates/flotilla-protocol/src/commands.rs crates/flotilla-core/src/providers/types.rs
git commit -m "refactor: split workspace prepare and attach types"
```

## Chunk 2: Move Workspace Orchestration Into One Mixed-Host Plan

### Task 2: Replace the old TUI choreography with two symbolic steps

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/workspace.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`
- Modify: `crates/flotilla-core/src/providers/workspace/mod.rs`

- [ ] **Step 1: Add failing planner tests for unified local and remote workspace creation**

Add tests asserting:
- local create-workspace builds `PrepareWorkspace` then `AttachWorkspace`, both local
- remote create-workspace builds the same two steps, with only prepare remote
- checkout plans that auto-create a workspace now emit the same prepare/attach pair

- [ ] **Step 2: Run the targeted planner tests**

Run: `cargo test -p flotilla-core --locked build_plan_create_workspace`
Expected: FAIL because the old actions/plan shape are still in place.

- [ ] **Step 3: Rewrite workspace planning around `PrepareWorkspace` and `AttachWorkspace`**

Update planner code so:
- `CreateWorkspaceForCheckout` becomes a unified staged plan
- remote and local flows differ only by `StepHost`
- checkout auto-workspace uses the same staged workflow

- [ ] **Step 4: Implement prepare-phase resolver logic**

On the checkout host:
- find the checkout
- read `.flotilla/workspace.yaml`
- apply fallback/default there
- prepare terminal/session commands there
- ensure attachable set there
- return `PreparedWorkspace`

- [ ] **Step 5: Implement attach-phase resolver logic**

On the presentation host:
- consume `PreparedWorkspace` from prior step output
- choose the current local fallback working directory behavior
- perform hop wrapping locally
- build `WorkspaceAttachRequest`
- call the local workspace manager without reopening template YAML

- [ ] **Step 6: Update workspace-manager code to consume attach-phase data only**

Ensure local workspace attachment/rendering uses:
- provided `template_yaml`
- provided `attach_commands`

and does not re-read `.flotilla/workspace.yaml` during attach.

- [ ] **Step 7: Run focused core tests**

Run: `cargo test -p flotilla-core --locked workspace`
Expected: PASS for planner, resolver, and regression coverage.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/workspace.rs crates/flotilla-core/src/executor/tests.rs crates/flotilla-core/src/providers/workspace/mod.rs
git commit -m "refactor: unify workspace prepare and attach flow"
```

## Chunk 3: Remove TUI Follow-Up Command Behavior

### Task 3: Make the TUI emit one command only

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/intent/tests.rs`

- [ ] **Step 1: Add failing TUI tests for one-command create-workspace behavior**

Add tests asserting:
- local create-workspace emits one command
- remote create-workspace also emits one command
- result handling no longer queues `CreateWorkspaceFromPreparedTerminal`

- [ ] **Step 2: Run the targeted TUI tests**

Run: `cargo test -p flotilla-tui --locked create_workspace`
Expected: FAIL because the result handler still queues a follow-up command.

- [ ] **Step 3: Remove result-driven workspace follow-up orchestration**

Update TUI intent handling so `Intent::CreateWorkspace` always dispatches the unified command directly.

Remove the `TerminalPrepared`-driven follow-up queueing path from result handling for this workflow.

- [ ] **Step 4: Run focused TUI tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS for intent and executor coverage.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/intent.rs crates/flotilla-tui/src/app/executor.rs crates/flotilla-tui/src/app/intent/tests.rs
git commit -m "refactor: remove workspace follow-up command hack"
```

## Chunk 4: End-to-End Regression Coverage And Verification

### Task 4: Lock in mixed-host behavior and run repo verification

**Files:**
- Modify: `crates/flotilla-daemon/src/server/tests.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Add failing end-to-end regression tests**

Cover:
- remote workspace creation reads template on the checkout host
- attach happens locally from presentation-host hop perspective
- local workspace creation still goes through prepare then attach
- attach phase does not reopen the checkout-host template path

- [ ] **Step 2: Run the targeted end-to-end tests**

Run: `cargo test -p flotilla-core --locked in_process_daemon`
Expected: FAIL until the mixed-host workflow is wired end to end.

- [ ] **Step 3: Fix any remaining event/protocol expectations**

Update daemon/server tests to reflect the new one-command flow and step/result types.

- [ ] **Step 4: Run sandbox-safe repo verification**

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`
Expected: PASS

- [ ] **Step 5: Run formatting and clippy gates**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: PASS

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/server/tests.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "test: cover unified workspace daemon phases"
```

Plan complete and saved to `docs/superpowers/plans/2026-03-25-workspace-config-daemon-phases.md`. Ready to execute?
