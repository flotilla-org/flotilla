# Step-Level Remote Routing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make mutation commands plan locally and execute symbolic steps on their target hosts while preserving one flat command timeline on the presentation host.

**Architecture:** Move serializable step types into `flotilla-protocol`, add a dedicated routed peer RPC for remote step execution, and teach the local stepper to dispatch `StepHost::Remote` segments through that RPC. Keep whole-command forwarding only for query commands so the mutation path becomes local planning plus step-level routing.

**Tech Stack:** Rust, Tokio, serde, broadcast event streams, routed peer protocol, cargo test

---

## File Structure

- Modify: `crates/flotilla-protocol/src/commands.rs`
  Responsibility: move step status-adjacent transport types into protocol or re-export from a dedicated module if preferred.
- Create: `crates/flotilla-protocol/src/step.rs`
  Responsibility: serializable `StepHost`, `StepAction`, `Step`, and `StepOutcome`.
- Modify: `crates/flotilla-protocol/src/peer.rs`
  Responsibility: add routed peer messages for remote step execution, progress, response, and cancellation.
- Modify: `crates/flotilla-protocol/src/lib.rs`
  Responsibility: re-export protocol step types.
- Modify: `crates/flotilla-core/src/step.rs`
  Responsibility: consume protocol step types and add local/remote dispatch support to the step runner.
- Modify: `crates/flotilla-core/src/executor.rs`
  Responsibility: stamp mixed-host plans correctly from `Command.host`; remove stale originating-host assumptions.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Responsibility: construct the step dispatcher dependencies for local execution.
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
  Responsibility: keep query whole-command forwarding and add remote step routing/cancellation handling.
- Modify: `crates/flotilla-daemon/src/server/peer_runtime.rs`
  Responsibility: dispatch incoming remote step routed messages.
- Test: `crates/flotilla-core/src/executor/tests.rs`
  Responsibility: plan-stamping regression coverage.
- Test: `crates/flotilla-core/src/step/tests.rs`
  Responsibility: local/remote step composition and cancellation behavior.
- Test: `crates/flotilla-daemon/src/server/tests.rs`
  Responsibility: remote step RPC behavior, progress remapping, and cancellation routing.
- Test: `crates/flotilla-protocol/src/peer.rs`
  Responsibility: serde round-trip coverage for new routed peer messages.

## Chunk 1: Protocol Step Transport

### Task 1: Move symbolic step types into protocol

**Files:**
- Create: `crates/flotilla-protocol/src/step.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-core/src/step.rs`
- Test: `crates/flotilla-core/src/step/tests.rs`

- [ ] **Step 1: Write the failing protocol round-trip test**

Add a test that serializes and deserializes a representative `Step` and `StepOutcome`, including a variant that carries a `CommandValue`.

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-protocol --locked step`
Expected: FAIL because the step types do not exist in protocol yet.

- [ ] **Step 3: Add protocol step types**

Create `crates/flotilla-protocol/src/step.rs` with serializable definitions for:

```rust
pub enum StepHost {
    Local,
    Remote(HostName),
}

pub enum StepOutcome {
    Completed,
    CompletedWith(CommandValue),
    Produced(CommandValue),
    Skipped,
}

pub enum StepAction { /* move existing symbolic step variants here */ }

pub struct Step {
    pub description: String,
    pub host: StepHost,
    pub action: StepAction,
}
```

Re-export these from `lib.rs`, and update `flotilla-core/src/step.rs` to use the protocol definitions instead of declaring local duplicates.

- [ ] **Step 4: Run tests to verify the new types round-trip**

Run: `cargo test -p flotilla-protocol --locked step`
Expected: PASS.

- [ ] **Step 5: Run affected core tests**

Run: `cargo test -p flotilla-core --locked step`
Expected: PASS after imports and type ownership are updated.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/step.rs crates/flotilla-protocol/src/lib.rs crates/flotilla-core/src/step.rs crates/flotilla-core/src/step/tests.rs
git commit -m "refactor: move symbolic step types into protocol"
```

### Task 2: Add routed peer messages for remote step execution

**Files:**
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/peer.rs`

- [ ] **Step 1: Write the failing peer message round-trip test**

Add serde round-trip tests for:

- `RemoteStepRequest`
- `RemoteStepEvent`
- `RemoteStepResponse`
- `RemoteStepCancelRequest`
- `RemoteStepCancelResponse`

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-protocol --locked peer`
Expected: FAIL because the new routed peer message variants are not defined.

- [ ] **Step 3: Add the new routed peer message variants**

Extend `RoutedPeerMessage` and any supporting enums with a remote-step RPC family. The request should carry repo execution context, a batch of protocol `Step`s, and a global step offset. The response should carry ordered `Vec<StepOutcome>`.

- [ ] **Step 4: Run the focused protocol tests**

Run: `cargo test -p flotilla-protocol --locked peer`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/peer.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: add routed remote step protocol messages"
```

## Chunk 2: Planner And Stepper

### Task 3: Stamp mixed-host plans in the executor

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Test: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Write the failing planner regression test**

Add a test asserting that a mutation with `Command.host = Some("feta")` produces:

- remote checkout creation,
- remote terminal preparation where applicable,
- local workspace creation.

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-core --locked build_plan`
Expected: FAIL because the plan still stamps every step as `Local`.

- [ ] **Step 3: Implement host stamping in `build_plan()`**

Use `Command.host` when building mutation plans so mixed-host commands stamp the correct `StepHost` variants. Remove or simplify `_originating_host` handling as part of this change.

- [ ] **Step 4: Run the focused planner tests**

Run: `cargo test -p flotilla-core --locked build_plan`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "fix: stamp mixed-host mutation plans by step"
```

### Task 4: Teach the step runner to dispatch remote segments

**Files:**
- Modify: `crates/flotilla-core/src/step.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-core/src/step/tests.rs`

- [ ] **Step 1: Write the failing step-runner tests**

Add tests covering:

- a local step consuming a `Produced` outcome from a remote step,
- remote failure stopping the global plan,
- remote progress mapping preserving global step indices,
- cancellation while a remote segment is active.

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-core --locked step`
Expected: FAIL because `run_step_plan()` ignores `StepHost::Remote`.

- [ ] **Step 3: Introduce a remote step execution dependency**

Add an abstraction that `run_step_plan()` can call for remote execution, for example:

```rust
#[async_trait::async_trait]
pub trait RemoteStepExecutor {
    async fn execute_batch(&self, request: RemoteStepBatchRequest) -> Result<Vec<StepOutcome>, String>;
    async fn cancel_active_batch(&self, command_id: u64) -> Result<(), String>;
}
```

Wire this through the in-process daemon setup so the step runner has both a local resolver and a remote execution path.

- [ ] **Step 4: Implement naive remote segment dispatch**

Update `run_step_plan()` to:

- group the next consecutive `Remote(same_host)` segment,
- call the remote executor,
- append returned outcomes to the local `prior` list in order,
- flatten remote substep progress into the existing command event stream.

Phase 1 may send single-step segments even if the API is batch-shaped.

- [ ] **Step 5: Run the focused step tests**

Run: `cargo test -p flotilla-core --locked step`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/step.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/step/tests.rs
git commit -m "feat: dispatch remote step segments from the local stepper"
```

## Chunk 3: Daemon Remote Step Service

### Task 5: Add daemon handling for remote step requests

**Files:**
- Modify: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Test: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Write the failing daemon routing tests**

Add tests asserting that:

- query commands still emit `CommandRequest`,
- mutation remote steps emit the new remote-step request message,
- remote step events are remapped to the presentation host command id and global step indices.

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-daemon --locked remote_command`
Expected: FAIL because the daemon only knows whole-command forwarding today.

- [ ] **Step 3: Implement remote step request dispatch**

Teach the remote command router to:

- keep using whole-command forwarding for query commands,
- route mutation remote segments through the new step RPC,
- track pending remote step batches and active cancellations separately from forwarded commands if needed.

Update `peer_runtime.rs` to dispatch the new routed peer messages to the remote step service.

- [ ] **Step 4: Implement remote step execution on the target host**

On receipt of a remote step request, execute the provided symbolic steps against a resolver built from the target daemon's local repo/provider context. Send progress events and the ordered `Vec<StepOutcome>` back through the peer mesh.

- [ ] **Step 5: Run the focused daemon tests**

Run: `cargo test -p flotilla-daemon --locked remote_command`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/server/peer_runtime.rs crates/flotilla-daemon/src/server/remote_commands.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "feat: add daemon support for remote step execution"
```

### Task 6: Add cancellation coverage for active remote segments

**Files:**
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Test: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Write the failing cancellation test**

Add a test where the presentation host cancels a command while a remote segment is active and assert that the cancel request reaches the active remote step execution path.

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test -p flotilla-daemon --locked cancel`
Expected: FAIL because cancellation only understands forwarded whole commands.

- [ ] **Step 3: Implement remote step cancellation tracking**

Track the active remote batch per presentation-host command id and route cancellation to that batch on the target host. Preserve the existing timeout/error behavior shape from whole-command cancel where practical.

- [ ] **Step 4: Run the focused cancellation tests**

Run: `cargo test -p flotilla-daemon --locked cancel`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server/remote_commands.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "fix: cancel active remote step segments"
```

## Chunk 4: End-To-End Regression Coverage And Verification

### Task 7: Add regression coverage for remote checkout plus local workspace

**Files:**
- Modify: `crates/flotilla-core/src/executor/tests.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Write the failing regression test**

Add coverage for the current regression:

- checkout targeted at remote host `B`,
- checkout and terminal prep happen on `B`,
- workspace creation remains on the presentation host.

- [ ] **Step 2: Run the focused regression test to verify it fails**

Run: `cargo test -p flotilla-core --locked checkout`
Run: `cargo test -p flotilla-daemon --locked checkout`
Expected: at least one test FAILS before the full stack is complete.

- [ ] **Step 3: Adjust implementation until the regression passes**

Use the existing step and daemon machinery from earlier tasks; do not add TUI-specific command choreography back in.

- [ ] **Step 4: Run the sandbox-safe project verification**

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`
Expected: PASS.

- [ ] **Step 5: Run the exact non-test CI checks required by the repo**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: PASS.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor/tests.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "test: cover step-level remote checkout routing"
```

## Notes For Follow-Up Work

- Coalescing consecutive remote steps for the same host belongs in a follow-up once naive routing is stable.
- If flattened progress proves insufficient, add TUI affordances later without changing the phase-1 execution model.
- Query-command forwarding should remain untouched during the mutation-path refactor except for shared plumbing that now lives alongside the new remote step RPC.
