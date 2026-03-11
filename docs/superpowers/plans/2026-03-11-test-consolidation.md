# Test Consolidation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consolidate duplicated setup and assertions in the newly added Rust tests while preserving behavior and coverage.

**Architecture:** Keep the refactor test-only. Use case tables for parser tests, local builders for repeated provider structs, and shared TUI test helpers for snapshot/delta/error setup so the tests read as behavior checks instead of fixture assembly.

**Tech Stack:** Rust, `cargo test`, existing repo test helpers

---

## Chunk 1: Provider Test Consolidation

### Task 1: Refactor GitHub code review tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/code_review/github.rs`
- Test: `crates/flotilla-core/src/providers/code_review/github.rs`

- [ ] **Step 1: Write the failing test**

Replace duplicated parser checks with table-driven tests and add a compact `GhPr` builder while keeping behavior assertions intact.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-core providers::code_review::github::tests`
Expected: one or more tests fail until helper extraction is complete.

- [ ] **Step 3: Write minimal implementation**

Refactor only the test module. Do not change production code.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-core providers::code_review::github::tests`
Expected: PASS

- [ ] **Step 5: Commit**

Commit together with the other test-consolidation tasks once all verification passes.

### Task 2: Refactor Cursor coding agent tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/cursor.rs`
- Test: `crates/flotilla-core/src/providers/coding_agent/cursor.rs`

- [ ] **Step 1: Write the failing test**

Replace repeated `CursorAgent` and `CursorCodingAgent` construction with small helpers and combine parser-like cases where it improves readability.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-core providers::coding_agent::cursor::tests`
Expected: one or more tests fail until helper extraction is complete.

- [ ] **Step 3: Write minimal implementation**

Refactor only the test module. Do not change production code.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-core providers::coding_agent::cursor::tests`
Expected: PASS

- [ ] **Step 5: Commit**

Commit together with the other test-consolidation tasks once all verification passes.

## Chunk 2: TUI Test Support Consolidation

### Task 3: Add shared app test helpers

**Files:**
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Test: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write the failing test**

Refactor the app tests to call helper constructors for snapshots, deltas, and provider errors.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-tui app::tests`
Expected: one or more tests fail until helper extraction is complete.

- [ ] **Step 3: Write minimal implementation**

Add focused test helpers to `test_support.rs` and update the tests to use them.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-tui app::tests`
Expected: PASS

- [ ] **Step 5: Commit**

Commit together with the other test-consolidation tasks once all verification passes.

## Chunk 3: Verification And Commit

### Task 4: Run full sandbox-safe verification

**Files:**
- Modify: none
- Test: workspace

- [ ] **Step 1: Run the workspace tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 2: Review git diff**

Run: `git diff --stat`
Expected: only the intended test/doc refactor files are changed.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-03-11-test-consolidation-design.md docs/superpowers/plans/2026-03-11-test-consolidation.md crates/flotilla-core/src/providers/code_review/github.rs crates/flotilla-core/src/providers/coding_agent/cursor.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/test_support.rs
git commit -m "test: consolidate new coverage"
```
