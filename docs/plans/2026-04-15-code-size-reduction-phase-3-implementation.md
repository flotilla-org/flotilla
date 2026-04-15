# Code Size Reduction — Phase 3 (Tasks D, B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Depends on Phase 2 (`2026-04-15-code-size-reduction-phase-2-implementation.md`) — the controller-loop harness reuses the fixture builders, and the contract tests benefit from the shared meta helpers.

**Goal:** Build a controller-loop test harness (Task D) that centralises spawn/wait/abort boilerplate, and build a generic backend contract test harness (Task B) that parameterises CRUD/watch semantics across resource kinds using `rstest`.

**Architecture:**

- **Task D harness:** a helper struct with join-handle tracking, `wait_until_*` methods, and `Drop` that aborts in-flight loops. Lives in `crates/flotilla-controllers/tests/common/mod.rs` so both provisioning and controller-loop tests can use it.
- **Task B contract harness:** a per-resource fixture trait (`ResourceContractFixture`) and a set of `rstest`-driven test functions that exercise every backend implementation against the trait. The trait supplies `meta`, `spec`, `updated_spec`, `status`, and resource-specific assertions.

**Tech Stack:** Rust, `tokio`, `rstest`, `bon` (for fixture builders from Phase 2).

**Spec:** `docs/plans/2026-04-15-post-pr-code-size-reduction-cleanup-plan.md` — Phase 3.

---

## File Structure

### Task D — controller-loop harness
- Modify: `crates/flotilla-controllers/tests/common/mod.rs` — add harness
- Modify: `crates/flotilla-controllers/tests/provisioning_in_memory.rs` — migrate onto harness
- Modify: `crates/flotilla-resources/tests/controller_loop.rs` — migrate onto harness

### Task B — generic backend contract tests
- Create: `crates/flotilla-resources/tests/common/contract.rs` — contract trait + test functions
- Modify: `crates/flotilla-resources/tests/common/mod.rs` — re-export the contract module
- Modify: `crates/flotilla-resources/tests/in_memory.rs` — use contract tests
- Modify: `crates/flotilla-resources/tests/workflow_template_in_memory.rs` — use contract tests
- Optional follow-up: `crates/flotilla-resources/tests/http_wire.rs` (separate PR)

---

## Task 1: Audit existing spawn/wait/abort patterns

- [ ] **Step 1: Enumerate loop spawns and aborts**

Run:
```bash
rg -n 'tokio::spawn|JoinHandle|\.abort\(\)|wait_until' \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs \
  crates/flotilla-resources/tests/controller_loop.rs
```

Record what gets spawned, how abort is triggered, and what `wait_until`-style helpers exist per file. This is the surface the harness must absorb.

---

## Task 2: Implement the `ControllerLoopHarness`

- [ ] **Step 1: Write the harness skeleton**

Add to `crates/flotilla-controllers/tests/common/mod.rs`:

```rust
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct ControllerLoopHarness {
    handles: Vec<JoinHandle<()>>,
    pub backend: ResourceBackend,
    // add resolver fields as needed
}

impl ControllerLoopHarness {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { handles: Vec::new(), backend }
    }

    pub fn spawn(&mut self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.handles.push(tokio::spawn(future));
    }

    pub async fn wait_until<F, Fut>(&self, timeout: Duration, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond().await { return; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not satisfied within {timeout:?}");
    }

    pub async fn shutdown(mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for ControllerLoopHarness {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}
```

The harness takes on the existing top-level `wait_until` function's responsibility; delete it (or reimplement it as `ControllerLoopHarness::wait_until_free` for tests that aren't holding a harness).

- [ ] **Step 2: Add resource-typed `wait_until_status` convenience methods**

Once baseline compiles, add methods like:

```rust
impl ControllerLoopHarness {
    pub async fn wait_until_environment_ready(&self, namespace: &str, name: &str, timeout: Duration) {
        let envs = self.backend.clone().using::<Environment>(namespace);
        self.wait_until(timeout, || async {
            envs.get(name).await.ok()
                .and_then(|env| env.status.map(|s| s.phase == EnvironmentPhase::Ready))
                .unwrap_or(false)
        }).await;
    }
}
```

Add per-resource methods for any status-phase convergence pattern observed in Task 1's audit.

- [ ] **Step 3: Verify**

Run: `cargo build --workspace --tests --locked`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: add ControllerLoopHarness"
```

---

## Task 3: Migrate `provisioning_in_memory.rs` onto the harness

- [ ] **Step 1: Replace inline spawn/abort patterns with harness calls**

Open `crates/flotilla-controllers/tests/provisioning_in_memory.rs`. For each test that currently:
1. Creates loops via `tokio::spawn`
2. Keeps `JoinHandle`s around
3. Calls `.abort()` at the end

Replace with:
```rust
let mut harness = ControllerLoopHarness::new(backend.clone());
harness.spawn(controller_loop_future);
harness.wait_until_<resource>_ready(NAMESPACE, name, Duration::from_secs(5)).await;
// assertions ...
harness.shutdown().await;
```

Delete any local `wait_until` reimplementations.

- [ ] **Step 2: Run the target tests**

Run: `cargo test -p flotilla-controllers --test provisioning_in_memory --locked`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "test: migrate provisioning_in_memory to ControllerLoopHarness"
```

---

## Task 4: Migrate `flotilla-resources/tests/controller_loop.rs` onto the harness

- [ ] **Step 1: Re-export the harness from the resources-crate tests**

The resources crate's `tests/common/mod.rs` does not currently have the harness — it lives in the controllers crate. Cross-crate sharing of test helpers is awkward. Options:
- Move the harness to `flotilla-resources/tests/common/mod.rs` and re-export from `flotilla-controllers/tests/common/mod.rs` via `use flotilla_resources::*;` if reachable, or
- Duplicate the struct in the resources common module (acceptable given both are test-only code and the surface is small).

Pick based on what compiles cleanly. Duplication is fine if the types it references (`ResourceBackend`, resource-specific `wait_until_*`) differ between crates.

- [ ] **Step 2: Migrate each test**

Same pattern as Task 3. Delete local `wait_until`.

- [ ] **Step 3: Run**

Run: `cargo test -p flotilla-resources --test controller_loop --locked`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: migrate resources controller_loop tests to harness"
```

---

## Task 5: Design the `ResourceContractFixture` trait

- [ ] **Step 1: Define the trait**

Create `crates/flotilla-resources/tests/common/contract.rs`:

```rust
use crate::common::*;
use flotilla_resources::{Resource, ResourceBackend, ResourceObject, InputMeta};

pub trait ResourceContractFixture: Resource + Sized {
    /// Human-friendly label used in test names and panic messages.
    fn label() -> &'static str;

    /// Initial metadata for a fixture object.
    fn meta(name: &str) -> InputMeta;

    /// Initial spec.
    fn spec() -> Self::Spec;

    /// A spec that differs from `spec()` — used for update tests.
    fn updated_spec() -> Self::Spec;

    /// Optional status for status-roundtrip tests.
    fn status() -> Option<Self::Status> { None }

    /// Resource-specific assertion comparing two objects' "meaningful" fields
    /// (e.g. ignoring resource_version, creation_timestamp).
    fn assert_equivalent(actual: &ResourceObject<Self>, expected_spec: &Self::Spec);
}
```

Add module declaration at the top of `crates/flotilla-resources/tests/common/mod.rs`:
```rust
pub mod contract;
```

- [ ] **Step 2: Implement the trait for `WorkflowTemplate`**

In `contract.rs` or a sibling fixtures module:

```rust
pub struct WorkflowTemplateFixture;

impl ResourceContractFixture for flotilla_resources::WorkflowTemplate {
    fn label() -> &'static str { "WorkflowTemplate" }
    fn meta(name: &str) -> InputMeta { workflow_template_meta(name) }
    fn spec() -> Self::Spec { valid_workflow_template_spec() }
    fn updated_spec() -> Self::Spec { updated_workflow_template_spec() }
    fn assert_equivalent(actual: &ResourceObject<Self>, expected_spec: &Self::Spec) {
        assert_eq!(&actual.spec, expected_spec);
    }
}
```

Implement for one additional resource (whichever `in_memory.rs` primarily exercises — likely `Environment` or `Convoy`).

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p flotilla-resources --tests --locked`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: introduce ResourceContractFixture trait"
```

---

## Task 6: Write generic contract test functions

- [ ] **Step 1: Write the seven contracts**

In `contract.rs`, add generic async test helpers. Each takes `&ResourceBackend`, a namespace, and uses `F: ResourceContractFixture`.

```rust
pub async fn assert_create_get_list_roundtrip<F: ResourceContractFixture>(
    backend: &ResourceBackend,
    namespace: &str,
) {
    let typed = backend.clone().using::<F>(namespace);
    let meta = F::meta("roundtrip");
    typed.create(meta, F::spec()).await.expect("create");
    let fetched = typed.get("roundtrip").await.expect("get");
    F::assert_equivalent(&fetched, &F::spec());
    let listed = typed.list().await.expect("list").items;
    assert_eq!(listed.len(), 1, "{} list should have one item", F::label());
}

pub async fn assert_stale_resource_version_conflicts<F: ResourceContractFixture>(
    backend: &ResourceBackend,
    namespace: &str,
) { /* ... */ }

// ... and so on for the remaining five contracts:
// - assert_delete_emits_event
// - assert_watch_from_version_replays
// - assert_watch_now_semantics
// - assert_namespace_isolation
// - assert_metadata_roundtrip
```

Write each contract as a small async function. The exact implementation details derive from the current test bodies in `in_memory.rs` — read the existing tests and generalise them onto `F`.

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p flotilla-resources --tests --locked`
Expected: success.

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "test: add ResourceContractFixture contract test helpers"
```

---

## Task 7: Migrate `in_memory.rs` and `workflow_template_in_memory.rs` to use the contract

- [ ] **Step 1: Replace in_memory.rs' per-contract tests with contract-harness calls**

For each of the seven contracts, replace the hand-written test in `in_memory.rs` with:

```rust
#[rstest]
#[case::environment(Environment)]
#[case::convoy(Convoy)]
#[tokio::test]
async fn create_get_list_roundtrip<F: ResourceContractFixture>(#[case] _marker: F) {
    let backend = in_memory_backend();
    assert_create_get_list_roundtrip::<F>(&backend, "flotilla").await;
}
```

If `rstest` parameterisation over generic type parameters is awkward, fall back to explicit monomorphic functions that delegate:

```rust
#[tokio::test]
async fn environment_roundtrip() {
    assert_create_get_list_roundtrip::<Environment>(&in_memory_backend(), "flotilla").await;
}

#[tokio::test]
async fn convoy_roundtrip() {
    assert_create_get_list_roundtrip::<Convoy>(&in_memory_backend(), "flotilla").await;
}
```

Pick whichever reads better; the duplication reduction comes from the contract functions, not the call-site `rstest` gymnastics.

- [ ] **Step 2: Replace `workflow_template_in_memory.rs` contents**

Delete the duplicated tests; the same contract calls cover the workflow-template case via `WorkflowTemplate: ResourceContractFixture`. Keep only workflow-template-specific tests (validation, nothing already in the contract set).

- [ ] **Step 3: Run**

Run: `cargo test -p flotilla-resources --locked`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "test: move in-memory backend tests onto ResourceContractFixture"
```

---

## Task 8: Full verify

- [ ] **Step 1: Workspace-wide test**

Run: `cargo test --workspace --locked`
Expected: all pass.

- [ ] **Step 2: Lints**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo dylint --all -- --all-targets
```

Expected: clean.

- [ ] **Step 3: Record after line counts**

Run:
```bash
wc -l \
  crates/flotilla-controllers/tests/provisioning_in_memory.rs \
  crates/flotilla-resources/tests/controller_loop.rs \
  crates/flotilla-resources/tests/in_memory.rs \
  crates/flotilla-resources/tests/workflow_template_in_memory.rs
```

Record delta vs Phase 2 baseline.

---

## Acceptance check against the spec

- Harness spawns loops, retains handles, exposes backend/resolvers, provides `wait_until_*`, aborts on drop — Task 2
- Provisioning and controller-loop tests no longer duplicate spawn/abort patterns — Tasks 3, 4
- Local `wait_until` implementations consolidated — Tasks 3, 4
- `in_memory.rs` and `workflow_template_in_memory.rs` share one contract harness — Task 7
- Seven contracts expressed (create/get/list roundtrip, stale resource version, delete event, watch replay, watch-now, namespace isolation, metadata roundtrip) — Task 6
- Adding a new resource kind requires only a new fixture impl, not new test bodies — established by the trait shape

## Deferred

- `http_wire.rs` migration onto the contract harness — follow-up PR. Not in this plan because contract shape may change once in-memory tests exercise it first.
