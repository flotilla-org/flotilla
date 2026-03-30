# Environment Manager Phase 1-2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a real `EnvironmentManager` as the runtime owner of managed execution environments, then migrate the daemon's ambient host execution context onto the same abstraction without yet taking on SSH environments, `QualifiedPath` completion, or `NodeId`.

**Architecture:** First extract provisioned-environment lifecycle and discovery state out of `ExecutorStepResolver` into a dedicated manager service in `flotilla-core`. Then represent the daemon's ambient execution context as a managed direct environment and route local discovery, provider lookup, and host-summary construction through that service. Preserve the current protocol shape and `StepExecutionContext` transport semantics during this tranche so the main change is ownership and runtime model, not addressing.

**Tech Stack:** Rust, async-trait, tokio, flotilla-core, flotilla-protocol, flotilla-daemon

**Spec:** `docs/superpowers/specs/2026-03-30-environment-model-sequencing-design.md`

---

## File Structure

Planned new or heavily modified files for Phases 1 and 2:

- Create: `crates/flotilla-core/src/environment_manager.rs`
  Runtime owner for managed environments, direct-environment state, provisioned-environment handles, and environment-scoped discovery/registry caching.
- Modify: `crates/flotilla-core/src/lib.rs`
  Export the new module.
- Modify: `crates/flotilla-core/src/executor.rs`
  Remove ownership of environment lifecycle state from `ExecutorStepResolver`; delegate environment actions and environment-scoped lookups to `EnvironmentManager`.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Create and store the `EnvironmentManager`; stop storing a singleton ambient `host_bag`; route startup discovery and repo registration through the local direct environment.
- Modify: `crates/flotilla-core/src/host_summary.rs`
  Build local summaries from manager-backed local direct-environment state rather than a free-standing ambient host bag.
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
  Add any helper APIs needed to run host detectors and provider discovery per managed environment without coupling to `InProcessDaemon`.
- Modify: `crates/flotilla-core/src/daemon.rs`
  Add any daemon-facing accessors needed for manager-backed environment state, if required by callers.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Ensure daemon startup wires the manager-backed local environment cleanly and keeps host-summary publication unchanged at the transport layer.
- Modify: `crates/flotilla-core/src/executor/tests.rs`
  Replace direct assertions on resolver-owned environment maps with assertions against manager behavior and delegation.
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  Add regression tests proving local repo/provider discovery runs via the managed direct environment.

Later phases intentionally deferred from detailed planning:

- Static SSH environments
- Environment-scoped summaries visible to peers and UI
- `QualifiedPath` / real `HostId` completion
- `NodeId` mesh identity migration

## Task 1: Introduce `EnvironmentManager` core types and contracts

**Files:**
- Create: `crates/flotilla-core/src/environment_manager.rs`
- Modify: `crates/flotilla-core/src/lib.rs`
- Test: `crates/flotilla-core/src/environment_manager.rs`

- [ ] **Step 1: Define the runtime model in the new module**

Create `crates/flotilla-core/src/environment_manager.rs` with focused runtime types:

```rust
use std::{collections::HashMap, sync::Arc};

use flotilla_protocol::EnvironmentId;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{DiscoveryRuntime, EnvironmentBag},
        environment::EnvironmentHandle,
        registry::ProviderRegistry,
        CommandRunner,
    },
};

#[derive(Clone)]
pub enum ManagedEnvironmentKind {
    Direct(DirectEnvironmentState),
    Provisioned(ProvisionedEnvironmentState),
}

#[derive(Clone)]
pub struct DirectEnvironmentState {
    pub runner: Arc<dyn CommandRunner>,
    pub env_bag: EnvironmentBag,
}

#[derive(Clone)]
pub struct ProvisionedEnvironmentState {
    pub handle: EnvironmentHandle,
    pub env_bag: EnvironmentBag,
    pub registry: Option<Arc<ProviderRegistry>>,
}

pub struct EnvironmentManager {
    local_environment_id: EnvironmentId,
    discovery: DiscoveryRuntime,
    managed: std::sync::Mutex<HashMap<EnvironmentId, ManagedEnvironmentKind>>,
}
```

Keep the first version deliberately narrow. The manager only needs enough state to own environments and answer lookups used by the executor and `InProcessDaemon`.

- [ ] **Step 2: Add a construction API for the local direct environment**

Add a constructor that takes the daemon's `DiscoveryRuntime` plus a local direct `EnvironmentId`, runs host detectors with the discovery runtime's injected runner/env, and registers the direct environment as the initial managed environment:

```rust
impl EnvironmentManager {
    pub async fn new_local(
        discovery: DiscoveryRuntime,
        local_environment_id: EnvironmentId,
    ) -> Self {
        let env_bag = crate::providers::discovery::run_host_detectors(
            &discovery.host_detectors,
            &*discovery.runner,
            &*discovery.env,
        ).await;

        let mut managed = HashMap::new();
        managed.insert(
            local_environment_id.clone(),
            ManagedEnvironmentKind::Direct(DirectEnvironmentState {
                runner: Arc::clone(&discovery.runner),
                env_bag,
            }),
        );

        Self {
            local_environment_id,
            discovery,
            managed: std::sync::Mutex::new(managed),
        }
    }
}
```

- [ ] **Step 3: Add minimal lookup methods needed by the rest of the system**

Add APIs to:

- fetch the local direct environment id
- fetch an environment-scoped runner
- fetch an environment-scoped bag
- fetch an environment-scoped provider registry if one exists
- register/unregister provisioned environments

Keep these narrow and explicit rather than adding a generic catch-all.

- [ ] **Step 4: Add focused unit tests for the manager core**

In the new module, add tests that prove:

- `new_local()` registers one direct environment
- `local_environment_id()` returns the configured id
- direct-environment runner/bag lookups succeed
- registering then removing a provisioned environment updates internal state

Use discovery test support and mock runners where possible instead of filesystem-heavy setup.

- [ ] **Step 5: Run the focused tests**

Run: `cargo test -p flotilla-core environment_manager -- --nocapture`
Expected: all new environment manager tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/environment_manager.rs crates/flotilla-core/src/lib.rs
git commit -m "feat: add environment manager core runtime"
```

## Task 2: Move provisioned-environment ownership from the executor into `EnvironmentManager`

**Files:**
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Extend `EnvironmentManager` with provisioned-environment lifecycle methods**

Add methods to:

- create and store a provisioned environment handle
- discover providers for a provisioned environment and cache the resulting registry
- destroy and unregister a provisioned environment

These methods should encapsulate the logic currently embedded in `ExecutorStepResolver` for:

- `CreateEnvironment`
- `DiscoverEnvironmentProviders`
- `DestroyEnvironment`

Shape them around the existing behavior before changing semantics. For example:

```rust
pub async fn create_provisioned_environment(
    &self,
    env_id: EnvironmentId,
    provider: &str,
    registry: &ProviderRegistry,
    repo_root: &ExecutionEnvironmentPath,
    daemon_socket_path: &crate::path_context::DaemonHostPath,
    prior: &[crate::step::StepOutcome],
) -> Result<(), String>;
```

If that signature feels too executor-shaped, split it into smaller helpers, but keep ownership entirely inside the manager.

- [ ] **Step 2: Move the bespoke environment discovery logic behind the manager**

Port the existing `DiscoverEnvironmentProviders` behavior into the manager:

- derive the environment runner from the stored handle
- build the initial `EnvironmentBag`
- probe factories
- cache the resulting `ProviderRegistry`

Do not change the current discovery semantics yet beyond moving the owner. The goal of this task is relocation, not redesign.

- [ ] **Step 3: Replace resolver-owned state with a manager dependency**

In `crates/flotilla-core/src/executor.rs`, change `ExecutorStepResolver` from:

```rust
pub environment_handles: std::sync::Mutex<HashMap<EnvironmentId, EnvironmentHandle>>,
pub environment_registries: std::sync::Mutex<HashMap<EnvironmentId, Arc<ProviderRegistry>>>,
```

to:

```rust
pub environment_manager: Arc<crate::environment_manager::EnvironmentManager>,
```

Update all environment-scoped resolution paths to use manager queries instead of direct map access.

- [ ] **Step 4: Rewrite `StepAction` handlers to delegate**

Update the `CreateEnvironment`, `DiscoverEnvironmentProviders`, and `DestroyEnvironment` action handlers so they call the manager rather than mutating local resolver state.

Keep the surrounding step contract unchanged:

- `CreateEnvironment` still returns `EnvironmentCreated`
- `DiscoverEnvironmentProviders` still returns `Completed`
- `DestroyEnvironment` still returns `Completed`

- [ ] **Step 5: Update environment-context resolution to use manager lookups**

In the `StepExecutionContext::Environment(_, env_id)` branch, replace direct map access with manager lookups for:

- effective runner
- effective registry
- container name when building prepared workspaces

This is the moment where the executor becomes a consumer rather than an owner.

- [ ] **Step 6: Update executor tests**

Refactor the existing executor tests that currently construct resolver-owned environment maps. Replace those fixtures with an `EnvironmentManager` test helper that can register a mock provisioned environment and pre-seed a mock environment registry.

Update assertions to verify behavior, not internal storage.

- [ ] **Step 7: Run focused executor tests**

Run: `cargo test -p flotilla-core executor -- --nocapture`
Expected: existing environment lifecycle and environment-step tests still pass with the manager-backed resolver

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/environment_manager.rs crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "refactor: move provisioned environment ownership into environment manager"
```

## Task 3: Migrate `InProcessDaemon` startup to use a manager-backed local direct environment

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add a manager field to `InProcessDaemon`**

In `crates/flotilla-core/src/in_process.rs`, add:

```rust
environment_manager: Arc<crate::environment_manager::EnvironmentManager>,
local_environment_id: EnvironmentId,
```

Initialize both during daemon construction. For this phase, a stable local environment id can continue to use the existing mechanism already present in the codebase for environment ids; do not expand this plan into the later `HostId`/identity migration.

- [ ] **Step 2: Stop storing a singleton ambient `host_bag` as daemon-owned state**

Remove the long-lived `host_bag` field from `InProcessDaemon`.

Replace usages with manager-backed local direct-environment lookups. The manager should now be the owner of the ambient direct environment's discovery bag.

- [ ] **Step 3: Route startup repo discovery through the local direct environment**

During `InProcessDaemon::new()`, fetch the local direct environment bag from the manager and use that for `discover_providers(...)`.

This should preserve current behavior while proving that ambient discovery now flows through the manager-owned direct environment.

- [ ] **Step 4: Route later repo additions and rediscovery through the manager**

Update repo registration and any later code paths that currently merge or inspect `self.host_bag` so they instead retrieve the local direct environment bag from the manager.

Key call sites include:

- repo registration in `InProcessDaemon::new()`
- add/refresh discovery paths later in the file
- any host-discovery reporting methods that still inspect the old field

- [ ] **Step 5: Add regression tests for manager-backed local discovery**

In `crates/flotilla-core/tests/in_process_daemon.rs`, add tests that prove:

- a daemon still discovers local providers and repo identity correctly
- repo registration still works when driven through the manager-backed local direct environment
- the daemon does not depend on a standalone `host_bag` field anymore

Prefer injected discovery runtimes and existing test support helpers over end-to-end process setup.

- [ ] **Step 6: Run focused in-process daemon tests**

Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: in-process daemon tests pass with manager-backed local discovery

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-core/src/environment_manager.rs
git commit -m "refactor: route local daemon discovery through environment manager"
```

## Task 4: Build host summaries from the manager-backed local direct environment

**Files:**
- Modify: `crates/flotilla-core/src/host_summary.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/host_registry.rs`
- Modify: `crates/flotilla-protocol/src/host_summary.rs` only if needed for non-breaking defaults
- Test: `crates/flotilla-core/src/host_summary.rs`

- [ ] **Step 1: Change host-summary construction to accept manager-backed local environment data**

Refactor `build_local_host_summary(...)` so the environment inventory and tool inventory are derived from:

- the local direct environment bag held by the manager
- any provisioned environments currently registered in the manager

Do not change peer protocol shape or rename protocol fields in this tranche.

- [ ] **Step 2: Add a small summary-facing API on `EnvironmentManager`**

Expose exactly what host-summary code needs, for example:

```rust
pub fn local_direct_environment_bag(&self) -> Result<EnvironmentBag, String>;
pub fn environment_infos(&self) -> Vec<flotilla_protocol::EnvironmentInfo>;
```

Keep these summary APIs read-only and avoid exposing internal storage directly.

- [ ] **Step 3: Update `InProcessDaemon::new()` to build the initial local summary from the manager**

Where the daemon currently builds `local_host_summary` from `host_bag`, switch to manager-backed APIs so the host summary now reflects the same source of truth as execution and discovery.

- [ ] **Step 4: Add targeted host-summary tests**

Update or add tests that prove:

- the local direct environment inventory still populates summary inventory
- registered provisioned environments still appear in `HostSummary.environments`
- the summary builder does not require a free-standing `host_bag`

- [ ] **Step 5: Run focused tests**

Run: `cargo test -p flotilla-core host_summary -- --nocapture`
Expected: host-summary tests pass with manager-backed local environment data

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/host_summary.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/host_registry.rs
git commit -m "refactor: build local host summary from environment manager"
```

## Task 5: Clean up executor and daemon integration seams

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: any compile-error-driven call sites touched by the refactor

- [ ] **Step 1: Add any daemon-facing accessor needed for manager-backed execution**

If current call sites need explicit access to the local environment id or manager, add narrow accessors in `crates/flotilla-core/src/daemon.rs` and `InProcessDaemon`.

Do not widen the trait surface unless a real caller needs it.

- [ ] **Step 2: Update server startup wiring**

In `crates/flotilla-daemon/src/server.rs`, make sure daemon construction still cleanly initializes:

- local environment manager
- local host summary
- peer networking startup

The server should not gain ownership of environment lifecycle state; it should remain a transport/bootstrap layer.

- [ ] **Step 3: Remove dead executor-owned environment helpers**

Once the manager fully owns environment lifecycle, delete any resolver-local helper code and comments that assume the resolver owns handles or registries.

- [ ] **Step 4: Run `cargo check` on the workspace**

Run: `cargo check --workspace --locked`
Expected: full workspace compiles after the refactor

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-daemon/src/server.rs crates/flotilla-core/src/executor.rs
git commit -m "refactor: finalize environment manager integration for phase 1 and 2"
```

## Task 6: Verify the tranche end-to-end

**Files:**
- No intended code changes; only if verification exposes a bug

- [ ] **Step 1: Run the sandbox-safe workspace test command**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: all tests pass in the sandbox-safe configuration

- [ ] **Step 2: Run the pinned formatter check**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
```

Expected: no formatting diffs

- [ ] **Step 3: Run clippy**

Run:

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: no clippy warnings

- [ ] **Step 4: Review the runtime invariants before calling the tranche complete**

Manually verify:

- `ExecutorStepResolver` no longer owns environment lifecycle state
- `InProcessDaemon` no longer owns a singleton ambient `host_bag`
- ambient local discovery routes through the managed direct environment
- provisioned environments and the local direct environment now share one runtime owner

- [ ] **Step 5: Commit any final fixes**

```bash
git add -A
git commit -m "refactor: complete environment manager phase 1 and 2"
```

## Follow-On Roadmap After This Plan

Do not implement these from this plan without a follow-up planning pass.

### Phase 3

- Add static SSH direct environments managed by `EnvironmentManager`
- Reuse the same discovery and runner abstractions as the local direct environment

### Phase 4

- Move provider attribution and environment visibility more explicitly to environment scope
- Make the environment model more visible in summaries and correlation inputs

### Phase 5

- Complete `QualifiedPath` migration and wire real `HostId`
- Add mount translation rules where provisioned environments need structured path rewriting

### Phase 6

- Rekey mesh identity from `HostName` to `NodeId`
- Remove remaining execution-context uses of `HostName`
