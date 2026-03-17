# Terminal Scan Authority Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make persisted attachable identity authoritative and limit terminal scans to liveness updates, preventing synthetic duplicate attachable sets from shpool refresh.

**Architecture:** Update shpool refresh to reconcile only previously known sessions, add grace-period handling for known terminals missing from scans, and cover the behavior with provider-level and in-process daemon regressions.

**Tech Stack:** Rust, `flotilla-core`, shpool terminal provider tests, in-process daemon tests

---

## Chunk 1: Unknown Scan Handling

### Task 1: Prevent scan-time synthetic attachable creation

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Test: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Write the failing test**
  - Add a shpool provider test where `list_terminals()` returns a session with no existing persisted binding.
  - Assert that refresh does not create an attachable or attachable set for that session.

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p flotilla-core --locked unknown_shpool`
Expected: FAIL because the current implementation creates a synthetic set.

- [ ] **Step 3: Write the minimal implementation**
  - Change scan-time registration to reconcile only sessions with an existing `terminal_pool/shpool` attachable binding.
  - Log unknown scanned sessions instead of creating identity for them.

- [ ] **Step 4: Run the targeted test to verify it passes**

Run: `cargo test -p flotilla-core --locked unknown_shpool`
Expected: PASS

## Chunk 2: Missing Known Terminal Grace Period

### Task 2: Keep known terminals as disconnected before reaping

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Possibly modify: `crates/flotilla-core/src/attachable/store.rs`
- Test: `crates/flotilla-core/src/providers/terminal/shpool.rs`

- [ ] **Step 1: Write the failing tests**
  - Add a test for a known persisted session missing from the latest scan.
  - Assert it remains present with `Disconnected` status during the grace period.
  - Add a second test showing reap after threshold behavior.

- [ ] **Step 2: Run the targeted tests to verify they fail**

Run: `cargo test -p flotilla-core --locked disconnected_shpool`
Expected: FAIL because missing sessions currently disappear from live provider data immediately.

- [ ] **Step 3: Write the minimal implementation**
  - Track missed-scan state for known terminals.
  - Keep missing terminals in provider data as `Disconnected` until the grace threshold is exceeded.
  - Reap only terminal presence; do not delete attachable-set persistence here.

- [ ] **Step 4: Run the targeted tests to verify they pass**

Run: `cargo test -p flotilla-core --locked disconnected_shpool`
Expected: PASS

## Chunk 3: End-to-End Correlation Regression

### Task 3: Prove one logical checkout yields one logical correlated item

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Possibly modify: `crates/flotilla-core/src/data.rs`

- [ ] **Step 1: Write the failing regression**
  - Add/update an in-process daemon scenario with:
    - a workspace-bound remote checkout
    - known scanned terminals for the same logical checkout
  - Assert the final snapshot has:
    - one checkout work item
    - the expected `attachable_set_id`
    - no extra synthetic attachable-set-only item for the same logical checkout

- [ ] **Step 2: Run the targeted regression to verify it fails**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: FAIL on the duplicate synthetic set behavior before the fix is complete.

- [ ] **Step 3: Make the regression pass**
  - Adjust whichever remaining projection/correlation code is necessary after shpool reconciliation changes.

- [ ] **Step 4: Run focused verification**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: PASS

Run: `cargo test -p flotilla-core --locked`
Expected: PASS or surface any unrelated failures explicitly
