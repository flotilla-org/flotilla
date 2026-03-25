# TUI Bug Batch 1 Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the first live TUI bug batch by preventing repo-scoped actions from panicking when no repo is active and by unwinding wedged issue-fetch state when background issue dispatch fails immediately.

**Architecture:** Introduce an explicit optional active-repo access path for repo-scoped UI actions instead of indexing `repo_order` directly. Keep background issue commands fire-and-forget, but route immediate `daemon.execute()` failures back into app state so `issue_fetch_pending` and `status_message` stay coherent.

**Tech Stack:** Rust, ratatui TUI widgets, async daemon dispatch via `tokio`, existing widget/app test harnesses.

---

## Chunk 1: No-Active-Repo Safety

### Task 1: Add failing widget/app tests for overview and empty-repo cases

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`
- Modify: `crates/flotilla-tui/src/widgets/issue_search.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers/tests.rs`

- [ ] **Step 1: Write failing tests**
  Add tests that exercise command-palette search, issue-search dismiss/confirm, and overview-triggered palette behavior with an empty `repo_order`.

- [ ] **Step 2: Run targeted tests to verify they fail**
  Run: `cargo test -p flotilla-tui --locked no_active_repo`
  Expected: FAIL because the current code indexes `repo_order[active_repo]`.

- [ ] **Step 3: Implement minimal optional-active-repo handling**
  Add a shared helper path for repo-scoped actions to return `No active repo` instead of indexing directly.

- [ ] **Step 4: Re-run targeted tests**
  Run: `cargo test -p flotilla-tui --locked no_active_repo`
  Expected: PASS.

## Chunk 2: Background Issue Dispatch Failure Recovery

### Task 2: Add failing executor/app tests for immediate background dispatch failure

**Files:**
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`

- [ ] **Step 1: Write failing tests**
  Add tests for a `FetchMoreIssues` dispatch failure that clears `issue_fetch_pending` and surfaces the error, plus a non-fetch issue action that surfaces the error without wedging state.

- [ ] **Step 2: Run targeted tests to verify they fail**
  Run: `cargo test -p flotilla-tui --locked dispatch_background_issue`
  Expected: FAIL because the spawned task discards the error.

- [ ] **Step 3: Implement minimal background failure reporting**
  Keep spawned issue commands, but send immediate failures back to app state through a small app-owned error channel or equivalent callback path.

- [ ] **Step 4: Re-run targeted tests**
  Run: `cargo test -p flotilla-tui --locked dispatch_background_issue`
  Expected: PASS.

## Chunk 3: Verification

### Task 3: Run focused and package-level verification

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`
- Modify: `crates/flotilla-tui/src/widgets/issue_search.rs`

- [ ] **Step 1: Run focused TUI tests**
  Run: `cargo test -p flotilla-tui --locked`
  Expected: PASS.

- [ ] **Step 2: Run formatting check**
  Run: `cargo +nightly-2026-03-12 fmt --check`
  Expected: PASS.

- [ ] **Step 3: Commit**
  ```bash
  git add docs/superpowers/plans/2026-03-25-tui-bug-batch1.md \
    crates/flotilla-tui/src/app/executor.rs \
    crates/flotilla-tui/src/app/key_handlers/tests.rs \
    crates/flotilla-tui/src/app/mod.rs \
    crates/flotilla-tui/src/app/test_support.rs \
    crates/flotilla-tui/src/widgets/command_palette.rs \
    crates/flotilla-tui/src/widgets/issue_search.rs
  git commit -m "fix: harden tui repo-scoped actions"
  ```
