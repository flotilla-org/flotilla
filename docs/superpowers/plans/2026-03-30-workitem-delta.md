# Work Item Delta Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert repo delta transport and replay to send incremental work-item changes keyed by `WorkItemIdentity` instead of a full `Vec<WorkItem>` on every delta.

**Architecture:** Keep `RepoSnapshot` as the full-state resync path and make `RepoDelta` carry only incremental `Change` entries, including `Change::WorkItem`. Track previous broadcast work items in repo state, emit work-item diffs during refresh, and apply those keyed changes incrementally in the TUI.

**Tech Stack:** Rust, serde protocol structs, indexmap-based diffing, ratatui TUI state, cargo tests

---

## Chunk 1: Protocol and Core Delta Generation

### Task 1: Remove full work-item vectors from repo deltas

**Files:**
- Modify: `crates/flotilla-protocol/src/delta.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Test: `crates/flotilla-protocol/src/lib/tests.rs`

- [ ] **Step 1: Write the failing protocol tests**

Add or update roundtrip tests to serialize and deserialize `RepoDelta`/`DeltaEntry` without a `work_items` field and with `Change::WorkItem` entries inside `changes`.

- [ ] **Step 2: Run targeted protocol tests to verify failure**

Run: `cargo test -p flotilla-protocol --locked`
Expected: FAIL in protocol tests that still construct `RepoDelta`/`DeltaEntry` with `work_items`.

- [ ] **Step 3: Remove `work_items` from the protocol structs**

Update `DeltaEntry` and `RepoDelta` so work-item deltas travel only through `changes`.

- [ ] **Step 4: Run targeted protocol tests to verify pass**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS

### Task 2: Emit work-item diffs in repo-state delta recording

**Files:**
- Modify: `crates/flotilla-core/src/repo_state.rs`
- Modify: `crates/flotilla-core/src/delta.rs`
- Test: `crates/flotilla-core/src/delta/tests.rs`
- Test: `crates/flotilla-core/src/repo_state.rs`

- [ ] **Step 1: Write failing core tests for work-item changes**

Add tests showing that recording a delta with changed work items emits `Change::WorkItem` add/update/remove operations and no full replacement vector.

- [ ] **Step 2: Run targeted core tests to verify failure**

Run: `cargo test -p flotilla-core --locked delta::tests repo_state::tests`
Expected: FAIL because `record_delta()` does not yet include work-item diffs in `changes`.

- [ ] **Step 3: Implement work-item diff tracking**

Add previous-broadcast work-item tracking to `RepoState`, append `diff_work_items()` output to the delta, and update cached work-item state after recording.

- [ ] **Step 4: Run targeted core tests to verify pass**

Run: `cargo test -p flotilla-core --locked delta::tests repo_state::tests`
Expected: PASS

## Chunk 2: Daemon Replay and TUI Delta Application

### Task 3: Stop daemon delta emission from carrying full work-item vectors

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-core/src/in_process/tests.rs`
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write failing daemon tests**

Update delta-selection and replay tests to expect work-item changes inside `changes` and no `RepoDelta.work_items`.

- [ ] **Step 2: Run targeted daemon tests to verify failure**

Run: `cargo test -p flotilla-core --locked in_process::tests --test in_process_daemon`
Expected: FAIL because daemon code still constructs `RepoDelta` with full `work_items`.

- [ ] **Step 3: Remove full work-item payloads from live and replayed deltas**

Update `choose_event()` and `replay_since()` to forward only the combined `changes`.

- [ ] **Step 4: Run targeted daemon tests to verify pass**

Run: `cargo test -p flotilla-core --locked in_process::tests --test in_process_daemon`
Expected: PASS

### Task 4: Apply work-item deltas incrementally in the TUI

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Test: `crates/flotilla-tui/src/app/tests.rs`

- [ ] **Step 1: Write failing TUI tests**

Add tests covering a full snapshot followed by `RepoDelta` work-item add/update/remove changes, asserting the rendered repo data updates incrementally.

- [ ] **Step 2: Run targeted TUI tests to verify failure**

Run: `cargo test -p flotilla-tui --locked app::tests`
Expected: FAIL because `apply_delta()` currently expects a replacement `work_items` vector.

- [ ] **Step 3: Implement incremental work-item application**

Update delta application to mutate the repo’s current work-item state from `Change::WorkItem` ops while leaving snapshot replacement behavior unchanged.

- [ ] **Step 4: Run targeted TUI tests to verify pass**

Run: `cargo test -p flotilla-tui --locked app::tests`
Expected: PASS

## Chunk 3: End-to-End Verification

### Task 5: Run focused and repo-safe verification

**Files:**
- Modify: `crates/flotilla-client/src/lib/tests.rs`
- Modify: `crates/flotilla-tui/src/cli/tests.rs`
- Modify: any remaining compile/test fixes touched by the protocol change

- [ ] **Step 1: Update compile fixtures and helper constructors**

Fix test helpers and fixture constructors affected by the `RepoDelta` shape change.

- [ ] **Step 2: Run focused crate tests**

Run: `cargo test -p flotilla-protocol --locked`
Expected: PASS

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 3: Run repo-safe workspace verification**

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`
Expected: PASS
