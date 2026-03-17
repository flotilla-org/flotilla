# Checkout Anchor Priority Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prefer checkout-anchored work items over attachable-set-anchored work items when both are present in the same correlated group.

**Architecture:** Keep the correlation graph unchanged and change only the final anchor selection in `group_to_work_item`. Preserve attachable-set metadata on the resulting checkout item, and leave attachable-set-only groups unchanged.

**Tech Stack:** Rust, `flotilla-core` correlation model tests, in-process daemon regression tests

---

## Chunk 1: Correlation Model

### Task 1: Flip final anchor precedence

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`
- Test: `crates/flotilla-core/src/data.rs`

- [ ] **Step 1: Write the failing tests**
  - Add/adjust unit tests so a group containing both a checkout and an attachable set produces `WorkItemKind::Checkout`.
  - Keep assertions that `attachable_set_id`, `workspace_refs`, and `terminal_ids` still survive on that checkout item.

- [ ] **Step 2: Run targeted tests to verify they fail**

Run: `cargo test -p flotilla-core --locked correlate_attachable_set`
Expected: FAIL because the current implementation still anchors on `AttachableSet`.

- [ ] **Step 3: Write the minimal implementation**
  - In `group_to_work_item`, choose `CorrelatedAnchor::Checkout` before `CorrelatedAnchor::AttachableSet`.
  - Do not change how `attachable_set_id`, `workspace_refs`, `terminal_ids`, description, or source are populated except as required by the new anchor choice.

- [ ] **Step 4: Run targeted tests to verify they pass**

Run: `cargo test -p flotilla-core --locked correlate_attachable_set`
Expected: PASS

## Chunk 2: Snapshot/Live Regression

### Task 2: Verify user-visible output shape

**Files:**
- Test: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Update/add regression assertions**
  - Ensure snapshot/in-process tests assert that the joined remote checkout remains a checkout item rather than a standalone attachable-set item when both exist.

- [ ] **Step 2: Run targeted verification**

Run: `cargo test -p flotilla-core --locked build_repo_snapshot_with_peers_preserves_remote_attachable_set_for_local_workspace_binding`
Expected: PASS

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: PASS

- [ ] **Step 3: Final focused verification**

Run: `cargo test -p flotilla-core --locked attachable::store::tests`
Expected: PASS
