# Code Size Reduction — Phase 2 (Task A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Depends on Phase 1 (`2026-04-15-code-size-reduction-phase-1-implementation.md`) being merged. Task E (git helper) is covered by Phase 1; this plan covers Task A — shared resource-fixture helpers — which is the rest of the spec's Phase 2.

**Goal:** Expand the shared test-fixture layer in `crates/flotilla-resources/tests/common/mod.rs` and `crates/flotilla-controllers/tests/common/mod.rs` so tests stop reimplementing `InputMeta` / spec / status / create-and-seed helpers locally. Uses `bon::Builder` derives from Phase 1 plus `#[builder]`-annotated fixture functions.

**Architecture:** Two shared `common` modules expose `#[builder]` functions for:
- metadata construction (labels, annotations, owner refs, finalizers, deletion)
- seeded-resource creation (`create_environment`, `create_clone`, `create_checkout`, `create_host`, `create_convoy_with_single_task`) with optional status

Test files migrate to these helpers, deleting their local duplicates.

**Tech Stack:** Rust, `bon` (already in workspace), `tokio`, `rstest` (workspace dep but not required for this plan).

**Spec:** `docs/plans/2026-04-15-post-pr-code-size-reduction-cleanup-plan.md` — Phase 2, Task A.

---

## File Structure

- Modify: `crates/flotilla-resources/tests/common/mod.rs` — add `#[builder]` fixture functions
- Modify: `crates/flotilla-controllers/tests/common/mod.rs` — add `#[builder]` fixture functions, harmonise with the resources-side helpers
- Modify (migrations): `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`, `crates/flotilla-controllers/tests/provisioning_in_memory.rs`, `crates/flotilla-resources/tests/controller_loop.rs`, `crates/flotilla-resources/tests/provisioning_resources_in_memory.rs`

---

## Task 1: Identify duplication hotspots

- [ ] **Step 1: Enumerate local helpers in the target test files**

Run:
```bash
rg -n '^fn |^async fn |^pub fn |^pub async fn ' \
  crates/flotilla-controllers/tests/task_workspace_reconciler.rs \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs \
  crates/flotilla-resources/tests/controller_loop.rs \
  crates/flotilla-resources/tests/provisioning_resources_in_memory.rs
```

Record the list of local helpers that look like duplicated fixtures. Cross-reference against what's already in `common/mod.rs` for each crate. Note anything that appears in two or more files with nearly identical bodies — those are first migration targets.

- [ ] **Step 2: Record baseline line counts**

Run:
```bash
wc -l \
  crates/flotilla-controllers/tests/task_workspace_reconciler.rs \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs \
  crates/flotilla-resources/tests/controller_loop.rs \
  crates/flotilla-resources/tests/provisioning_resources_in_memory.rs \
  crates/flotilla-resources/tests/common/mod.rs \
  crates/flotilla-controllers/tests/common/mod.rs
```

Save for comparison after the phase completes.

---

## Task 2: Add metadata fixture builder to `flotilla-controllers/tests/common/mod.rs`

The existing `meta`, `labeled_meta`, `task_workspace_meta` helpers will be replaced by a single `#[builder]`-annotated function that exposes all metadata knobs.

- [ ] **Step 1: Add a `controller_meta` builder function**

In `crates/flotilla-controllers/tests/common/mod.rs`, replace the three local meta helpers (`meta`, `labeled_meta`, `task_workspace_meta`) with:

```rust
#[bon::builder]
pub fn controller_meta(
    name: &str,
    #[builder(default)] labels: BTreeMap<String, String>,
    #[builder(default)] annotations: BTreeMap<String, String>,
    #[builder(default)] owner_references: Vec<flotilla_resources::OwnerReference>,
    #[builder(default)] finalizers: Vec<String>,
    deletion_timestamp: Option<chrono::DateTime<chrono::Utc>>,
) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(labels)
        .annotations(annotations)
        .owner_references(owner_references)
        .finalizers(finalizers)
        .maybe_deletion_timestamp(deletion_timestamp)
        .build()
}

pub fn task_workspace_meta(name: &str, repo_url: &str) -> InputMeta {
    let canonical_repo = canonicalize_repo_url(repo_url).expect("repo URL should canonicalize");
    controller_meta()
        .name(name)
        .labels([("flotilla.work/repo-key".to_string(), repo_key(&canonical_repo))].into_iter().collect())
        .call()
}
```

Note: bon's `maybe_<field>` setter takes `Option<T>`. Check bon's documentation for the exact syntax once Phase 1's work has exercised it; if `maybe_deletion_timestamp` is not the right name, use the idiom Phase 1 landed on.

Delete the `meta` and `labeled_meta` standalone functions.

- [ ] **Step 2: Update callers**

Run: `cargo build --workspace --tests --locked`
Expected: compiler errors at any caller using the deleted `meta` or `labeled_meta`. For each one:
- `common::meta("foo")` → `common::controller_meta().name("foo").call()`
- `common::labeled_meta("foo", labels)` → `common::controller_meta().name("foo").labels(labels).call()`

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace --locked`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: replace controller test meta helpers with bon builder"
```

---

## Task 3: Add metadata fixture builder to `flotilla-resources/tests/common/mod.rs`

Mirror Task 2 in the resources crate's common module. The existing `input_meta` / `workflow_template_meta` / `convoy_meta` can be reduced to a single builder function with domain-specific thin wrappers.

- [ ] **Step 1: Add `resource_meta` builder function**

Add to `crates/flotilla-resources/tests/common/mod.rs`:

```rust
#[bon::builder]
pub fn resource_meta(
    name: &str,
    #[builder(default)] labels: BTreeMap<String, String>,
    #[builder(default)] annotations: BTreeMap<String, String>,
    #[builder(default)] owner_references: Vec<OwnerReference>,
    #[builder(default)] finalizers: Vec<String>,
) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(labels)
        .annotations(annotations)
        .owner_references(owner_references)
        .finalizers(finalizers)
        .build()
}
```

Replace the `input_meta`, `convoy_meta`, `workflow_template_meta` function bodies with calls into `resource_meta().name(...).labels(...).annotations(...).call()`. Keep the three convenience wrappers; they carry domain labels and annotations.

- [ ] **Step 2: Verify tests pass**

Run: `cargo test -p flotilla-resources --locked`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "test: replace resource test meta helpers with bon builder"
```

---

## Task 4: Add seeded-resource creation helpers

Provide `#[builder]` helpers for creating resources already in a specific status. Reduces the `backend.create(...); backend.update_status(..., status)` two-step boilerplate.

- [ ] **Step 1: Design the helper shape**

Pick one resource (start with `Environment`) and write the builder:

```rust
#[bon::builder]
pub async fn create_environment(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: EnvironmentSpec,
    status: Option<EnvironmentStatus>,
    #[builder(default)] labels: BTreeMap<String, String>,
) -> ResourceObject<Environment> {
    let meta = resource_meta().name(name).labels(labels).call();
    let typed = backend.clone().using::<Environment>(namespace);
    typed.create(meta, spec).await.expect("create environment");
    if let Some(status) = status {
        typed.update_status(name, &status).await.expect("update env status");
    }
    typed.get(name).await.expect("get environment")
}
```

Place this in `crates/flotilla-resources/tests/common/mod.rs` (so both crates can reuse via the `tests/common/mod.rs` re-export pattern already in use) **or** duplicate the controllers-crate version as needed if the trait bounds differ.

- [ ] **Step 2: Add the same pattern for `Clone`, `Checkout`, `Host`, `Convoy`**

Follow the same template. For `Convoy`, add a `create_convoy_with_single_task` variant whose builder accepts workflow spec inputs.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace --locked`
Expected: passing (no consumers yet, just compilation check).

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: add seeded-resource creation builders"
```

---

## Task 5: Migrate three high-duplication test files to the new helpers

The spec's acceptance criteria require removing duplicated local helpers from at least three test files.

Pick the three files identified in Task 1 that had the most local helpers. Candidates (confirmed by Task 1):

- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- `crates/flotilla-resources/tests/controller_loop.rs`

- [ ] **Step 1: Migrate `task_workspace_reconciler.rs`**

In the target file:
1. Identify local helpers that duplicate the new `common` builders.
2. Replace all call sites with the shared helpers.
3. Delete the local duplicates.
4. Run `cargo test -p flotilla-controllers --locked --test task_workspace_reconciler` — expect pass.
5. Commit.

```bash
git add -u
git commit -m "test: migrate task_workspace_reconciler tests to shared fixtures"
```

- [ ] **Step 2: Migrate `provisioning_in_memory.rs`**

Same process. Commit separately with message `test: migrate provisioning_in_memory tests to shared fixtures`.

- [ ] **Step 3: Migrate `controller_loop.rs`**

Same process. Commit separately with message `test: migrate controller_loop tests to shared fixtures`.

- [ ] **Step 4: Full verify**

```bash
cargo test --workspace --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: all clean.

---

## Acceptance check against the spec

- Shared helpers exist in both `common/mod.rs` files — Tasks 2, 3, 4
- Duplicated local helper functions removed from at least three test files — Task 5
- New tests can use shared helpers by default — established by the shape in Tasks 2-4
- Metadata fixtures use bon's `#[builder]` rather than named variants — Tasks 2, 3
