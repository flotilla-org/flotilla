# Code Size Reduction — Phase 4 (Tasks C, F) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Depends on Phase 3 (`2026-04-15-code-size-reduction-phase-3-implementation.md`). Task C benefits from the fixture builders (Phase 2); Task F benefits from the controller-loop harness (Phase 3) because it moves happy-path tests onto loop-level coverage.

**Goal:** Compact `task_workspace_reconciler.rs` and the secondary reconciler tests using `rstest` parameterisation (Task C), then move happy-path reconciler unit tests outward to controller-loop tests in `provisioning_in_memory.rs`, keeping only edge-case and branch-logic unit tests (Task F).

**Architecture:**

- **Task C:** convert groups of near-identical tests into `#[rstest]` functions with `#[case(...)]` attributes. Good candidates: placement policy variants, cwd variants, host-direct vs docker outcomes, failure propagation.
- **Task F:** for each reconciler file, identify happy-path unit tests, move equivalent coverage to `provisioning_in_memory.rs` using the Phase 3 harness, then delete the unit tests. Keep unit tests for validation, naming, failure mapping, and non-obvious branch logic.

**Tech Stack:** Rust, `rstest`, the fixture builders and controller-loop harness from Phases 2-3.

**Spec:** `docs/plans/2026-04-15-post-pr-code-size-reduction-cleanup-plan.md` — Phase 4.

---

## File Structure

### Task C — rstest compaction
- Modify: `crates/flotilla-controllers/tests/task_workspace_reconciler.rs` (primary)
- Modify: `crates/flotilla-controllers/tests/clone_reconciler.rs` (secondary)
- Modify: `crates/flotilla-controllers/tests/environment_reconciler.rs` (secondary)
- Modify: `crates/flotilla-controllers/tests/terminal_session_reconciler.rs` (secondary)
- Modify: `crates/flotilla-controllers/tests/common/mod.rs` (shared case helpers if needed)

### Task F — move happy paths outward
- Modify: `crates/flotilla-controllers/tests/clone_reconciler.rs`
- Modify: `crates/flotilla-controllers/tests/environment_reconciler.rs`
- Modify: `crates/flotilla-controllers/tests/checkout_reconciler.rs`
- Modify: `crates/flotilla-controllers/tests/terminal_session_reconciler.rs`
- Modify: `crates/flotilla-controllers/tests/provisioning_in_memory.rs` (new observable-behaviour tests)

---

## Task 1: Baseline and categorisation

- [ ] **Step 1: Record baseline line counts**

```bash
wc -l \
  crates/flotilla-controllers/tests/task_workspace_reconciler.rs \
  crates/flotilla-controllers/tests/clone_reconciler.rs \
  crates/flotilla-controllers/tests/environment_reconciler.rs \
  crates/flotilla-controllers/tests/checkout_reconciler.rs \
  crates/flotilla-controllers/tests/terminal_session_reconciler.rs \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs
```

- [ ] **Step 2: Categorise tests per file**

For each of the six files above, go through every `#[test]` / `#[tokio::test]` and tag it as:

- **P** — parameterisable (Task C candidate): a family of near-identical tests that differ only in inputs/expectations.
- **H** — happy-path unit test (Task F candidate): asserts on patch selection or successful output in a scenario that controller-loop tests could cover instead.
- **E** — edge case / validation / naming / failure-mapping / branch logic (keep as-is).

Write the categorisation into a scratch file or PR comment. Don't edit tests yet.

---

## Task 2: Parameterise `task_workspace_reconciler.rs`

- [ ] **Step 1: Identify the first parameterisable group**

Based on Task 1, pick the largest P-tagged group (likely placement-policy strategy variants). Expect 4-8 tests that share setup and differ in spec inputs and expected outputs.

- [ ] **Step 2: Convert to `#[rstest]`**

Template:

```rust
use rstest::rstest;

#[rstest]
#[case::host_direct(
    placement_policy_host_direct(),
    ExpectedOutcome { cwd: "/workspace", placement_kind: PlacementKind::HostDirect }
)]
#[case::docker_per_task(
    placement_policy_docker(),
    ExpectedOutcome { cwd: "/container/workspace", placement_kind: PlacementKind::Docker }
)]
#[tokio::test]
async fn reconciler_places_workspace_according_to_policy(
    #[case] policy: PlacementPolicySpec,
    #[case] expected: ExpectedOutcome,
) {
    let (reconciler, deps) = setup_with_policy(policy).await;
    let outcome = reconciler.reconcile(&task_workspace_fixture(), &deps, now()).into_single_actuation();
    assert_eq!(outcome.placement_kind(), expected.placement_kind);
    assert_eq!(outcome.cwd(), expected.cwd);
}
```

Define `ExpectedOutcome` locally if a concise comparison type helps readability. Use the Phase 2 fixture builders to construct the spec inputs.

- [ ] **Step 3: Run the rstest and verify it still covers each case**

Run: `cargo test -p flotilla-controllers --test task_workspace_reconciler --locked -- reconciler_places_workspace_according_to_policy`
Expected: one test per case, all pass.

- [ ] **Step 4: Delete the old per-variant tests**

Only after step 3 passes. Removing before verifying regressions is the usual trap; resist.

- [ ] **Step 5: Repeat for the next P-group**

Next candidates per spec: cwd variants, host-direct vs docker provisioning outcomes, failure-propagation variants. Each gets its own `#[rstest]` function and its own commit.

- [ ] **Step 6: Commit after each P-group**

Pattern: `test: parameterise <group> in task_workspace_reconciler`.

- [ ] **Step 7: Run full file**

Run: `cargo test -p flotilla-controllers --test task_workspace_reconciler --locked`
Expected: pass.

- [ ] **Step 8: Measure shrinkage**

Run: `wc -l crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
Expect a material reduction vs the Task 1 baseline (goal: at least 25% shorter if the P-groups were real).

---

## Task 3: Parameterise secondary reconciler files where clearly beneficial

For each of `clone_reconciler.rs`, `environment_reconciler.rs`, `terminal_session_reconciler.rs`:

- [ ] **Step 1: Identify P-groups in the file**

Reuse Task 1's categorisation.

- [ ] **Step 2: Apply the rstest template only where it's clearly shorter**

If a file has no clear P-group, skip it. Don't parameterise things that don't benefit. The spec explicitly says:
> Avoid parameterizing: tests whose assertions are already compact, tests where the parameter table would be less readable than explicit cases.

- [ ] **Step 3: Commit per file**

Pattern: `test: parameterise <group> in <file>`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-controllers --locked`
Expected: pass.

---

## Task 4: Move happy-path tests outward — `clone_reconciler.rs`

- [ ] **Step 1: Pick one H-tagged happy-path test**

From Task 1's categorisation, pick the most representative happy-path unit test. Example shape: "given a clone resource with spec X, reconcile produces a `CreateClone` actuation with fields Y and Z."

- [ ] **Step 2: Write an equivalent test in `provisioning_in_memory.rs`**

The equivalent test runs the controller loop and observes the resulting state:

```rust
#[tokio::test]
async fn clone_resource_progresses_to_ready_state() {
    let backend = in_memory_backend();
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(spawn_clone_controller(backend.clone()));

    let clone_name = "my-clone";
    create_clone()
        .backend(&backend)
        .namespace(NAMESPACE)
        .name(clone_name)
        .spec(clone_spec())
        .call()
        .await;

    harness.wait_until_clone_ready(NAMESPACE, clone_name, Duration::from_secs(5)).await;

    let clones = backend.using::<Clone>(NAMESPACE);
    let clone = clones.get(clone_name).await.expect("clone exists");
    assert_eq!(clone.metadata.labels.get("flotilla.work/...").map(String::as_str), Some(...));
    // ... other observable assertions that replace the patch-level assertions
}
```

Use the Phase 2 `create_clone` builder and Phase 3 `ControllerLoopHarness`. Replace patch-level assertions with observable assertions: resulting status phase, child resource creation, observable refs/IDs/paths.

- [ ] **Step 3: Run the new loop test**

Run: `cargo test -p flotilla-controllers --test provisioning_in_memory --locked -- clone_resource_progresses_to_ready_state`
Expected: pass.

- [ ] **Step 4: Delete the corresponding unit test from `clone_reconciler.rs`**

Only after step 3 passes.

- [ ] **Step 5: Repeat for additional H-tests in `clone_reconciler.rs` until only E-tests remain**

Stop when remaining tests cover only naming, validation, failure mapping, and subtle branch logic (the spec's ceiling).

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "test: move clone reconciler happy paths to loop tests"
```

---

## Task 5: Repeat Task 4 for the remaining reconciler files

- [ ] **Step 1: `environment_reconciler.rs`**

Same workflow. Commit: `test: move environment reconciler happy paths to loop tests`.

- [ ] **Step 2: `checkout_reconciler.rs`**

Same workflow. Commit: `test: move checkout reconciler happy paths to loop tests`.

- [ ] **Step 3: `terminal_session_reconciler.rs`**

Same workflow. Commit: `test: move terminal session reconciler happy paths to loop tests`.

---

## Task 6: Verify ceiling criterion

For each reconciler file touched in Tasks 4-5, check the remaining tests meet the spec's ceiling:

> Remaining reconciler unit tests cover only naming, validation, failure mapping, and subtle branch logic.

- [ ] **Step 1: Review each file**

Open each file and walk through every test. Each remaining test should justify itself as an E-category test. If any test still asserts on a happy-path patch selection, either move it outward (Task 4 pattern) or reclassify as a branch-logic test if it specifically exercises a branch condition.

- [ ] **Step 2: Commit cleanup if anything shifted**

```bash
git add -u
git commit -m "test: tidy remaining reconciler unit tests"
```

---

## Task 7: Full verify and metrics

- [ ] **Step 1: Workspace tests**

Run: `cargo test --workspace --locked`
Expected: pass.

- [ ] **Step 2: Lints**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo dylint --all -- --all-targets
```

- [ ] **Step 3: Record final line counts**

Run:
```bash
wc -l \
  crates/flotilla-controllers/tests/task_workspace_reconciler.rs \
  crates/flotilla-controllers/tests/clone_reconciler.rs \
  crates/flotilla-controllers/tests/environment_reconciler.rs \
  crates/flotilla-controllers/tests/checkout_reconciler.rs \
  crates/flotilla-controllers/tests/terminal_session_reconciler.rs \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs
```

Compare against baselines from every phase. Document the total LOC delta across all four phases in the final PR description or release note — this is the quantitative success metric from the spec.

---

## Acceptance check against the spec

- `task_workspace_reconciler.rs` materially shorter — Task 2 steps 7-8
- Strategy-specific tests expressed as cases — Tasks 2, 3
- At least one happy-path unit test per reconciler removed or merged into loop coverage — Tasks 4, 5
- Remaining reconciler unit tests focus on edge cases and branch logic — Task 6 (ceiling enforcement)
