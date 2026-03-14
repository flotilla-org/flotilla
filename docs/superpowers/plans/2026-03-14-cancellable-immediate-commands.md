# Cancellable Immediate Commands Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make slow immediate commands participate in the existing daemon cancellation flow so `Esc` yields `CommandResult::Cancelled` instead of being ignored.

**Architecture:** Convert `ArchiveSession` and `GenerateBranchName` from `ExecutionPlan::Immediate` to single-step `StepPlan`s, then update the step runner to observe cancellation requested while a step is already running. Verify the daemon behavior with a deterministic slow cloud-agent test and keep provider-level interruption as a separate follow-up.

**Tech Stack:** Rust, Tokio, tokio-util `CancellationToken`, existing in-process daemon and executor tests

---

## File Map

- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  Add an end-to-end regression test with a slow fake cloud-agent provider.
- Modify: `crates/flotilla-core/src/executor.rs`
  Build single-step plans for cancellable immediate commands instead of routing them through the catch-all immediate path.
- Modify: `crates/flotilla-core/src/step.rs`
  Observe cancellation after an awaited step completes and add a unit test for that edge case.

## Chunk 1: Red

### Task 1: Add the failing daemon regression test

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add a slow cloud-agent test runtime**

Create a small fake `CloudAgentService` and `Factory` in the test file. `list_sessions()` should expose one session, and `archive_session()` should block on a `Notify` so the test can cancel while the command is in flight.

- [ ] **Step 2: Add the regression test**

Add a test that:

1. starts an `InProcessDaemon` with the fake runtime
2. refreshes the repo so the session is discoverable
3. executes `ArchiveSession`
4. waits for `CommandStarted`
5. calls `daemon.cancel(command_id)`
6. releases the provider block
7. asserts `CommandFinished` reports `CommandResult::Cancelled`

- [ ] **Step 3: Run the focused test to verify it fails**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon archive_session_can_be_cancelled_while_provider_call_is_in_flight -- --nocapture
```

Expected: FAIL because the command still completes as `Ok` or `Error` instead of `Cancelled`.

## Chunk 2: Green

### Task 2: Wrap slow immediate commands in step plans

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Add a helper for single-step cancellable commands**

Add a small helper that returns `ExecutionPlan::Steps(StepPlan::new(vec![Step { ... }]))` for a provided description and async action.

- [ ] **Step 2: Route `ArchiveSession` through the helper**

Keep the existing session/provider resolution logic, but move it into the single-step closure.

- [ ] **Step 3: Route `GenerateBranchName` through the helper**

Keep the existing branch-name logic and fallback behavior, but run it inside the single-step closure.

- [ ] **Step 4: Run the focused regression test**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon archive_session_can_be_cancelled_while_provider_call_is_in_flight -- --nocapture
```

Expected: Still FAIL until the step runner notices cancellation after the in-flight step returns.

### Task 3: Teach the step runner to honor mid-step cancellation

**Files:**
- Modify: `crates/flotilla-core/src/step.rs`

- [ ] **Step 1: Add a failing unit test**

Add a unit test that starts a step, cancels the token while the step future is blocked, then unblocks the step and expects `CommandResult::Cancelled`.

- [ ] **Step 2: Update `run_step_plan()`**

After each awaited step action resolves, check `cancel.is_cancelled()` before recording success or returning the final result. If set, return `CommandResult::Cancelled`.

- [ ] **Step 3: Run the focused tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked step::tests::cancellation_during_running_step_returns_cancelled -- --nocapture
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon archive_session_can_be_cancelled_while_provider_call_is_in_flight -- --nocapture
```

Expected: PASS

## Chunk 3: Verify

### Task 4: Run broader regression coverage

**Files:**
- Modify: none

- [ ] **Step 1: Run the relevant core test targets**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked executor:: -- --nocapture
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked step:: -- --nocapture
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon -- --nocapture
```

Expected: PASS

- [ ] **Step 2: Run the sandbox-safe workspace tests if time allows**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

### Task 5: File the provider-level follow-up

**Files:**
- Modify: none

- [ ] **Step 1: Open a follow-up issue**

Capture that provider APIs still do not accept cancellation tokens, so long-running HTTP/subprocess work is only cancelled after the current step returns.
