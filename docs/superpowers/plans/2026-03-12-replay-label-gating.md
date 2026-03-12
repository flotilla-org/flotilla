# Replay Label Gating Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compile out replay-only channel label construction from production builds while preserving existing replay behavior in tests.

**Architecture:** Add a small internal helper path in `providers/mod.rs` that uses `cfg(any(test, feature = "replay"))` to choose between real label derivation and a shared noop label. Verify this with tests that panic if a labeler is invoked on the non-replay path and still assert normal labeling on the replay path.

**Tech Stack:** Rust, Cargo features, `cargo test`

---

## Chunk 1: TDD For Macro Gating

### Task 1: Add focused failing tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs`
- Test: `crates/flotilla-core/src/providers/mod.rs`

- [ ] **Step 1: Write the failing test**

Add tests that:
- assert the replay-enabled path still derives labels for each macro shape needed in `mod.rs`
- assert the non-replay helper path returns the noop label without touching a panic-on-use labeler

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-core providers::tests::label`
Expected: FAIL because the noop helper path does not exist yet.

- [ ] **Step 3: Write minimal implementation**

Add the noop label constant/helper functions and route the macros through them.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-core providers::tests::label`
Expected: PASS

- [ ] **Step 5: Commit**

Commit with the production change after workspace verification passes.

## Chunk 2: Feature Wiring And Verification

### Task 2: Add replay feature and verify workspace

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`
- Modify: `docs/superpowers/specs/2026-03-12-replay-label-gating-design.md`
- Modify: `docs/superpowers/plans/2026-03-12-replay-label-gating.md`

- [ ] **Step 1: Add the Cargo feature**

Define an opt-in `replay` feature in `flotilla-core` for non-test builds that want full label derivation.

- [ ] **Step 2: Run focused verification**

Run: `cargo test --locked -p flotilla-core providers::tests::label`
Expected: PASS

- [ ] **Step 3: Run sandbox-safe workspace verification**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 4: Review diff**

Run: `git diff --stat`
Expected: only the intended provider/doc/Cargo files are changed.

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-03-12-replay-label-gating-design.md docs/superpowers/plans/2026-03-12-replay-label-gating.md crates/flotilla-core/Cargo.toml crates/flotilla-core/src/providers/mod.rs
git commit -m "perf: gate replay label construction"
```
