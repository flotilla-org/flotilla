# Executor And Server Service Refactor Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `flotilla-core` executor and `flotilla-daemon` server internals into service-owned modules with clearer boundaries, while preserving existing functionality and deferring protocol cleanup until after `#287`.

**Architecture:** Keep `executor.rs` and `server.rs` as shallow facades over explicit service types. Extract shared orchestration logic in executor flows and split daemon transport, routing, and peer runtime behavior into owning services so future protocol work can build on cleaner internals rather than monolithic files.

**Tech Stack:** Rust, Tokio, async-trait, existing `flotilla-core`, `flotilla-daemon`, `flotilla-protocol`, and repo-local test/support utilities.

---

## File Structure

- `crates/flotilla-core/src/executor.rs`
  - Reduce to public facade entrypoints and minimal shared types.
- `crates/flotilla-core/src/executor/`
  - Create focused executor service modules and external test files.
- `crates/flotilla-daemon/src/server.rs`
  - Reduce to daemon server facade and top-level runtime wiring.
- `crates/flotilla-daemon/src/server/`
  - Create focused runtime service modules and external test files.
- `docs/superpowers/specs/2026-03-18-executor-server-service-refactor-design.md`
  - Update only if implementation reveals a real design correction.

Planned executor module split:

- `crates/flotilla-core/src/executor/mod.rs` or keep `executor.rs` as the root facade
- `crates/flotilla-core/src/executor/checkout.rs`
- `crates/flotilla-core/src/executor/workspace.rs`
- `crates/flotilla-core/src/executor/terminals.rs`
- `crates/flotilla-core/src/executor/session_actions.rs`
- `crates/flotilla-core/src/executor/tests.rs`

Planned server module split:

- `crates/flotilla-daemon/src/server/mod.rs` or keep `server.rs` as the root facade
- `crates/flotilla-daemon/src/server/request_dispatch.rs`
- `crates/flotilla-daemon/src/server/remote_commands.rs`
- `crates/flotilla-daemon/src/server/peer_runtime.rs`
- `crates/flotilla-daemon/src/server/client_connection.rs`
- `crates/flotilla-daemon/src/server/peer_connection.rs`
- `crates/flotilla-daemon/src/server/tests.rs`

## Chunk 1: Extract And Stabilize Executor Test Coverage

### Task 1: Move inline executor tests into an external test module

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Create: `crates/flotilla-core/src/executor/tests.rs`
- Test: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Move the existing `#[cfg(test)] mod tests` block into the new file**

Preserve helper types, imports, and test names exactly where possible.

- [ ] **Step 2: Wire the new test module from the executor root**

Use the smallest possible root change, for example:

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 3: Run the targeted executor tests**

Run: `cargo test -p flotilla-core --locked executor`

Expected: PASS with no behavior change.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: split executor tests into module file"
```

### Task 2: Add a focused characterization test for duplicated executor flows

**Files:**
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Add tests that pin today’s shared behavior across plan and immediate paths**

Add at least:

- checkout creation succeeds through `build_plan`
- checkout creation succeeds through `execute`
- remove checkout performs best-effort terminal cleanup in both paths
- teleport creates a new workspace and does not reuse an existing one

The goal is to freeze behavior before service extraction.

- [ ] **Step 2: Run the targeted tests**

Run: `cargo test -p flotilla-core --locked checkout teleport workspace`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor/tests.rs
git commit -m "test: characterize executor orchestration behavior"
```

## Chunk 2: Introduce Executor Services Without Behavioral Change

### Task 3: Extract `CheckoutService`

**Files:**
- Create: `crates/flotilla-core/src/executor/checkout.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Move checkout-specific helpers into `checkout.rs`**

Target code includes:

- target validation
- checkout selector resolution
- issue link persistence
- create/remove checkout operations

- [ ] **Step 2: Introduce a concrete `CheckoutService` type**

Start with a small service API that can be shared by both `build_plan` and
`execute`, for example:

```rust
pub(crate) struct CheckoutService<'a> {
    pub registry: &'a ProviderRegistry,
    pub providers_data: &'a ProviderData,
    pub runner: &'a dyn CommandRunner,
    pub local_host: &'a HostName,
}
```

- [ ] **Step 3: Switch existing call sites to the service without changing semantics**

Do not deduplicate plan/immediate flow logic yet if that obscures the move.
First establish the ownership boundary cleanly.

- [ ] **Step 4: Run targeted executor tests**

Run: `cargo test -p flotilla-core --locked checkout remove_checkout`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/checkout.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: extract checkout service from executor"
```

### Task 4: Extract `WorkspaceOrchestrator`

**Files:**
- Create: `crates/flotilla-core/src/executor/workspace.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Move workspace creation/selection/binding logic into `workspace.rs`**

Target code includes:

- workspace manager lookup
- existing workspace selection
- local and prepared-terminal workspace creation
- attachable set and workspace binding persistence

- [ ] **Step 2: Introduce `WorkspaceOrchestrator` as the owning type**

It should own the rules for:

- reuse vs create
- attachable persistence
- workspace manager interaction

- [ ] **Step 3: Keep `ExecutorStepResolver` delegating into the orchestrator**

The resolver should become a thin bridge, not a second owner of workspace
logic.

- [ ] **Step 4: Run targeted executor tests**

Run: `cargo test -p flotilla-core --locked workspace prepared_terminal`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/workspace.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: extract workspace orchestrator from executor"
```

### Task 5: Extract `TerminalPreparationService`

**Files:**
- Create: `crates/flotilla-core/src/executor/terminals.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Move template parsing/rendering and terminal-pool logic into `terminals.rs`**

Target code includes:

- terminal env var construction
- terminal pool resolution
- prepared terminal command generation
- remote attach command wrapping
- template rendering helpers

- [ ] **Step 2: Normalize duplicate template and terminal resolution paths**

During extraction, consolidate repeated template parsing and terminal command
construction behind the new service. Preserve behavior first; avoid new
features.

- [ ] **Step 3: Use change-aware attachable store calls where applicable**

If the extracted service can avoid unconditional saves without changing
behavior, prefer the `_with_change` helpers already available in the
attachable store.

- [ ] **Step 4: Run targeted executor tests**

Run: `cargo test -p flotilla-core --locked terminal wrap_remote_attach_commands prepare_terminal`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/terminals.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: extract terminal preparation service"
```

### Task 6: Extract session-oriented executor actions and remove orchestration duplication

**Files:**
- Create: `crates/flotilla-core/src/executor/session_actions.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Move session attach/archive/branch-name helpers into `session_actions.rs`**

- [ ] **Step 2: Consolidate duplicated flow logic**

Unify the repeated logic between:

- checkout create plan vs immediate execute
- checkout remove plan vs immediate execute
- teleport plan vs immediate execute

Use shared service operations, not copy-equivalent wrappers.

- [ ] **Step 3: Reduce `executor.rs` to a facade**

After extraction, `executor.rs` should mainly contain:

- shared public types
- public entrypoints
- thin action-to-service delegation

- [ ] **Step 4: Run focused executor tests**

Run: `cargo test -p flotilla-core --locked executor`

Expected: PASS.

- [ ] **Step 5: Run the sandbox-safe core integration test**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/session_actions.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: reduce executor to service facade"
```

## Chunk 3: Extract And Stabilize Server Test Coverage

### Task 7: Move inline server tests into an external test module

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`
- Create: `crates/flotilla-daemon/src/server/tests.rs`
- Test: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Move the existing `#[cfg(test)] mod tests` block into the new file**

- [ ] **Step 2: Wire the new test module from the server root**

Use:

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 3: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked server`

Expected: PASS with no behavior change.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: split server tests into module file"
```

### Task 8: Add characterization tests for server responsibility seams

**Files:**
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Add seam-freezing tests**

Cover at least:

- local request dispatch
- remote execute routing
- remote cancel routing
- peer hello session registration
- client event streaming

These tests should pin current behavior before extraction, not assert the new
module structure.

- [ ] **Step 2: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked dispatch_request handle_client execute_forwarded_command cancel_forwarded_command`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-daemon/src/server/tests.rs
git commit -m "test: characterize server runtime behavior"
```

## Chunk 4: Extract Server Runtime Services

### Task 9: Extract `RemoteCommandRouter`

**Files:**
- Create: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Move forwarded command state and helpers into `remote_commands.rs`**

Target code includes:

- pending remote command tracking
- forwarded command tracking
- remote cancel tracking
- forwarded execute/cancel handlers
- remote event/result completion helpers

- [ ] **Step 2: Introduce an owning `RemoteCommandRouter` type**

It should own the maps and expose methods used by request dispatch and peer
runtime code.

- [ ] **Step 3: Replace direct map manipulation in `dispatch_request` and peer handlers**

`dispatch_request` should delegate instead of directly constructing routed
execute/cancel bookkeeping.

- [ ] **Step 4: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked execute_forwarded_command cancel_forwarded_command dispatch_request_execute_remote dispatch_request_cancel_remote`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/remote_commands.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: extract remote command router"
```

### Task 10: Extract `PeerRuntime`

**Files:**
- Create: `crates/flotilla-daemon/src/server/peer_runtime.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Move peer runtime orchestration into `peer_runtime.rs`**

Target code includes:

- peer runtime spawning
- reconnect loops
- keepalive forwarding
- overlay rebuilds
- local snapshot replication
- routed resync dispatch

- [ ] **Step 2: Introduce `PeerRuntime` as the owner of peer replication behavior**

Keep protocol shape unchanged unless a minimal change is required to support
the boundary cleanly.

- [ ] **Step 3: Remove duplicated reconnect/forward/disconnect patterns during extraction**

The initial-connect and reconnect paths should share one owned runtime flow.

- [ ] **Step 4: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked send_local_to_peer forward_with_keepalive disconnect_peer_and_rebuild`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/peer_runtime.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: extract peer runtime service"
```

### Task 11: Split `handle_client` into `ClientConnection` and `PeerConnection`

**Files:**
- Create: `crates/flotilla-daemon/src/server/client_connection.rs`
- Create: `crates/flotilla-daemon/src/server/peer_connection.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Extract client-session behavior into `ClientConnection`**

Move:

- event subscription forwarding
- request loop
- shutdown-aware client teardown

- [ ] **Step 2: Extract peer-session behavior into `PeerConnection`**

Move:

- hello negotiation
- peer sender registration
- peer wire forwarding
- disconnect cleanup

- [ ] **Step 3: Leave `server.rs` with first-message routing only**

The top-level socket handler should decide which connection type to construct,
then delegate.

- [ ] **Step 4: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked handle_client`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/client_connection.rs crates/flotilla-daemon/src/server/peer_connection.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: split client and peer socket handling"
```

### Task 12: Extract `RequestDispatcher`

**Files:**
- Create: `crates/flotilla-daemon/src/server/request_dispatch.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`

- [ ] **Step 1: Move `dispatch_request` logic into `request_dispatch.rs`**

- [ ] **Step 2: Keep dispatcher transport-agnostic**

It should map requests to daemon or router operations and own the `AgentHook`
update flow, but not socket I/O or peer-loop details.

- [ ] **Step 3: Trim `server.rs` to daemon server lifecycle wiring**

After extraction, `server.rs` should mainly own:

- listener startup/shutdown
- idle timeout watcher
- signal handling
- service construction and task spawning

- [ ] **Step 4: Run targeted daemon tests**

Run: `cargo test -p flotilla-daemon --locked dispatch_request`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server/request_dispatch.rs crates/flotilla-daemon/src/server/tests.rs
git commit -m "refactor: extract request dispatcher"
```

## Chunk 5: Integration Verification And Cleanup

### Task 13: Run repo formatting and targeted clippy

**Files:**
- Modify: any files changed above as required by formatting or lint fixes

- [ ] **Step 1: Run formatter**

Run: `cargo +nightly-2026-03-12 fmt --check`

Expected: PASS, or format the affected files and re-run until PASS.

- [ ] **Step 2: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: PASS.

- [ ] **Step 3: Commit any lint/format cleanup**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server
git commit -m "style: finalize executor and server service refactor"
```

### Task 14: Run CI-parity or sandbox-safe tests and close the refactor

**Files:**
- Modify: any final fixes required by test failures

- [ ] **Step 1: Run the appropriate workspace test command**

If running in the restricted sandbox, use:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Otherwise use:

```bash
cargo test --workspace --locked
```

Expected: PASS.

- [ ] **Step 2: If failures appear, fix them in the owning service module, not by re-bloating the facade files**

- [ ] **Step 3: Commit the final green state**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/src/server
git commit -m "refactor: modularize executor and daemon server"
```

- [ ] **Step 4: Verify the design doc still matches reality**

Re-read:

- `docs/superpowers/specs/2026-03-18-executor-server-service-refactor-design.md`

If implementation forced a meaningful design change, update the spec before
closing the work.
