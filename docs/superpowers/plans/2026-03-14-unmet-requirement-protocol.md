# Structured Unmet Requirement Protocol Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace unstable unmet-requirement string output with structured protocol data and add direct in-process coverage for repo detail/provider queries.

**Architecture:** Update the protocol type to `{ factory, kind, value? }`, add a single explicit conversion path from `UnmetRequirement` to the protocol struct in the in-process daemon, and extend integration tests to cover `get_repo_detail` and `get_repo_providers` directly. Adapt CLI human formatting to the new fields.

**Tech Stack:** Rust, serde, tokio, async-trait, cargo test

**Spec:** `docs/superpowers/specs/2026-03-14-unmet-requirement-protocol-design.md`

---

## Chunk 1: Protocol and Formatting

### Task 1: Add a failing protocol-formatting test

**Files:**
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Write a test that expects structured unmet requirements to render sensibly**
- [ ] **Step 2: Run the focused test and verify it fails for the expected reason**
- [ ] **Step 3: Update the protocol type and CLI formatter for `kind` + optional `value`**
- [ ] **Step 4: Re-run the focused test and verify it passes**

### Task 2: Add a failing direct in-process providers test

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write a test for `get_repo_providers` that expects structured unmet requirements and discovery data**
- [ ] **Step 2: Run the focused test and verify it fails for the expected reason**
- [ ] **Step 3: Add explicit `UnmetRequirement` -> `UnmetRequirementInfo` conversion in `crates/flotilla-core/src/in_process.rs`**
- [ ] **Step 4: Re-run the focused test and verify it passes**

## Chunk 2: Query Coverage

### Task 3: Add a failing direct repo detail test

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write a test for `get_repo_detail` that asserts provider health and errors are preserved**
- [ ] **Step 2: Run the focused test and verify it fails for the expected reason**
- [ ] **Step 3: Make the minimal test-support changes needed so the existing implementation satisfies the test**
- [ ] **Step 4: Re-run the focused test and verify it passes**

### Task 4: Verify the combined change

**Files:**
- Modify: `crates/flotilla-protocol/src/query.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Run focused tests for the touched areas**
- [ ] **Step 2: Run the sandbox-safe workspace test command**
- [ ] **Step 3: Review the diff for accidental protocol churn**
