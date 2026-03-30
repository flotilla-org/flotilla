# Environment Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the daemon's local machine and Docker containers explicit environments with `EnvironmentId`, wire up real `HostId` at startup, environment-qualify container checkouts, and support `suppress_local_environment`.

**Architecture:** The daemon resolves a real `HostId` (UUID from `host_identity.rs`) and an `EnvironmentId` for its local machine at startup. Discovery and providers become environment-scoped. Container checkouts use `QualifiedPath::environment()`. The `HostSummary` protocol type gains a host→environment hierarchy. `StepExecutionContext` evolves to carry `EnvironmentId`.

**Tech Stack:** Rust, flotilla-protocol (serde types), flotilla-core (daemon, discovery, executor), flotilla-tui (UI model), flotilla-daemon (server).

**Spec:** `docs/superpowers/specs/2026-03-28-environment-model-design.md`

---

### Task 1: Wire real HostId at daemon startup

The `host_identity.rs` module already implements `machine_scoped_state_dir()` and `resolve_or_create_host_id()` with atomic creation and machine-id scoping. Currently `InProcessDaemon` uses `HostName` as identity and `from_host_path()` maps `HostName` → `HostId` by stringifying the hostname. This task wires the real UUID-based `HostId` into the daemon.

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs` — add `host_id: HostId` field, resolve at startup
- Modify: `crates/flotilla-core/src/config.rs` — add `machine_id` field to `DaemonConfig`
- Modify: `crates/flotilla-core/src/daemon.rs` — add `host_id()` to `DaemonHandle` trait
- Modify: `crates/flotilla-core/src/host_identity.rs` — add `resolve_local_environment_id()` (same pattern as host-id but for `environment-id` file)
- Modify: `crates/flotilla-protocol/src/qualified_path.rs` — deprecate/remove `from_host_path()` once callers use real `HostId`
- Test: existing tests in `host_identity.rs`, plus new integration test

- [ ] **Step 1: Add `machine_id` to `DaemonConfig`**

In `crates/flotilla-core/src/config.rs`, add to `DaemonConfig`:

```rust
pub struct DaemonConfig {
    #[serde(default)]
    pub follower: bool,
    pub host_name: Option<String>,
    #[serde(default)]
    pub suppress_local_environment: bool,
    pub machine_id: Option<String>,
}
```

- [ ] **Step 2: Add `resolve_local_environment_id()` to `host_identity.rs`**

Same atomic create-or-read pattern as `resolve_or_create_host_id`, but for `environment-id` file:

```rust
pub fn resolve_or_create_environment_id(state_dir: &Path) -> Result<flotilla_protocol::EnvironmentId, String> {
    let target = state_dir.join("environment-id");

    if let Ok(content) = fs::read_to_string(&target) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Ok(flotilla_protocol::EnvironmentId::new(trimmed));
        }
    }

    let new_id = Uuid::new_v4().to_string();
    let temp = state_dir.join(format!(".environment-id.{}", std::process::id()));

    fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir {}: {e}", state_dir.display()))?;
    fs::write(&temp, format!("{new_id}\n")).map_err(|e| format!("failed to write temp environment-id: {e}"))?;

    match fs::hard_link(&temp, &target) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            Ok(flotilla_protocol::EnvironmentId::new(new_id))
        }
        Err(_) => {
            let _ = fs::remove_file(&temp);
            let content = fs::read_to_string(&target)
                .map_err(|e| format!("failed to read environment-id after link race: {e}"))?;
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Err("environment-id file exists but is empty".to_owned());
            }
            Ok(flotilla_protocol::EnvironmentId::new(trimmed))
        }
    }
}
```

- [ ] **Step 3: Write test for environment-id resolution**

```rust
#[test]
fn generates_and_persists_environment_id() {
    let dir = tempfile::tempdir().unwrap();
    let id1 = resolve_or_create_environment_id(dir.path()).unwrap();
    let id2 = resolve_or_create_environment_id(dir.path()).unwrap();
    assert_eq!(id1, id2, "should return same ID on second call");
    assert!(!id1.as_str().is_empty());
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core host_identity -- --nocapture`
Expected: all pass

- [ ] **Step 5: Add `host_id()` and `local_environment_id()` to `DaemonHandle` trait**

In `crates/flotilla-core/src/daemon.rs`:

```rust
fn host_id(&self) -> &flotilla_protocol::qualified_path::HostId;
fn local_environment_id(&self) -> Option<&flotilla_protocol::EnvironmentId>;
```

- [ ] **Step 6: Wire `HostId` and `EnvironmentId` into `InProcessDaemon`**

In `crates/flotilla-core/src/in_process.rs`:

Add fields:
```rust
host_id: HostId,
local_environment_id: Option<EnvironmentId>,
```

In the constructor, after resolving the state dir:
```rust
let daemon_config = config.load_daemon()?;
let state_dir = machine_scoped_state_dir(
    config.state_dir().as_path(),
    daemon_config.machine_id.as_deref(),
    discovery.runner.as_ref(),
).await?;
let host_id = resolve_or_create_host_id(&state_dir)?;
let local_environment_id = if daemon_config.suppress_local_environment {
    None
} else {
    Some(resolve_or_create_environment_id(&state_dir)?)
};
```

Implement the trait methods to return these fields.

- [ ] **Step 7: Update callers that use `from_host_path()` to use the real `HostId`**

Thread `host_id` through refresh, executor, and provider construction so that `QualifiedPath::host(host_id.clone(), path)` is used instead of `QualifiedPath::from_host_path(&host_name, path)`. Key call sites:

- `crates/flotilla-core/src/refresh.rs` — receives `HostId` instead of `HostName` for checkout qualification
- `crates/flotilla-core/src/executor.rs` — `CheckoutFlow` and related structs receive `HostId`
- `crates/flotilla-core/src/providers/vcs/git_worktree.rs` — `GitCheckoutManager` stores `HostId`
- `crates/flotilla-core/src/providers/vcs/wt.rs` — `WtVcs` stores `HostId`
- `crates/flotilla-core/src/providers/vcs/clone.rs` — `CloneCheckoutManager` stores `HostId` (will change again in Task 2)

Each VCS provider/checkout manager currently receives `HostName` — change to receive `HostId`. The factories that construct these providers will need to receive `HostId` from the daemon.

- [ ] **Step 8: Update `from_host_path()` to panic with deprecation or remove it**

Once all callers use the real `HostId`, remove `from_host_path()` from `QualifiedPath`. If any test-support code still needs it, move it behind `#[cfg(test)]` or `test-support` feature.

- [ ] **Step 9: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: all pass

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat: wire real HostId and local EnvironmentId at daemon startup"
```

---

### Task 2: Environment-qualify container checkouts

Container checkouts created via `CloneCheckoutManager` (inside Docker environments) currently use `QualifiedPath::from_host_path()`. Per the spec's normalize-to-most-persistent rule: checkouts on container-local storage (no host-side bind mount) should use `QualifiedPath::environment(env_id, path)`. The reference repo `/ref/repo` is a bind mount but the clone at `/workspace/<branch>` is container-local.

**Files:**
- Modify: `crates/flotilla-core/src/providers/vcs/clone.rs` — use `QualifiedPath::environment()` instead of `from_host_path()`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/clone.rs` — pass `EnvironmentId` to factory
- Test: `crates/flotilla-core/src/providers/vcs/clone.rs` existing tests

- [ ] **Step 1: Pass `EnvironmentId` into `CloneCheckoutManager`**

The `CloneCheckoutManagerFactory` creates `CloneCheckoutManager`. It activates when `FLOTILLA_ENVIRONMENT_ID` env var is present. Pass that ID through:

In `clone.rs`, change `CloneCheckoutManager`:
```rust
pub struct CloneCheckoutManager {
    environment_id: EnvironmentId,  // was: host_name: HostName
    reference_repo: ExecutionEnvironmentPath,
    runner: Arc<dyn CommandRunner>,
    env: Arc<dyn EnvVars>,
}
```

In checkout creation, use:
```rust
let qp = QualifiedPath::environment(self.environment_id.clone(), PathBuf::from(&checkout_dir));
```

- [ ] **Step 2: Update `CloneCheckoutManagerFactory` to pass `EnvironmentId`**

In `factories/clone.rs`, extract `EnvironmentId` from the bag's `FLOTILLA_ENVIRONMENT_ID` env var:

```rust
let env_id_str = bag.find_env_var("FLOTILLA_ENVIRONMENT_ID")?;
let env_id = EnvironmentId::new(env_id_str);
```

Pass `env_id` to `CloneCheckoutManager::new()`.

- [ ] **Step 3: Update `CloneCheckoutManager` scan/list methods**

The `scan_checkouts()` and `list_checkout_branches()` methods also construct `QualifiedPath` — update these to use `QualifiedPath::environment()` with the stored `environment_id`.

- [ ] **Step 4: Update tests**

Update any tests that assert on `CloneCheckoutManager` output to expect `PathQualifier::Environment` instead of `PathQualifier::Host`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-core clone -- --nocapture`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: environment-qualify container checkouts in CloneCheckoutManager"
```

---

### Task 3: Per-environment discovery in DiscoveryRuntime

Currently `DiscoveryRuntime` holds one runner/env pair and `InProcessDaemon` holds one `host_bag: EnvironmentBag`. The spec wants discovery to run per-environment. This task makes `DiscoveryRuntime` able to run host detection for multiple environments, each with their own `CommandRunner` and `EnvVars`.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` — add `run_host_detectors_for()` method that takes a runner + env + environment_id
- Modify: `crates/flotilla-core/src/in_process.rs` — store `environment_bags: HashMap<EnvironmentId, EnvironmentBag>` instead of single `host_bag`
- Test: in-process daemon tests

- [ ] **Step 1: Add `run_host_detectors_for()` to `DiscoveryRuntime`**

```rust
pub async fn run_host_detectors_for(
    &self,
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
) -> EnvironmentBag {
    let mut bag = EnvironmentBag::new();
    for detector in &self.host_detectors {
        let assertions = detector.detect(runner, env).await;
        for assertion in assertions {
            bag = bag.with(assertion);
        }
    }
    bag
}
```

This is the same as the existing `run_host_detectors()` but accepts arbitrary runner/env rather than using `self.runner`/`self.env`. The existing method can delegate to this one.

- [ ] **Step 2: Replace `host_bag` with `environment_bags` in `InProcessDaemon`**

```rust
environment_bags: RwLock<HashMap<EnvironmentId, EnvironmentBag>>,
```

At startup, run host detection for the local environment (if not suppressed) and store it keyed by `local_environment_id`.

- [ ] **Step 3: Update `discover_providers()` calls**

Anywhere the daemon passes `host_bag` to `discover_providers()`, look up the correct bag for the environment being discovered. For local repos this is the local environment's bag. For Docker environment discovery (the `DiscoverEnvironmentProviders` step action), the bag is built inline from the container's env vars — this already works and doesn't need the stored bags.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: per-environment discovery bags in DiscoveryRuntime"
```

---

### Task 4: Wire suppress_local_environment

The `suppress_local_environment` flag is already in `DaemonConfig` but not wired up. When true, the daemon should skip local environment detection and provider construction. It can still manage repos (for peer data relay), but won't discover local tools or construct local providers.

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs` — skip host detection and local provider discovery when suppressed
- Modify: `crates/flotilla-core/src/refresh.rs` — skip provider refresh when no local environment
- Test: new unit test

- [ ] **Step 1: Skip host detection when suppressed**

In `InProcessDaemon::new()`, the `local_environment_id` is already `None` when suppressed (from Task 1). Guard host detection:

```rust
let environment_bags = if let Some(env_id) = &local_environment_id {
    let bag = discovery.run_host_detectors_for(discovery.runner.as_ref(), discovery.env.as_ref()).await;
    let mut map = HashMap::new();
    map.insert(env_id.clone(), bag);
    map
} else {
    HashMap::new()
};
```

- [ ] **Step 2: Guard provider discovery in refresh**

In `refresh.rs`, when building providers for a repo, check that a local environment exists. If suppressed, skip local provider construction (the repo can still receive peer data).

- [ ] **Step 3: Write test for suppressed environment**

Create an `InProcessDaemon` with `suppress_local_environment = true` in config. Assert that no local providers are constructed and no host bag exists, but the daemon still starts and can track repos.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: wire suppress_local_environment to skip local discovery"
```

---

### Task 5: HostSummary → host/environment hierarchy

The protocol's `HostSummary` currently has flat `SystemInfo` + `ToolInventory`. The spec wants a `HostInfo` with `direct_environment: Option<EnvironmentSummary>` + `provisioned_environments: Vec<EnvironmentSummary>`.

**Files:**
- Modify: `crates/flotilla-protocol/src/host_summary.rs` — add `EnvironmentSummary`, `HostInfo`, restructure `HostSummary`
- Modify: `crates/flotilla-core/src/host_registry.rs` — build summaries from per-environment data
- Modify: `crates/flotilla-tui/src/` — update TUI to read from new structure
- Test: snapshot tests will need updating (intentionally — new structure)

- [ ] **Step 1: Add `EnvironmentSummary` and `EnvironmentKind` to protocol**

In `crates/flotilla-protocol/src/host_summary.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentKind {
    Direct,
    Docker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSummary {
    pub environment_id: crate::EnvironmentId,
    pub display_name: String,
    pub kind: EnvironmentKind,
    pub system: SystemInfo,
    pub inventory: ToolInventory,
    pub providers: Vec<HostProviderStatus>,
}
```

- [ ] **Step 2: Add `HostInfo` and restructure `HostSummary`**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: crate::qualified_path::HostId,
    pub display_name: String,
    pub direct_environment: Option<EnvironmentSummary>,
    pub provisioned_environments: Vec<EnvironmentSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub host_name: crate::HostName,  // daemon mesh identity (becomes NodeId later)
    pub display_name: String,
    pub hosts: Vec<HostInfo>,
}
```

Keep the old fields behind `#[serde(default)]` temporarily for backward compat during transition, or just change them outright (no backwards compat phase).

- [ ] **Step 3: Update `HostSummary` construction in `host_registry.rs`**

Build `HostInfo` from the daemon's `host_id`, `local_environment_id`, and per-environment bags. The `EnvironmentSummary` for the local direct environment uses the local bag's assertions. Provisioned environments use data from the environment provisioning system.

- [ ] **Step 4: Update TUI to read from new structure**

The TUI reads `HostSummary` for the host panel, path shortening, and work item display. Update it to traverse the `hosts[].direct_environment` path for system info and inventory. This is mostly mechanical — the data is the same, just nested differently.

- [ ] **Step 5: Update snapshot tests**

Snapshot tests that render host info will fail with the new structure. Review each failure — the changes should reflect the new hierarchy. Accept snapshots that correctly show the new structure.

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass (after snapshot updates)

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: HostSummary host/environment hierarchy with EnvironmentSummary"
```

---

### Task 6: StepExecutionContext evolution

`StepExecutionContext` currently has `Host(HostName)` and `Environment(HostName, EnvironmentId)`. The spec wants this to evolve so that the execution target is always an `EnvironmentId`. This task updates the enum and its consumers.

**Files:**
- Modify: `crates/flotilla-protocol/src/step.rs` — evolve `StepExecutionContext`
- Modify: `crates/flotilla-core/src/executor.rs` — update step dispatch
- Modify: `crates/flotilla-core/src/step.rs` — update plan building
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs` — update remote dispatch

- [ ] **Step 1: Evolve `StepExecutionContext`**

```rust
pub enum StepExecutionContext {
    /// Run in the daemon's direct environment on a specific host.
    /// `host_name` is for routing (which daemon); `environment_id` is the execution target.
    DirectEnvironment {
        host_name: HostName,
        environment_id: EnvironmentId,
    },
    /// Run in a provisioned environment on a specific host.
    ProvisionedEnvironment {
        host_name: HostName,
        environment_id: EnvironmentId,
    },
}
```

Both variants carry `EnvironmentId`. The `host_name` is retained for routing (which daemon to send the step to) — this becomes `NodeId` in the node identity spec.

Provide `host_name()` and `environment_id()` accessor methods.

- [ ] **Step 2: Update plan building**

In `crates/flotilla-core/src/step.rs` and `crates/flotilla-core/src/executor.rs`, where `StepExecutionContext::Host(host)` is constructed, use `DirectEnvironment { host_name, environment_id }` with the daemon's local environment ID.

Where `StepExecutionContext::Environment(host, env_id)` is constructed, use `ProvisionedEnvironment { host_name, environment_id }`.

- [ ] **Step 3: Update step dispatch**

The executor dispatches steps based on execution context. Update the match arms from `Host(h)` / `Environment(h, e)` to the new variants.

- [ ] **Step 4: Update remote dispatch**

In `crates/flotilla-daemon/src/server/remote_commands.rs`, the routing logic extracts `host_name()` from the context. This still works via the accessor method.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: StepExecutionContext carries EnvironmentId in all variants"
```

---

### Task 7: Merge validation for environment-qualified paths

The merge logic in `merge.rs` currently only handles `PathQualifier::Host`. With environment-qualified container checkouts (Task 2), merge needs to handle `PathQualifier::Environment` too.

**Files:**
- Modify: `crates/flotilla-core/src/merge.rs` — handle `Environment` qualifier in checkout merge
- Test: merge tests

- [ ] **Step 1: Update checkout merge for environment-qualified paths**

In `merge_provider_data()`, the checkout loop currently does:
```rust
if qp.host_id().map(|h| h.as_str()) == Some(local_host.as_str()) {
    continue;  // local-owned
}
if qp.host_id().map(|h| h.as_str()) != Some(peer_host.as_str()) {
    continue;  // not owned by this peer
}
```

Add handling for environment-qualified paths. Environment checkouts belong to whoever provisioned the environment. Since environments are currently always local to the daemon that created them, environment-qualified checkouts from a peer should be accepted if the peer is the one reporting them:

```rust
match &qp.qualifier {
    PathQualifier::Host(id) => {
        if id.as_str() == local_host.as_str() {
            continue;  // local-owned
        }
        if id.as_str() != peer_host.as_str() {
            continue;  // not owned by this peer
        }
    }
    PathQualifier::Environment(_env_id) => {
        // Environment checkouts are accepted from the peer that reports them.
        // The peer manages the environment that contains this checkout.
        // No host-id check needed — environment ownership is implicit
        // in who sends the data.
    }
}
```

- [ ] **Step 2: Write test for environment-qualified merge**

```rust
#[test]
fn merge_accepts_peer_environment_checkouts() {
    let local = ProviderData::default();
    let mut peer_data = ProviderData::default();
    let env_qp = QualifiedPath::environment(
        EnvironmentId::new("env-123"),
        PathBuf::from("/workspace/feat"),
    );
    peer_data.checkouts.insert(env_qp.clone(), test_checkout("feat"));

    let merged = merge_provider_data(&local, &HostName::new("local"), &[
        (HostName::new("peer"), &peer_data),
    ]);

    assert!(merged.checkouts.contains_key(&env_qp));
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core merge -- --nocapture`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: merge validation handles environment-qualified checkout paths"
```

---

### Task 8: Provider attribution per environment

Providers are currently attributed to the daemon as a whole. With multiple environments, each provider instance should be scoped to an `EnvironmentId`. This ensures that a `git` provider on the NAS environment is distinct from `git` on the local environment.

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs` — `ProviderRegistry` gains `environment_id` field
- Modify: `crates/flotilla-core/src/refresh.rs` — pass `EnvironmentId` when constructing providers
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — `ProviderData` optionally carries `environment_id`

- [ ] **Step 1: Add `environment_id` to `ProviderRegistry`**

```rust
pub struct ProviderRegistry {
    pub environment_id: Option<EnvironmentId>,
    // ... existing fields
}
```

Set this when constructing the registry during discovery. For the local environment, use the daemon's `local_environment_id`. For Docker environments, use the container's `EnvironmentId`.

- [ ] **Step 2: Thread `environment_id` through refresh**

When `refresh.rs` calls `discover_providers()`, pass the environment ID so the resulting `ProviderRegistry` carries it. When providers produce `ProviderData`, the environment attribution is available for correlation and display.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: provider attribution scoped to EnvironmentId"
```

---

### Task 9: Integration verification

End-to-end verification that the environment model works correctly.

**Files:**
- Test: `crates/flotilla-core/tests/in_process_daemon.rs` — add integration test
- Modify: `crates/flotilla-protocol/src/test_support.rs` — update `qp()` helper if needed

- [ ] **Step 1: Write integration test for HostId stability**

Create an `InProcessDaemon`, verify it has a real UUID `HostId` (not a hostname string). Create a second daemon pointing at the same state dir, verify same `HostId`.

- [ ] **Step 2: Write integration test for suppress_local_environment**

Create a daemon with suppression enabled. Verify no local providers are discovered, no local environment ID exists, but the daemon starts and can track repos for peer data relay.

- [ ] **Step 3: Verify environment-qualified checkout in Docker flow**

This requires the Docker provisioning flow to work end-to-end. If a Docker-based integration test exists, verify that checkouts inside the container use `PathQualifier::Environment`. If not, write a unit test using `CloneCheckoutManager` directly with a mock `CommandRunner`.

- [ ] **Step 4: Run full CI suite**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all pass

- [ ] **Step 5: Commit any test additions**

```bash
git add -A
git commit -m "test: integration tests for environment model"
```
