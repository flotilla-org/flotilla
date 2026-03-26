# Environment Provisioning End-to-End Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire environment provisioning infrastructure into the step system, plan builder, checkout flow, hop chain, refresh pipeline, and sandbox socket lifecycle — producing a working end-to-end path from `Command { environment: Some(spec) }` through container creation, checkout, workspace attach, and environment hop chain.

**Architecture:** The `Command` gains an `environment` field. When present, the plan builder prepends environment lifecycle steps (ensure image → create → discover providers) before checkout/workspace steps. The step resolver manages environment handles, sockets, and per-environment provider registries. The hop chain inserts `EnterEnvironment` hops for container access. `StepHost` is renamed to `StepExecutionContext` with variants `Host(HostName)` and `Environment(HostName, EnvironmentId)` — the `HostName` determines transport routing, the variant determines provider context.

**Tech Stack:** Rust, tokio, async-trait, serde, Docker CLI, flotilla crate ecosystem

**Spec:** `docs/superpowers/specs/2026-03-25-environment-provisioning-e2e-design.md`

---

### Task 1: Data Model Corrections — Remove Stale Phase C Fields

Remove misplaced Phase C fields: `environment_id` from `CloudAgentSession`, `environment_binding` from `RepoSnapshot`, and the `EnvironmentBinding` type.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs:224-241` (CloudAgentSession)
- Modify: `crates/flotilla-protocol/src/snapshot.rs:59-78` (RepoSnapshot)
- Modify: `crates/flotilla-protocol/src/environment.rs:81-86` (EnvironmentBinding)
- Modify: `crates/flotilla-protocol/src/lib.rs:19` (re-export)
- Modify: `crates/flotilla-protocol/src/lib/tests.rs:135`
- Modify: `crates/flotilla-protocol/src/snapshot.rs:222,279`
- Modify: `crates/flotilla-client/src/lib/tests.rs:36`
- Modify: `crates/flotilla-core/src/in_process/tests.rs:95,124,198`
- Modify: `crates/flotilla-core/src/convert.rs:215`
- Modify: `crates/flotilla-tui/src/app/test_support.rs:106`
- Modify: `crates/flotilla-tui/src/cli/tests.rs:217`

- [ ] **Step 1: Remove `EnvironmentBinding` type from protocol**

In `crates/flotilla-protocol/src/environment.rs`, delete the `EnvironmentBinding` struct (lines 81-86):

```rust
// DELETE this entire block:
/// Associates a sandbox environment with the host it runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentBinding {
    pub environment_id: EnvironmentId,
    pub host: HostName,
}
```

In `crates/flotilla-protocol/src/lib.rs`, remove `EnvironmentBinding` from the re-export:

```rust
// Change:
pub use environment::{EnvironmentBinding, EnvironmentId, EnvironmentInfo, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
// To:
pub use environment::{EnvironmentId, EnvironmentInfo, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
```

- [ ] **Step 2: Remove `environment_binding` from `RepoSnapshot`**

In `crates/flotilla-protocol/src/snapshot.rs`, remove the field from `RepoSnapshot`:

```rust
// DELETE these two lines from the struct:
    #[serde(default)]
    pub environment_binding: Option<EnvironmentBinding>,
```

Also remove the `EnvironmentBinding` import if it becomes unused (check the `use` block at the top of the file).

- [ ] **Step 3: Remove `environment_id` from `CloudAgentSession`**

In `crates/flotilla-protocol/src/provider_data.rs`, remove from `CloudAgentSession`:

```rust
// DELETE these two lines from the struct:
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
```

Remove the `EnvironmentId` import if it becomes unused.

- [ ] **Step 4: Fix all compilation errors from removals**

Remove `environment_binding: None,` from every site that constructs a `RepoSnapshot`:
- `crates/flotilla-protocol/src/lib/tests.rs` (~line 135)
- `crates/flotilla-protocol/src/snapshot.rs` (~lines 222, 279)
- `crates/flotilla-client/src/lib/tests.rs` (~line 36)
- `crates/flotilla-core/src/in_process/tests.rs` (~lines 95, 124, 198)
- `crates/flotilla-core/src/convert.rs` (~line 215)
- `crates/flotilla-tui/src/app/test_support.rs` (~line 106)
- `crates/flotilla-tui/src/cli/tests.rs` (~line 217)

Remove `environment_id: None,` from every site that constructs a `CloudAgentSession`. Search with `rg 'environment_id.*None' --type rust` and remove only the ones in `CloudAgentSession` constructors (not `Checkout` constructors — `Checkout.environment_id` stays).

- [ ] **Step 5: Verify compilation**

Run: `cargo build --workspace --locked`
Expected: Compiles cleanly.

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "refactor: remove stale Phase C fields (EnvironmentBinding, CloudAgentSession.environment_id, RepoSnapshot.environment_binding)"
```

---

### Task 2: EnvironmentProvider API — `create()` Takes `EnvironmentId`

Change `EnvironmentProvider::create()` to accept a pre-allocated `EnvironmentId` instead of generating one internally. Add `container_name()` to `ProvisionedEnvironment` for hop chain wiring.

**Files:**
- Modify: `crates/flotilla-core/src/providers/environment/mod.rs:30-34` (trait)
- Modify: `crates/flotilla-core/src/providers/environment/docker.rs:56-93` (DockerEnvironment::create, DockerProvisionedEnvironment)
- Modify: `crates/flotilla-core/src/providers/environment/tests.rs`

- [ ] **Step 1: Add `container_name()` to `ProvisionedEnvironment` trait**

In `crates/flotilla-core/src/providers/environment/mod.rs`, add to the trait:

```rust
#[async_trait]
pub trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;
    /// Provider-specific transport identifier (e.g. Docker container name).
    /// Used by hop chain to construct exec/enter commands.
    fn container_name(&self) -> Option<&str>;
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner>;
    async fn destroy(&self) -> Result<(), String>;
}
```

- [ ] **Step 2: Change `create()` signature to accept `EnvironmentId`**

In `crates/flotilla-core/src/providers/environment/mod.rs`, change the trait:

```rust
#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    async fn ensure_image(&self, spec: &EnvironmentSpec) -> Result<ImageId, String>;
    async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String>;
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
}
```

- [ ] **Step 3: Update `DockerEnvironment::create()` implementation**

In `crates/flotilla-core/src/providers/environment/docker.rs`, update the method:

```rust
async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String> {
    let container_name = format!("flotilla-env-{}", id);
    // ... rest unchanged except remove the line:
    //   let id = EnvironmentId::new(Uuid::new_v4().to_string());
```

Remove the `uuid::Uuid` import and the `uuid` dependency from `Cargo.toml` if it's no longer used anywhere in this crate.

- [ ] **Step 4: Implement `container_name()` on `DockerProvisionedEnvironment`**

In `crates/flotilla-core/src/providers/environment/docker.rs`, add to the impl:

```rust
fn container_name(&self) -> Option<&str> {
    Some(&self.container_name)
}
```

- [ ] **Step 5: Fix environment tests**

Update any tests in `crates/flotilla-core/src/providers/environment/tests.rs` that call `create()` to pass an `EnvironmentId`:

```rust
let id = EnvironmentId::new("test-env-1");
let handle = provider.create(id, &image_id, opts).await?;
```

- [ ] **Step 6: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "refactor: EnvironmentProvider::create() takes pre-allocated EnvironmentId, add container_name() to ProvisionedEnvironment"
```

---

### Task 3: Command Extension — `environment` Field

Add `environment: Option<EnvironmentSpec>` to `Command`.

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:58-67` (Command struct)

- [ ] **Step 1: Add the field**

In `crates/flotilla-protocol/src/commands.rs`, add to `Command`:

```rust
pub struct Command {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<crate::HostName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<crate::EnvironmentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_repo: Option<RepoSelector>,
    #[serde(flatten)]
    pub action: CommandAction,
}
```

- [ ] **Step 2: Fix all compilation errors**

Every site that constructs a `Command` needs `environment: None` added. Search with `rg 'Command \{' --type rust -l` and add the field. Key files:
- `crates/flotilla-protocol/src/commands.rs` (test constructors)
- `crates/flotilla-core/src/executor/tests.rs`
- `crates/flotilla-core/src/in_process.rs`
- `crates/flotilla-tui/src/app/intent.rs`
- `crates/flotilla-daemon/src/server/` (remote command handling)

- [ ] **Step 3: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: add environment field to Command for provisioning target"
```

---

### Task 4: StepHost → StepExecutionContext

Rename `StepHost` to `StepExecutionContext`. Remove `Local` variant. Add `Host(HostName)` and `Environment(HostName, EnvironmentId)` variants. Update the step runner to extract `HostName` for transport routing.

**Files:**
- Modify: `crates/flotilla-protocol/src/step.rs:15-20` (enum definition)
- Modify: `crates/flotilla-protocol/src/lib.rs:24` (re-export)
- Modify: `crates/flotilla-core/src/step.rs:7` (re-export)
- Modify: `crates/flotilla-core/src/step.rs:104-269` (run_step_plan_with_remote_executor)
- Modify: `crates/flotilla-core/src/executor.rs` (build_plan, helpers)
- Modify: `crates/flotilla-core/src/executor/tests.rs`
- Modify: `crates/flotilla-core/src/step/tests.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-daemon/src/server/remote_commands.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`
- Modify: `crates/flotilla-protocol/src/peer.rs`
- Modify: `crates/flotilla-protocol/src/lib/tests.rs`

- [ ] **Step 1: Define `StepExecutionContext` enum**

In `crates/flotilla-protocol/src/step.rs`, replace:

```rust
/// Which host a step should execute on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepHost {
    Local,
    Remote(HostName),
}
```

With:

```rust
/// Execution context for a step: which daemon (transport) and which provider scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepExecutionContext {
    /// Run on a host daemon using the host's own providers.
    Host(HostName),
    /// Run on a host daemon but resolve against an environment's providers.
    Environment(HostName, crate::EnvironmentId),
}

impl StepExecutionContext {
    /// The daemon host that will execute this step (determines transport routing).
    pub fn host_name(&self) -> &HostName {
        match self {
            Self::Host(h) | Self::Environment(h, _) => h,
        }
    }
}
```

- [ ] **Step 2: Update re-exports**

In `crates/flotilla-protocol/src/lib.rs`:
```rust
// Change:
pub use step::{CheckoutIntent, Step, StepAction, StepHost, StepOutcome};
// To:
pub use step::{CheckoutIntent, Step, StepAction, StepExecutionContext, StepOutcome};
```

In `crates/flotilla-core/src/step.rs`:
```rust
// Change:
pub use flotilla_protocol::{Step, StepAction, StepHost, StepOutcome};
// To:
pub use flotilla_protocol::{Step, StepAction, StepExecutionContext, StepOutcome};
```

- [ ] **Step 3: Update `Step` struct field type**

In `crates/flotilla-protocol/src/step.rs`:
```rust
pub struct Step {
    pub description: String,
    pub host: StepExecutionContext,
    pub action: StepAction,
}
```

- [ ] **Step 3b: Add `context` parameter to `StepResolver` trait**

The resolver needs the execution context to distinguish host vs environment steps. In `crates/flotilla-core/src/step.rs`:

```rust
#[async_trait::async_trait]
pub trait StepResolver: Send + Sync {
    async fn resolve(
        &self,
        description: &str,
        context: &StepExecutionContext,
        action: StepAction,
        prior: &[StepOutcome],
    ) -> Result<StepOutcome, String>;
}
```

Update `ExecutorStepResolver::resolve()` in `crates/flotilla-core/src/executor.rs` to accept the new parameter (ignore it for now — environment-polymorphic dispatch is added in Task 9). Update all test `StepResolver` implementations (in `step/tests.rs`, `executor/tests.rs`, `in_process.rs`).

- [ ] **Step 4: Update step runner transport routing**

In `crates/flotilla-core/src/step.rs`, update `run_step_plan_with_remote_executor`. Replace the `match step.host.clone()` block with host-name based routing:

```rust
while i < step_count {
    if cancel.is_cancelled() {
        return CommandValue::Cancelled;
    }

    let step = steps[i].clone();
    let step_target = step.host.host_name().clone();

    if step_target == local_host {
        // --- Local resolution (unchanged logic) ---
        emit_step_update(
            &event_tx, command_id, local_host.clone(), repo_identity.clone(),
            repo.as_path().to_path_buf(), i, step_count, step.description.clone(),
            StepStatus::Started,
        );

        let outcome = resolver.resolve(&step.description, &step.host, step.action, &outcomes).await;

        if cancel.is_cancelled() && outcome.is_ok() {
            return CommandValue::Cancelled;
        }

        match outcome {
            Ok(step_outcome) => {
                let status = match &step_outcome {
                    StepOutcome::Skipped => StepStatus::Skipped,
                    _ => StepStatus::Succeeded,
                };
                emit_step_update(
                    &event_tx, command_id, local_host.clone(), repo_identity.clone(),
                    repo.as_path().to_path_buf(), i, step_count, step.description.clone(), status,
                );
                outcomes.push(step_outcome);
            }
            Err(e) => {
                emit_step_update(
                    &event_tx, command_id, local_host.clone(), repo_identity.clone(),
                    repo.as_path().to_path_buf(), i, step_count, step.description.clone(),
                    StepStatus::Failed { message: e.clone() },
                );
                return prior_result_or_error(&outcomes, e);
            }
        }
        i += 1;
    } else {
        // --- Remote batching ---
        let target_host = step_target;
        let segment_start = i;
        let mut segment_steps = vec![step];
        i += 1;
        while i < step_count {
            if *steps[i].host.host_name() == target_host {
                segment_steps.push(steps[i].clone());
                i += 1;
            } else {
                break;
            }
        }

        // ... rest of remote batching logic unchanged, using target_host ...
    }
}
```

- [ ] **Step 5: Update `build_plan()` in executor.rs**

In `crates/flotilla-core/src/executor.rs`, change how `remote_host` is computed:

```rust
pub async fn build_plan(
    cmd: Command,
    // ... same params ...
    local_host: HostName,
) -> Result<StepPlan, CommandValue> {
    let Command { host, action, .. } = cmd;
    let target_host = host.unwrap_or_else(|| local_host.clone());
    let checkout_host = StepExecutionContext::Host(target_host.clone());

    // ... rest of match uses checkout_host instead of remote_host ...
}
```

Update `workspace_label_for_host`:

```rust
fn workspace_label_for_host(label: &str, host: &StepExecutionContext, local_host: &HostName) -> String {
    let target = host.host_name();
    if *target == *local_host {
        label.to_string()
    } else {
        format!("{label}@{target}")
    }
}
```

Update `build_create_checkout_plan` and `build_create_workspace_plan` to use `StepExecutionContext` parameter.

Update the `AttachWorkspace` step to always use `StepExecutionContext::Host(local_host)`:

```rust
Step { description: "Attach workspace".to_string(), host: StepExecutionContext::Host(local_host), action: StepAction::AttachWorkspace }
```

This requires `build_create_workspace_plan` to accept a `local_host: HostName` parameter for the AttachWorkspace step.

- [ ] **Step 6: Fix all remaining StepHost references**

Search with `rg 'StepHost' --type rust` and replace every occurrence:
- `StepHost::Local` → `StepExecutionContext::Host(local_host.clone())` (or the appropriate host)
- `StepHost::Remote(host)` → `StepExecutionContext::Host(host)`
- `StepHost` type references → `StepExecutionContext`

Key files: `executor/tests.rs`, `step/tests.rs`, `in_process.rs`, `server/remote_commands.rs`, `server/tests.rs`, `peer.rs`, `lib/tests.rs`.

- [ ] **Step 7: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "refactor: rename StepHost to StepExecutionContext with Host(HostName) and Environment(HostName, EnvironmentId) variants"
```

---

### Task 5: New StepAction Variants and CommandValue Extensions

Add environment lifecycle `StepAction` variants and corresponding `CommandValue` variants.

**Files:**
- Modify: `crates/flotilla-protocol/src/step.rs:32-119` (StepAction enum)
- Modify: `crates/flotilla-protocol/src/commands.rs` (CommandValue enum)
- Modify: `crates/flotilla-core/src/executor.rs` (match arm stubs)

- [ ] **Step 1: Add StepAction variants**

In `crates/flotilla-protocol/src/step.rs`, add to the `StepAction` enum after the `Noop` variant:

```rust
    // Environment lifecycle
    EnsureEnvironmentImage {
        spec: crate::EnvironmentSpec,
    },
    CreateEnvironment {
        env_id: crate::EnvironmentId,
        image: crate::ImageId,
    },
    DiscoverEnvironmentProviders {
        env_id: crate::EnvironmentId,
    },
    DestroyEnvironment {
        env_id: crate::EnvironmentId,
    },
```

- [ ] **Step 2: Add CommandValue variants**

In `crates/flotilla-protocol/src/commands.rs`, add to `CommandValue`:

```rust
    ImageEnsured {
        image: crate::ImageId,
    },
    EnvironmentCreated {
        env_id: crate::EnvironmentId,
    },
```

- [ ] **Step 3: Add stub match arms to ExecutorStepResolver**

In `crates/flotilla-core/src/executor.rs`, add to the `resolve()` match:

```rust
            StepAction::EnsureEnvironmentImage { .. }
            | StepAction::CreateEnvironment { .. }
            | StepAction::DiscoverEnvironmentProviders { .. }
            | StepAction::DestroyEnvironment { .. } => {
                Err("environment lifecycle steps not yet wired".to_string())
            }
```

- [ ] **Step 4: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add environment lifecycle StepAction variants and CommandValue extensions"
```

---

### Task 6: AttachableSet and PreparedWorkspace — `environment_id` Field

Add `environment_id: Option<EnvironmentId>` to `AttachableSet` (protocol) and `PreparedWorkspace`.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs:291-302` (AttachableSet)
- Modify: `crates/flotilla-protocol/src/commands.rs:44-56` (PreparedWorkspace)
- Fix: All sites constructing `AttachableSet` or `PreparedWorkspace`

- [ ] **Step 1: Add field to `AttachableSet`**

In `crates/flotilla-protocol/src/provider_data.rs`:

```rust
pub struct AttachableSet {
    pub id: AttachableSetId,
    #[serde(default)]
    pub host_affinity: Option<HostName>,
    #[serde(default)]
    pub checkout: Option<HostPath>,
    #[serde(default)]
    pub template_identity: Option<String>,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
    #[serde(default)]
    pub members: Vec<AttachableId>,
}
```

- [ ] **Step 2: Add field to `PreparedWorkspace`**

In `crates/flotilla-protocol/src/commands.rs`:

```rust
pub struct PreparedWorkspace {
    pub label: String,
    pub target_host: crate::HostName,
    pub checkout_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachable_set_id: Option<AttachableSetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<crate::EnvironmentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_yaml: Option<String>,
    pub prepared_commands: Vec<ResolvedPaneCommand>,
}
```

- [ ] **Step 3: Fix compilation errors**

Add `environment_id: None,` to all sites constructing `AttachableSet` or `PreparedWorkspace`. Search with `rg 'AttachableSet \{' --type rust` and `rg 'PreparedWorkspace' --type rust`.

Key files for `PreparedWorkspace`:
- `crates/flotilla-core/src/executor.rs` (PrepareWorkspace resolver ~line 527, CreateWorkspaceFromPreparedTerminal ~line 571)

Key files for `AttachableSet`:
- `crates/flotilla-core/src/attachable/store.rs`
- `crates/flotilla-core/src/executor/workspace.rs` (~line 206)

- [ ] **Step 4: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add environment_id to AttachableSet and PreparedWorkspace"
```

---

### Task 7: CloneCheckoutManager and Factory

New `CheckoutManager` for environments that uses `git clone --reference` inside containers.

**Files:**
- Create: `crates/flotilla-core/src/providers/vcs/clone.rs`
- Modify: `crates/flotilla-core/src/providers/vcs/mod.rs` (add module)
- Create: `crates/flotilla-core/src/providers/discovery/factories/clone.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/mod.rs` (add factory)

- [ ] **Step 1: Write the failing test for `CloneCheckoutManager`**

Create `crates/flotilla-core/src/providers/vcs/clone.rs`:

```rust
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::EnvironmentId;

use super::{Checkout, CheckoutManager};
use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{ChannelLabel, CommandRunner},
};

/// Checkout manager for sandbox environments.
///
/// Uses `git clone --reference` from a read-only reference repo mount,
/// producing full clones inside the container at `/workspace/<branch>`.
pub struct CloneCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    reference_dir: ExecutionEnvironmentPath,
}

impl CloneCheckoutManager {
    pub fn new(runner: Arc<dyn CommandRunner>, reference_dir: ExecutionEnvironmentPath) -> Self {
        Self { runner, reference_dir }
    }
}

#[async_trait]
impl CheckoutManager for CloneCheckoutManager {
    async fn list_checkouts(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        // List directories under /workspace/
        let output = self
            .runner
            .run("ls", &["-1"], Path::new("/workspace"), &ChannelLabel::Noop)
            .await
            .unwrap_or_default();

        let mut checkouts = Vec::new();
        for dir_name in output.lines() {
            let dir_name = dir_name.trim();
            if dir_name.is_empty() {
                continue;
            }
            let path = ExecutionEnvironmentPath::new(format!("/workspace/{dir_name}"));
            let git_dir = path.as_path().join(".git");
            if self.runner.run("test", &["-d", &git_dir.to_string_lossy()], Path::new("/"), &ChannelLabel::Noop).await.is_err() {
                continue;
            }
            let branch = self
                .runner
                .run("git", &["rev-parse", "--abbrev-ref", "HEAD"], path.as_path(), &ChannelLabel::Noop)
                .await
                .unwrap_or_else(|_| dir_name.to_string());
            checkouts.push((path, Checkout {
                branch: branch.trim().to_string(),
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
                environment_id: None,
            }));
        }
        Ok(checkouts)
    }

    async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        let clone_target = ExecutionEnvironmentPath::new(format!("/workspace/{branch}"));
        let ref_path = self.reference_dir.as_path().to_string_lossy().into_owned();

        // Get remote URL from reference
        let remote_url = self
            .runner
            .run("git", &["--git-dir", &ref_path, "remote", "get-url", "origin"], Path::new("/"), &ChannelLabel::Noop)
            .await?;
        let remote_url = remote_url.trim();

        let target_str = clone_target.as_path().to_string_lossy().into_owned();

        if create_branch {
            // Fresh branch: clone with --no-checkout, then create branch
            self.runner
                .run(
                    "git",
                    &["clone", "--reference", &ref_path, "--no-checkout", remote_url, &target_str],
                    Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
            self.runner
                .run("git", &["checkout", "-b", branch], clone_target.as_path(), &ChannelLabel::Noop)
                .await?;
        } else {
            // Existing branch: clone and checkout
            self.runner
                .run(
                    "git",
                    &["clone", "--reference", &ref_path, "-b", branch, remote_url, &target_str],
                    Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
        }

        Ok((clone_target, Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            environment_id: None,
        }))
    }

    async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        let target = format!("/workspace/{branch}");
        self.runner.run("rm", &["-rf", &target], Path::new("/"), &ChannelLabel::Noop).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        responses: Mutex<std::collections::VecDeque<Result<String, String>>>,
    }

    impl RecordingRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("lock").push((cmd.to_string(), args.iter().map(|s| s.to_string()).collect()));
            self.responses.lock().expect("lock").pop_front().unwrap_or(Ok(String::new()))
        }

        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
        ) -> Result<crate::providers::CommandOutput, String> {
            let stdout = self.run(cmd, args, cwd, label).await?;
            Ok(crate::providers::CommandOutput { stdout, stderr: String::new(), success: true })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn create_checkout_existing_branch_clones_with_reference() {
        let runner = Arc::new(RecordingRunner::new(vec![
            Ok("https://github.com/org/repo.git\n".to_string()), // git remote get-url
            Ok(String::new()),                                      // git clone
        ]));
        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));

        let (path, checkout) = mgr
            .create_checkout(&ExecutionEnvironmentPath::new("/workspace"), "feature-x", false)
            .await
            .expect("create_checkout");

        assert_eq!(path.as_path().to_str().unwrap(), "/workspace/feature-x");
        assert_eq!(checkout.branch, "feature-x");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "git");
        assert!(calls[0].1.contains(&"get-url".to_string()));
        assert_eq!(calls[1].0, "git");
        assert!(calls[1].1.contains(&"clone".to_string()));
        assert!(calls[1].1.contains(&"--reference".to_string()));
        assert!(calls[1].1.contains(&"-b".to_string()));
    }

    #[tokio::test]
    async fn create_checkout_fresh_branch_uses_no_checkout() {
        let runner = Arc::new(RecordingRunner::new(vec![
            Ok("https://github.com/org/repo.git\n".to_string()), // git remote get-url
            Ok(String::new()),                                      // git clone --no-checkout
            Ok(String::new()),                                      // git checkout -b
        ]));
        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));

        let (path, checkout) = mgr
            .create_checkout(&ExecutionEnvironmentPath::new("/workspace"), "new-branch", true)
            .await
            .expect("create_checkout");

        assert_eq!(path.as_path().to_str().unwrap(), "/workspace/new-branch");
        assert_eq!(checkout.branch, "new-branch");

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].1.contains(&"--no-checkout".to_string()));
        assert_eq!(calls[2].1, vec!["checkout", "-b", "new-branch"]);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/flotilla-core/src/providers/vcs/mod.rs`, add:

```rust
pub mod clone;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p flotilla-core clone::tests -v`
Expected: Both tests pass.

- [ ] **Step 4: Create `CloneCheckoutManagerFactory`**

Create `crates/flotilla-core/src/providers/discovery/factories/clone.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;

use super::super::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement};
use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{vcs::clone::CloneCheckoutManager, CommandRunner},
};

/// Factory that produces `CloneCheckoutManager` inside container environments.
///
/// Probes for:
/// - `FLOTILLA_ENVIRONMENT_ID` env var (we're inside a managed container)
/// - `/ref/repo` exists and is a valid git directory (reference mount is available)
pub struct CloneCheckoutManagerFactory;

#[async_trait]
impl Factory for CloneCheckoutManagerFactory {
    type Output = dyn crate::providers::vcs::CheckoutManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::CheckoutManager, "clone")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        // Must be inside a flotilla-managed environment
        if env.find_env_var("FLOTILLA_ENVIRONMENT_ID").is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())]);
        }

        // Reference repo must be mounted
        let ref_dir = ExecutionEnvironmentPath::new("/ref/repo");
        if !runner.exists("git", &["--git-dir", "/ref/repo", "rev-parse"]).await {
            return Err(vec![UnmetRequirement::Other("reference repo not mounted at /ref/repo".into())]);
        }

        Ok(Arc::new(CloneCheckoutManager::new(runner, ref_dir)))
    }
}
```

- [ ] **Step 5: Register factory**

In `crates/flotilla-core/src/providers/discovery/factories/mod.rs`, add `pub mod clone;` and add the factory to `checkout_manager_factories()`:

```rust
fn checkout_manager_factories() -> Vec<Box<super::CheckoutManagerFactory>> {
    vec![
        Box::new(clone::CloneCheckoutManagerFactory),  // highest priority inside environments
        Box::new(git::WtCheckoutManagerFactory),
        Box::new(git::GitCheckoutManagerFactory),
    ]
}
```

- [ ] **Step 6: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: add CloneCheckoutManager for container environments with git clone --reference"
```

---

### Task 8: Plan Builder — Environment Lifecycle Steps

When `cmd.environment` is `Some`, prepend environment lifecycle steps to the checkout plan.

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:87-264`

- [ ] **Step 1: Write failing test for environment checkout plan**

In `crates/flotilla-core/src/executor/tests.rs`, add:

```rust
#[test]
fn build_plan_with_environment_prepends_lifecycle_steps() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let local_host = HostName::new("laptop");
        let target_host = HostName::new("feta");
        let spec = flotilla_protocol::EnvironmentSpec {
            image: flotilla_protocol::ImageSource::Registry("flotilla-dev-env:latest".to_string()),
            token_requirements: vec!["github".to_string()],
        };

        let cmd = Command {
            host: Some(target_host.clone()),
            environment: Some(spec.clone()),
            context_repo: Some(RepoSelector::Identity(test_repo_identity())),
            action: CommandAction::Checkout {
                target: CheckoutTarget::FreshBranch("feature-x".to_string()),
                issue_ids: vec![],
            },
        };

        let (registry, _mocks) = build_test_registry();
        let store = test_attachable_store();
        let plan = build_plan(
            cmd,
            test_repo_execution_context(),
            Arc::new(registry),
            Arc::new(ProviderData::default()),
            DaemonHostPath::new("/tmp/config"),
            store,
            None,
            local_host.clone(),
        )
        .await
        .expect("build_plan");

        let step_summaries: Vec<(&str, &str)> = plan
            .steps
            .iter()
            .map(|s| {
                let host_label = match &s.host {
                    StepExecutionContext::Host(h) => format!("Host({})", h),
                    StepExecutionContext::Environment(h, env_id) => format!("Env({}, {})", h, env_id),
                };
                let action_label = match &s.action {
                    StepAction::EnsureEnvironmentImage { .. } => "EnsureEnvironmentImage",
                    StepAction::CreateEnvironment { .. } => "CreateEnvironment",
                    StepAction::DiscoverEnvironmentProviders { .. } => "DiscoverEnvironmentProviders",
                    StepAction::CreateCheckout { .. } => "CreateCheckout",
                    StepAction::PrepareWorkspace { .. } => "PrepareWorkspace",
                    StepAction::AttachWorkspace => "AttachWorkspace",
                    other => panic!("unexpected action: {other:?}"),
                };
                // Leak for test assertion convenience
                (Box::leak(host_label.into_boxed_str()) as &str, action_label)
            })
            .collect();

        assert_eq!(step_summaries.len(), 6);

        // Steps 1-3: environment lifecycle on target host
        assert_eq!(step_summaries[0], (format!("Host({})", target_host).leak() as &str, "EnsureEnvironmentImage"));
        assert_eq!(step_summaries[1].1, "CreateEnvironment");
        assert_eq!(step_summaries[2].1, "DiscoverEnvironmentProviders");

        // Steps 4-5: checkout and workspace inside environment
        assert!(step_summaries[3].0.starts_with("Env("));
        assert_eq!(step_summaries[3].1, "CreateCheckout");
        assert!(step_summaries[4].0.starts_with("Env("));
        assert_eq!(step_summaries[4].1, "PrepareWorkspace");

        // Step 6: attach workspace on local host
        assert_eq!(step_summaries[5], (format!("Host({})", local_host).leak() as &str, "AttachWorkspace"));
    });
}
```

Note: You'll need to add test helper functions `test_repo_identity()`, `test_repo_execution_context()`, `build_test_registry()`, and `test_attachable_store()` if they don't already exist — extract from existing test patterns in the same file.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core executor::tests::build_plan_with_environment_prepends_lifecycle_steps -v`
Expected: FAIL — the plan currently has no environment lifecycle steps.

- [ ] **Step 3: Implement environment lifecycle step generation**

In `crates/flotilla-core/src/executor.rs`, modify `build_plan()`. After computing `target_host` and `checkout_host`, add environment handling:

```rust
pub async fn build_plan(
    cmd: Command,
    repo: RepoExecutionContext,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: DaemonHostPath,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<DaemonHostPath>,
    local_host: HostName,
) -> Result<StepPlan, CommandValue> {
    let Command { host, environment, action, .. } = cmd;
    let target_host = host.unwrap_or_else(|| local_host.clone());

    // When an environment is specified and the command involves checkout, prepend lifecycle steps.
    if let Some(spec) = environment {
        if let CommandAction::Checkout { target, issue_ids, .. } = action {
            return Ok(build_environment_checkout_plan(spec, target, issue_ids, target_host, local_host));
        }
    }

    let checkout_host = StepExecutionContext::Host(target_host.clone());

    match action {
        // ... existing match arms unchanged ...
    }
}

fn build_environment_checkout_plan(
    spec: flotilla_protocol::EnvironmentSpec,
    target: CheckoutTarget,
    issue_ids: Vec<(String, String)>,
    target_host: HostName,
    local_host: HostName,
) -> StepPlan {
    let (branch, create_branch, intent) = match target {
        CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
        CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
    };

    let env_id = flotilla_protocol::EnvironmentId::new(uuid::Uuid::new_v4().to_string());
    let host_context = StepExecutionContext::Host(target_host.clone());
    let env_context = StepExecutionContext::Environment(target_host.clone(), env_id.clone());

    let mut steps = vec![
        Step {
            description: "Ensure environment image".to_string(),
            host: host_context.clone(),
            action: StepAction::EnsureEnvironmentImage { spec },
        },
        Step {
            description: format!("Create environment {env_id}"),
            host: host_context.clone(),
            action: StepAction::CreateEnvironment { env_id: env_id.clone(), image: flotilla_protocol::ImageId::new("placeholder") },
        },
        Step {
            description: format!("Discover providers in environment {env_id}"),
            host: host_context,
            action: StepAction::DiscoverEnvironmentProviders { env_id: env_id.clone() },
        },
        Step {
            description: format!("Create checkout for branch {branch}"),
            host: env_context.clone(),
            action: StepAction::CreateCheckout { branch: branch.clone(), create_branch, intent, issue_ids },
        },
    ];

    let workspace_label = if target_host == local_host {
        branch.clone()
    } else {
        format!("{branch}@{target_host}")
    };

    steps.push(Step {
        description: format!("Prepare workspace for {workspace_label}"),
        host: env_context,
        action: StepAction::PrepareWorkspace { checkout_path: None, label: workspace_label },
    });

    steps.push(Step {
        description: "Attach workspace".to_string(),
        host: StepExecutionContext::Host(local_host),
        action: StepAction::AttachWorkspace,
    });

    StepPlan::new(steps)
}
```

Note: The `image` in `CreateEnvironment` is a placeholder — the resolver will use the `ImageId` from the prior `EnsureEnvironmentImage` outcome. The plan builder passes a placeholder because it doesn't have the `ImageId` at plan-build time. The resolver extracts the real `ImageId` from `prior` outcomes. Update the resolver (Task 9) to handle this.

Actually, a simpler design: `CreateEnvironment` doesn't carry the `image` — the resolver extracts it from `prior`. But the spec says the variant carries it. Let's keep the spec's design and have the plan builder set a placeholder that the resolver overrides from prior outcomes. Or better: use `Produced(CommandValue::ImageEnsured { image })` from step 1, and the resolver for step 2 reads `image` from prior outcomes, ignoring the one in the action. Document this clearly.

- [ ] **Step 4: Add `uuid` dependency to `flotilla-core` if not present**

Check `crates/flotilla-core/Cargo.toml` for `uuid`. If missing, add:

```toml
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p flotilla-core executor::tests::build_plan_with_environment_prepends_lifecycle_steps -v`
Expected: PASS (test verifies step sequence and host assignments).

- [ ] **Step 6: Run all tests**

Run: `cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: plan builder prepends environment lifecycle steps when cmd.environment is set"
```

---

### Task 9: Step Resolver — Environment Lifecycle Action Handlers

Add environment state to `ExecutorStepResolver` and implement resolution for the four environment lifecycle actions.

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:356-731` (ExecutorStepResolver)
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Write failing tests for environment action resolution**

In `crates/flotilla-core/src/executor/tests.rs`, add:

```rust
#[tokio::test]
async fn resolve_ensure_environment_image_calls_provider() {
    let spec = flotilla_protocol::EnvironmentSpec {
        image: flotilla_protocol::ImageSource::Registry("test-image:latest".to_string()),
        token_requirements: vec![],
    };

    let (mut registry, _mocks) = build_test_registry();
    let mock_env_provider = Arc::new(MockEnvironmentProvider::new(vec![
        Ok(flotilla_protocol::ImageId::new("sha256:abc123")),
    ]));
    registry.environment_providers = crate::providers::registry::ProviderSet::from_single(
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        mock_env_provider,
    );

    let resolver = make_resolver_with_registry(registry);
    let outcome = resolver
        .resolve("ensure image", StepAction::EnsureEnvironmentImage { spec }, &[])
        .await
        .expect("resolve");

    match outcome {
        StepOutcome::Produced(CommandValue::ImageEnsured { image }) => {
            assert_eq!(image.as_str(), "sha256:abc123");
        }
        other => panic!("expected Produced(ImageEnsured), got {other:?}"),
    }
}
```

You'll need to create `MockEnvironmentProvider` — a test double for `EnvironmentProvider`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core executor::tests::resolve_ensure_environment_image -v`
Expected: FAIL — the stub match arm returns an error.

- [ ] **Step 3: Add environment state to `ExecutorStepResolver`**

In `crates/flotilla-core/src/executor.rs`:

```rust
use std::collections::HashMap;
use crate::providers::environment::{EnvironmentHandle, EnvironmentProvider};
use crate::providers::registry::ProviderRegistry;

pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub providers_data: Arc<ProviderData>,
    pub runner: Arc<dyn CommandRunner>,
    pub config_base: DaemonHostPath,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<DaemonHostPath>,
    pub local_host: HostName,
    // Environment lifecycle state (populated during step execution)
    pub environment_handles: std::sync::Mutex<HashMap<flotilla_protocol::EnvironmentId, EnvironmentHandle>>,
    pub environment_registries: std::sync::Mutex<HashMap<flotilla_protocol::EnvironmentId, Arc<ProviderRegistry>>>,
}
```

Update all construction sites to initialize the new fields with `std::sync::Mutex::new(HashMap::new())`.

- [ ] **Step 3b: Add environment-polymorphic dispatch to `resolve()`**

The `resolve()` method now receives `context: &StepExecutionContext`. At the top of the method, determine the effective `registry`, `runner`, and `repo_root` based on context:

```rust
async fn resolve(
    &self,
    _description: &str,
    context: &StepExecutionContext,
    action: StepAction,
    prior: &[StepOutcome],
) -> Result<StepOutcome, String> {
    // Determine effective providers based on execution context
    let (effective_registry, effective_runner, effective_repo_root) = match context {
        StepExecutionContext::Host(_) => {
            (self.registry.clone(), self.runner.clone(), self.repo.root.clone())
        }
        StepExecutionContext::Environment(_, env_id) => {
            let registry = {
                let registries = self.environment_registries.lock().expect("lock");
                registries.get(env_id).cloned()
                    .ok_or_else(|| format!("environment registry not found: {env_id}"))?
            };
            let runner = {
                let handles = self.environment_handles.lock().expect("lock");
                let handle = handles.get(env_id)
                    .ok_or_else(|| format!("environment handle not found: {env_id}"))?;
                handle.runner(self.runner.clone())
            };
            // Interior repo_root comes from prior CreateCheckout outcome
            let repo_root = prior.iter().find_map(|o| match o {
                StepOutcome::CompletedWith(CommandValue::CheckoutCreated { path, .. }) => {
                    Some(ExecutionEnvironmentPath::new(path))
                }
                _ => None,
            }).unwrap_or_else(|| ExecutionEnvironmentPath::new("/workspace"));
            (registry, runner, repo_root)
        }
    };

    // Use effective_registry, effective_runner, effective_repo_root in action handlers
    // instead of self.registry, self.runner, self.repo.root
    match action {
        // ... existing handlers, updated to use effective_* ...
    }
}
```

This makes existing step actions (CreateCheckout, PrepareWorkspace) work inside environments without modification — they use the environment's `CloneCheckoutManager` and environment runner transparently.

- [ ] **Step 4: Implement `EnsureEnvironmentImage` resolution**

Replace the stub match arm:

```rust
StepAction::EnsureEnvironmentImage { spec } => {
    let env_provider = self
        .registry
        .environment_providers
        .preferred()
        .ok_or_else(|| "no environment provider available".to_string())?;
    let image = env_provider.ensure_image(&spec).await?;
    Ok(StepOutcome::Produced(CommandValue::ImageEnsured { image }))
}
```

- [ ] **Step 5: Implement `CreateEnvironment` resolution**

```rust
StepAction::CreateEnvironment { env_id, image: _ } => {
    // Extract actual ImageId from prior EnsureEnvironmentImage outcome
    let image = prior
        .iter()
        .find_map(|o| match o {
            StepOutcome::Produced(CommandValue::ImageEnsured { image }) => Some(image.clone()),
            _ => None,
        })
        .ok_or_else(|| "image not produced by prior EnsureEnvironmentImage step".to_string())?;

    let env_provider = self
        .registry
        .environment_providers
        .preferred()
        .ok_or_else(|| "no environment provider available".to_string())?;

    // Build CreateOpts — the resolver has access to EnvironmentSocketRegistry and reference repo
    let reference_repo = self.resolve_reference_repo().await;
    let daemon_socket = self
        .daemon_socket_path
        .clone()
        .ok_or_else(|| "daemon socket path required for environment creation".to_string())?;

    let opts = crate::providers::environment::CreateOpts {
        tokens: vec![], // Token resolution deferred to Phase E
        reference_repo,
        daemon_socket_path: daemon_socket,
        working_directory: None,
    };

    let handle = env_provider.create(env_id.clone(), &image, opts).await?;
    self.environment_handles.insert(env_id.clone(), handle);
    Ok(StepOutcome::Produced(CommandValue::EnvironmentCreated { env_id }))
}
```

Add the helper method:

```rust
impl ExecutorStepResolver {
    async fn resolve_reference_repo(&self) -> Option<DaemonHostPath> {
        // Resolve the .git common dir for the repo root
        let result = self
            .runner
            .run(
                "git",
                &["rev-parse", "--git-common-dir"],
                self.repo.root.as_path(),
                &crate::providers::ChannelLabel::Noop,
            )
            .await;
        match result {
            Ok(path) => Some(DaemonHostPath::new(path.trim())),
            Err(_) => None,
        }
    }
}
```

Note: `ExecutorStepResolver` needs interior mutability for `environment_handles` and `environment_registries` since `resolve()` takes `&self`. Use `tokio::sync::Mutex` or `std::sync::Mutex` for these fields:

```rust
pub environment_handles: std::sync::Mutex<HashMap<flotilla_protocol::EnvironmentId, EnvironmentHandle>>,
pub environment_registries: std::sync::Mutex<HashMap<flotilla_protocol::EnvironmentId, Arc<ProviderRegistry>>>,
```

- [ ] **Step 6: Implement `DiscoverEnvironmentProviders` resolution**

```rust
StepAction::DiscoverEnvironmentProviders { env_id } => {
    let handle = {
        let handles = self.environment_handles.lock().expect("environment_handles lock");
        handles.get(&env_id).cloned().ok_or_else(|| format!("environment handle not found: {env_id}"))?
    };

    // Get raw env vars from the container
    let raw_env_vars = handle.env_vars().await?;

    // Build EnvironmentBag from raw env vars + detection
    let env_runner = handle.runner(self.runner.clone());
    let env_bag = crate::providers::discovery::build_environment_bag_from_vars(&raw_env_vars, &env_runner).await;

    // Probe factories through the environment runner
    let config = crate::config::ConfigStore::empty();
    let env_repo_root = ExecutionEnvironmentPath::new("/workspace");
    let factory_registry = crate::providers::discovery::FactoryRegistry::default_all();
    let provider_registry = factory_registry.probe_all(&env_bag, &config, &env_repo_root, env_runner).await;

    {
        let mut registries = self.environment_registries.lock().expect("environment_registries lock");
        registries.insert(env_id, Arc::new(provider_registry));
    }

    Ok(StepOutcome::Completed)
}
```

Note: `build_environment_bag_from_vars()` and `FactoryRegistry::probe_all()` may not exist yet — if they don't, create minimal versions. The `build_environment_bag_from_vars` function builds an `EnvironmentBag` by converting raw env vars to `EnvironmentAssertion::EnvVarSet` entries and running binary detection through the environment runner. `probe_all` runs all factories against the bag and builds a `ProviderRegistry`. Check the existing discovery pipeline code to see if these already exist in some form.

- [ ] **Step 7: Implement `DestroyEnvironment` resolution**

```rust
StepAction::DestroyEnvironment { env_id } => {
    let handle = {
        let mut handles = self.environment_handles.lock().expect("environment_handles lock");
        handles.remove(&env_id).ok_or_else(|| format!("environment handle not found: {env_id}"))?
    };
    handle.destroy().await?;
    {
        let mut registries = self.environment_registries.lock().expect("environment_registries lock");
        registries.remove(&env_id);
    }
    Ok(StepOutcome::Completed)
}
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p flotilla-core executor::tests -v`
Expected: All tests pass, including the new `resolve_ensure_environment_image` test.

- [ ] **Step 9: Commit**

```bash
git add -A && git commit -m "feat: ExecutorStepResolver handles environment lifecycle actions (ensure image, create, discover, destroy)"
```

---

### Task 10: Hop Chain — Environment Wiring

Make `HopPlanBuilder::build_for_attachable()` insert `EnterEnvironment` hops when the attachable set has an `environment_id`. Make `resolve_prepared_commands_via_hop_chain()` accept an `environment_id` and use `DockerEnvironmentHopResolver`.

**Files:**
- Modify: `crates/flotilla-core/src/hop_chain/builder.rs:17-35` (build_for_attachable)
- Modify: `crates/flotilla-core/src/executor/workspace.rs:239-290` (resolve_prepared_commands_via_hop_chain)
- Modify: `crates/flotilla-core/src/executor.rs:536-558` (AttachWorkspace resolver)

- [ ] **Step 1: Write failing test for `build_for_attachable` with environment_id**

In `crates/flotilla-core/src/hop_chain/tests.rs`, add a test:

```rust
#[test]
fn build_for_attachable_inserts_enter_environment_hop() {
    use crate::attachable::{Attachable, AttachableStore, TerminalAttachable, TerminalPurpose};
    use flotilla_protocol::{AttachableId, AttachableSet, AttachableSetId, EnvironmentId, HostName};

    let local_host = HostName::new("laptop");
    let remote_host = HostName::new("feta");
    let env_id = EnvironmentId::new("env-123");
    let set_id = AttachableSetId::new("set-1");
    let att_id = AttachableId::new("att-1");

    let store = test_store_with_environment(remote_host.clone(), env_id.clone(), set_id.clone(), att_id.clone());

    let builder = HopPlanBuilder::new(&local_host);
    let plan = builder.build_for_attachable(&att_id, &*store.lock().unwrap()).expect("build_for_attachable");

    assert_eq!(plan.0.len(), 3);
    assert!(matches!(&plan.0[0], Hop::RemoteToHost { host } if *host == remote_host));
    assert!(matches!(&plan.0[1], Hop::EnterEnvironment { env_id: id, .. } if *id == env_id));
    assert!(matches!(&plan.0[2], Hop::AttachTerminal { .. }));
}
```

You'll need to create a helper `test_store_with_environment()` that builds an `AttachableStore` with an `AttachableSet` that has `environment_id: Some(env_id)`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core hop_chain::tests::build_for_attachable_inserts_enter_environment_hop -v`
Expected: FAIL — currently no `EnterEnvironment` hop is inserted.

- [ ] **Step 3: Update `build_for_attachable` to insert environment hop**

In `crates/flotilla-core/src/hop_chain/builder.rs`:

```rust
pub fn build_for_attachable(&self, attachable_id: &AttachableId, store: &dyn AttachableStoreApi) -> Result<HopPlan, String> {
    let registry = store.registry();

    let attachable = registry.attachables.get(attachable_id).ok_or_else(|| format!("attachable not found: {attachable_id}"))?;
    let set = registry.sets.get(&attachable.set_id).ok_or_else(|| format!("attachable set not found: {}", attachable.set_id))?;

    let mut hops = Vec::new();

    if let Some(ref host) = set.host_affinity {
        if host != self.local_host {
            hops.push(Hop::RemoteToHost { host: host.clone() });
        }
    }

    if let Some(ref env_id) = set.environment_id {
        hops.push(Hop::EnterEnvironment { env_id: env_id.clone(), provider: "docker".to_string() });
    }

    hops.push(Hop::AttachTerminal { attachable_id: attachable_id.clone() });

    Ok(HopPlan(hops))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core hop_chain::tests::build_for_attachable_inserts_enter_environment_hop -v`
Expected: PASS.

- [ ] **Step 5: Update `resolve_prepared_commands_via_hop_chain` for environment awareness**

In `crates/flotilla-core/src/executor/workspace.rs`, change the function signature and body:

```rust
fn resolve_prepared_commands_via_hop_chain(
    target_host: &HostName,
    checkout_path: &Path,
    commands: &[ResolvedPaneCommand],
    config_base: &Path,
    local_host: &HostName,
    environment_id: Option<&flotilla_protocol::EnvironmentId>,
    container_name: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let ssh_resolver = ssh_resolver_from_config(&DaemonHostPath::new(config_base))?;

    let env_resolver: Arc<dyn crate::hop_chain::environment::EnvironmentHopResolver> = match (environment_id, container_name) {
        (Some(env_id), Some(name)) => {
            let mut containers = std::collections::HashMap::new();
            containers.insert(env_id.clone(), name.to_string());
            Arc::new(crate::hop_chain::environment::DockerEnvironmentHopResolver::new(containers))
        }
        _ => Arc::new(NoopEnvironmentHopResolver),
    };

    let hop_resolver = HopResolver {
        remote: Arc::new(ssh_resolver),
        environment: env_resolver,
        terminal: Arc::new(NoopTerminalHopResolver),
        strategy: Arc::new(AlwaysWrap),
    };
    let plan_builder = HopPlanBuilder::new(local_host);

    let mut result = Vec::with_capacity(commands.len());
    for cmd in commands {
        let mut plan = plan_builder.build_for_prepared_command(target_host, &cmd.args);

        // Insert environment hop if needed
        if let Some(env_id) = environment_id {
            // Insert EnterEnvironment before the RunCommand hop
            let run_cmd_index = plan.0.iter().position(|h| matches!(h, Hop::RunCommand { .. })).unwrap_or(plan.0.len());
            plan.0.insert(run_cmd_index, Hop::EnterEnvironment {
                env_id: env_id.clone(),
                provider: "docker".to_string(),
            });
        }

        let mut context = ResolutionContext {
            current_host: local_host.clone(),
            current_environment: None,
            working_directory: Some(ExecutionEnvironmentPath::new(checkout_path)),
            actions: Vec::new(),
            nesting_depth: 0,
        };
        let resolved = hop_resolver.resolve(&plan, &mut context)?;

        if resolved.0.len() != 1 {
            return Err(format!(
                "hop chain resolution produced {} actions for role '{}', expected exactly 1 (AlwaysWrap)",
                resolved.0.len(), cmd.role
            ));
        }
        let command_string = match resolved.0.into_iter().next() {
            Some(ResolvedAction::Command(args)) => arg::flatten(&args, 0),
            Some(_) => return Err(format!("hop chain resolution produced a non-Command action for role '{}'", cmd.role)),
            None => unreachable!("len checked above"),
        };

        result.push((cmd.role.clone(), command_string));
    }
    Ok(result)
}
```

- [ ] **Step 6: Update `attach_prepared_workspace` to pass environment info**

In `crates/flotilla-core/src/executor/workspace.rs`, update the call:

```rust
let attach_commands = resolve_prepared_commands_via_hop_chain(
    &prepared.target_host,
    &prepared.checkout_path,
    &prepared.prepared_commands,
    self.config_base,
    self.local_host,
    prepared.environment_id.as_ref(),
    None, // container_name resolved at attach time — see step 7
)?;
```

- [ ] **Step 7: Update `AttachWorkspace` resolver to pass container_name**

In `crates/flotilla-core/src/executor.rs`, update the `AttachWorkspace` match arm to extract container_name from `environment_handles` when the `PreparedWorkspace` has an `environment_id`:

```rust
StepAction::AttachWorkspace => {
    let prepared = prior
        .iter()
        .rev()
        .find_map(|o| match o {
            StepOutcome::Produced(CommandValue::PreparedWorkspace(prepared)) => Some(prepared.clone()),
            _ => None,
        })
        .ok_or_else(|| "prepared workspace not produced by prior step".to_string())?;

    // Look up container_name if environment_id is present
    let container_name = prepared.environment_id.as_ref().and_then(|env_id| {
        let handles = self.environment_handles.lock().expect("environment_handles lock");
        handles.get(env_id).and_then(|h| h.container_name().map(|s| s.to_string()))
    });

    let tm = self.terminal_manager();
    let workspace_orchestrator = WorkspaceOrchestrator::new(
        self.repo.root.as_path(),
        self.registry.as_ref(),
        self.config_base.as_path(),
        &self.attachable_store,
        self.daemon_socket_path.as_ref().map(|p| p.as_path()),
        &self.local_host,
        tm.as_ref(),
    );
    workspace_orchestrator
        .attach_prepared_workspace_with_env(&prepared, container_name.as_deref())
        .await?;
    Ok(StepOutcome::Completed)
}
```

Add `attach_prepared_workspace_with_env` to `WorkspaceOrchestrator` that forwards `container_name` to `resolve_prepared_commands_via_hop_chain`.

- [ ] **Step 8: Update the `PrepareWorkspace` resolver to set `environment_id` on `PreparedWorkspace`**

In the `PrepareWorkspace` match arm in `crates/flotilla-core/src/executor.rs`, when running inside an environment context, set `environment_id`:

The `PrepareWorkspace` step runs with `StepExecutionContext::Environment(host, env_id)`. But the resolver only sees the `StepAction`, not the `StepExecutionContext`. The environment_id needs to flow differently — either through a field on the resolver that's set per-step, or through the step action itself.

Simpler approach: The plan builder already knows the `env_id`. Add an `environment_id: Option<EnvironmentId>` field to `StepAction::PrepareWorkspace`:

In `crates/flotilla-protocol/src/step.rs`:
```rust
PrepareWorkspace {
    checkout_path: Option<ExecutionEnvironmentPath>,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment_id: Option<crate::EnvironmentId>,
},
```

The plan builder sets this when building environment plans. The resolver reads it and puts it in the `PreparedWorkspace`.

- [ ] **Step 9: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 10: Commit**

```bash
git add -A && git commit -m "feat: hop chain inserts EnterEnvironment hops for environment-targeted commands"
```

---

### Task 11: Host Summary — Environment Listing

Populate `HostSummary.environments` by querying `EnvironmentProvider::list()` during host summary build.

**Files:**
- Modify: `crates/flotilla-core/src/host_summary.rs:15-28`

- [ ] **Step 1: Write failing test**

In `crates/flotilla-core/src/host_summary.rs`, add to the `tests` module:

```rust
#[tokio::test]
async fn build_local_host_summary_populates_environments() {
    use flotilla_protocol::{EnvironmentId, EnvironmentInfo, EnvironmentStatus, ImageId};

    let host_name = HostName::new("test-host");
    let bag = EnvironmentBag::new();
    let env = TestEnv::default();

    let environments = vec![EnvironmentInfo {
        id: EnvironmentId::new("env-1"),
        image: ImageId::new("test-image:latest"),
        status: EnvironmentStatus::Running,
    }];

    let summary = build_local_host_summary(&host_name, &bag, vec![], &env, environments);

    assert_eq!(summary.environments.len(), 1);
    assert_eq!(summary.environments[0].id, EnvironmentId::new("env-1"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core host_summary::tests::build_local_host_summary_populates_environments -v`
Expected: FAIL — function signature doesn't accept environments yet.

- [ ] **Step 3: Add `environments` parameter to `build_local_host_summary`**

In `crates/flotilla-core/src/host_summary.rs`:

```rust
pub fn build_local_host_summary(
    host_name: &HostName,
    host_bag: &EnvironmentBag,
    providers: Vec<HostProviderStatus>,
    env: &dyn EnvVars,
    environments: Vec<flotilla_protocol::EnvironmentInfo>,
) -> HostSummary {
    HostSummary {
        host_name: host_name.clone(),
        system: collect_system_info(env),
        inventory: inventory_from_bag(host_bag),
        providers,
        environments,
    }
}
```

- [ ] **Step 4: Fix callers**

Find all call sites of `build_local_host_summary` and add `vec![]` as the environments argument (for now — the caller that does the actual environment listing will be wired separately).

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: build_local_host_summary accepts and populates HostSummary.environments"
```

---

### Task 12: Environment Listing in Host Summary Build Pipeline

Wire the actual `EnvironmentProvider::list()` call into the host summary build pipeline so running environments appear in `HostSummary.environments`.

**Files:**
- Modify: The call site(s) of `build_local_host_summary` (likely in `crates/flotilla-core/src/in_process.rs` or the daemon server)

- [ ] **Step 1: Find the call site**

Search for `build_local_host_summary(` in the codebase to find where host summaries are built. Modify that call site to:
1. Get the `EnvironmentProvider` from the host-level `ProviderRegistry`
2. Call `list()` to get handles
3. Map handles to `EnvironmentInfo` via `handle.id()`, `handle.image()`, `handle.status().await`
4. Pass the resulting `Vec<EnvironmentInfo>` to `build_local_host_summary`

```rust
let environments = if let Some(env_provider) = host_registry.environment_providers.preferred() {
    match env_provider.list().await {
        Ok(handles) => {
            let mut infos = Vec::with_capacity(handles.len());
            for handle in &handles {
                let status = handle.status().await.unwrap_or(EnvironmentStatus::Failed("status query failed".into()));
                infos.push(EnvironmentInfo {
                    id: handle.id().clone(),
                    image: handle.image().clone(),
                    status,
                });
            }
            infos
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to list environments for host summary");
            vec![]
        }
    }
} else {
    vec![]
};
```

- [ ] **Step 2: Verify compilation and tests**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: host summary queries EnvironmentProvider::list() to populate environment info"
```

---

### Task 13: Format, Lint, and Final Verification

Run all CI checks.

**Files:** Potentially any file touched above.

- [ ] **Step 1: Format**

Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Fix any warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 4: Commit any formatting/lint fixes**

```bash
git add -A && git commit -m "chore: fmt and clippy fixes"
```

---

## Implementation Notes

### Interior Mutability for Resolver State

`ExecutorStepResolver::resolve()` takes `&self` but needs to mutate `environment_handles` and `environment_registries`. Use `std::sync::Mutex<HashMap<...>>` for these fields. The resolver runs sequentially (one step at a time), so contention is not a concern.

### EnvironmentId Pre-Allocation

The plan builder generates the `EnvironmentId` (via `uuid::Uuid::new_v4()`) so it can be referenced across multiple steps in the plan. The resolver doesn't generate IDs — it receives them from the step action and uses them for socket setup, handle storage, and registry keying.

### Placeholder Image in CreateEnvironment

The `CreateEnvironment` step action carries an `image: ImageId` field per the spec, but the plan builder can't know the image ID at plan-build time (it's produced by `EnsureEnvironmentImage`). The resolver extracts the real `ImageId` from prior step outcomes, ignoring the action's `image` field. An alternative design would remove `image` from `CreateEnvironment` and always extract from prior — but the spec's design is preserved for wire-format consistency.

### DiscoverEnvironmentProviders

This step requires running the full discovery pipeline (environment bag construction → factory probing → provider registry) inside the container. The spec says raw env vars from `handle.env_vars()` feed the detection pipeline. If `build_environment_bag_from_vars()` doesn't exist, implement it as: convert each raw `KEY=VALUE` to `EnvironmentAssertion::EnvVarSet`, then run binary detectors through the environment runner. Similarly, `FactoryRegistry::probe_all()` may need implementation — it should iterate all factory categories, probe each, and build the resulting `ProviderRegistry`.

### Sandbox Socket Strategy

The spec describes per-environment sockets via `EnvironmentSocketRegistry::add()` during `CreateEnvironment`. The registry lives in `flotilla-daemon`, but the resolver lives in `flotilla-core` — a cross-crate boundary.

**This plan uses the main daemon socket** (`self.daemon_socket_path`) for `CreateOpts.daemon_socket_path` instead of per-environment sockets. The container mounts the main daemon socket at `/run/flotilla.sock`. This works correctly for Phase D testing and single-container scenarios.

**Full per-environment socket isolation** requires:
1. A trait in `flotilla-core` (e.g., `EnvironmentSocketOps`) with `add()` and `remove()` methods
2. Implementation on `EnvironmentSocketRegistry` in `flotilla-daemon`
3. `Arc<Mutex<dyn EnvironmentSocketOps>>` field on `ExecutorStepResolver`, threaded from the daemon server
4. `CreateEnvironment` resolver calls `add()` before `provider.create()`, cleans up on failure
5. `DestroyEnvironment` resolver calls `remove()` after `handle.destroy()`

This can be wired as a follow-up after the core E2E path works.

### Items Not Implemented (Deferred)

- **Per-environment socket isolation**: See above. Main daemon socket used for now.
- **Token resolution**: Tokens are passed as empty `vec![]` in `CreateOpts`. Token config resolution is Phase E scope.
- **Automatic rollback**: If a mid-plan step fails after environment creation, the container is left running. Manual cleanup or future `DestroyEnvironment` command.
- **Environment-aware `DiscoverEnvironmentProviders`**: The `DiscoverEnvironmentProviders` resolver needs to build an `EnvironmentBag` from raw env vars and run the discovery pipeline through the environment runner. The existing discovery pipeline (`DiscoveryRuntime::discover_with_bag`) may work directly, or may need a simplified entry point that takes raw env vars + a runner and returns a `ProviderRegistry`. Implement the minimal viable version — convert raw env vars to `EnvironmentAssertion::EnvVarSet` entries, run `FactoryRegistry::default_all().probe_all()` with the environment's runner.
