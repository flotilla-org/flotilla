# Executor Task 6 Step 2 Gap Closure Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the remaining duplicated checkout-create, remove-checkout, and teleport orchestration between `build_plan` and `execute` in the executor.

**Architecture:** Keep `executor.rs` as the facade, but move the last layer of per-command orchestration into small shared flow owners. Each flow owner should expose one authoritative description of the operation and support both plan-building and immediate execution without re-sequencing the same service calls in two places.

**Tech Stack:** Rust, Tokio, existing `flotilla-core` executor services and step runner.

---

## File Structure

- `crates/flotilla-core/src/executor.rs`
  - Reduce duplicated orchestration in `build_plan` and `execute` by delegating to shared flow owners.
- `crates/flotilla-core/src/executor/session_actions.rs`
  - If needed, host teleport flow helpers that are shared by both plan and immediate paths.
- `crates/flotilla-core/src/executor/tests.rs`
  - Keep parity tests for plan and immediate execution and add any missing characterization coverage.

## Chunk 1: Close The Remaining Executor Flow Duplication

### Task 1: Introduce shared flow owners for checkout, remove-checkout, and teleport

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/session_actions.rs`
- Test: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Add or tighten parity tests before the refactor**

Keep the current paired tests for:

- checkout create via `execute` and `build_plan`
- remove checkout via `execute` and `build_plan`
- teleport via `execute` and `build_plan`

If any flow lacks a direct “same outcome through both paths” assertion for the intended behavior, add the smallest characterization test needed.

- [ ] **Step 2: Introduce small flow-owner types or helpers**

Create explicit shared owners for:

- checkout create flow
- remove-checkout flow
- teleport flow

These owners should encapsulate argument normalization and the operation sequence so that both `build_plan` and `execute` call the same owner instead of rebuilding the sequence independently.

- [ ] **Step 3: Make `build_plan` delegate to the shared flow owners**

Replace the current bespoke `build_create_checkout_plan`, `build_remove_checkout_plan`, and `build_teleport_session_plan` orchestration with delegation into the new shared owners.

- [ ] **Step 4: Make `execute` delegate to the same shared flow owners**

Replace the current bespoke `CommandAction::Checkout`, `CommandAction::RemoveCheckout`, and `CommandAction::TeleportSession` arms with delegation into the same owners used by `build_plan`.

- [ ] **Step 5: Keep result semantics unchanged**

Preserve the current behavior that tests already pin:

- checkout plan path may return `CheckoutCreated` while the immediate path still returns its current result shape where applicable
- remove checkout remains best-effort around terminal cleanup
- teleport still creates a fresh workspace instead of reusing an existing one

- [ ] **Step 6: Run targeted verification**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy -p flotilla-core --tests --locked -- -D warnings
cargo test -p flotilla-core --locked executor
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/session_actions.rs crates/flotilla-core/src/executor/tests.rs docs/superpowers/plans/2026-03-18-executor-task6-step2-gap-closure.md
git commit -m "refactor: deduplicate executor checkout and teleport flows"
```
