# Presentation Manager and Presentation Resource (Stage 5)

## Context

Stages 1–4a established the resource-oriented convoy stack: `WorkflowTemplate`, `Convoy`, `TaskWorkspace`, `Environment`, `Clone`, `Checkout`, `TerminalSession`, each with a reconciler. Tool processes run end-to-end on a single task; agent processes are rejected until selector resolution lands.

What's missing is the **presentation side**. A convoy can be created, tasks can provision Environments/Checkouts/TerminalSessions, but the live multiplexer workspace the user interacts with is still wired through the pre-resource `WorkspaceOrchestrator` / `AttachableSet` machinery. Nothing in the convoy stack talks to `WorkspaceManager`.

Stage 5 closes that gap. It renames `WorkspaceManager` to `PresentationManager`, introduces a `Presentation` resource that declares "a slice of the convoy graph is being shown, governed by this policy," and adds a reconciler that keeps a live workspace aligned with the current set of Ready `TaskWorkspace`s and their `TerminalSession`s.

## Design Decisions

### `Presentation` is owned by Convoy (not by Task)

Each convoy gets one `Presentation` resource. The alternative — one Presentation per Task, or Presentation-as-direct-call-from-task-workspace-reconciler — was rejected because it bakes task-scoped workspace lifecycles into the design exactly when we want the presentation unit to be something that can later span multiple tasks (reconfigure-in-place) or even multiple convoys (Yeoman-era multi-convoy views).

### Declarative subscription, not active task list

`Presentation.spec` is stable day-to-day. It carries a selector that matches `TerminalSession`s by convoy. The convoy controller does not rewrite the spec as tasks transition. Instead, reconciliation is driven by label-based watches on `TerminalSession` (membership) and `TaskWorkspace` (liveness).

This is the k8s-Deployment analogy: a Deployment spec says "I want N replicas of image X"; it doesn't get rewritten every time a Pod comes up. Similarly, a Presentation spec says "present the sessions for this convoy using this policy"; the world (session graph, task phases) changes, the reconciler responds.

### Replace-on-change for v1

Task transitions cause the reconciler to tear down the current workspace and re-create it. Reconfigure-in-place (`update_workspace`, `add_panes`) is deferred. For single-task convoys — which is what selector-resolution-blocked stage 4a supports anyway — replace-on-change and reconfigure are observationally equivalent.

Future `PresentationPolicy` variants (`continuous`, `churn`) will decide reconfigure vs replace on a per-presentation basis. The resource schema does not need to change to support that.

### Presentation policy is code-level in v1

`Presentation.spec.presentation_policy_ref: String` mirrors `TaskWorkspace.spec.placement_policy_ref`. Today the only recognized value is `"default"`, resolved through a code-level registry. When `PresentationPolicy` becomes a real CRD (parallel to the deferred `PlacementPolicy` reification), the reconciler gains a watch without schema churn anywhere.

### Process metadata via labels

`ProcessDefinition` gains an optional `labels: BTreeMap<String, String>` field. These propagate to `TerminalSession.metadata.labels` at provisioning time. The default policy does not use them for slot-matching (it keys off the existing `role` field). Yeoman-era layout policies will. This is also the hook for agents to read and write process metadata programmatically.

### Rename is internal

`WorkspaceManager` → `PresentationManager` renames the trait, module, config key, and registry field. TUI strings referring to the multiplexer concept still say "workspace" because tmux / cmux / zellij actually use that word (or an adjacent one — screen, tab). User-facing terminology is left for later evolution. `WorkspaceOrchestrator` keeps calling the renamed trait; it's removed in stage 7.

### Creation trigger is a policy, not a contract

Stage 5 ships a single auto-present hook ("on `ConvoyPhase::Active`, create one default-policy Presentation"), which matches today's default flotilla TUI behaviour and preserves passthrough terminal pool semantics (attachment starts processes). The trigger is deliberately a single swap-point in the convoy reconciler — later work can replace it with explicit UI commands, Yeoman decisions, or a policy table without touching the Presentation resource.

---

## Architecture

```
Convoy becomes Active
  │
  ▼
Convoy reconciler emits CreatePresentation actuation
  │
  ▼
Presentation resource exists (selector-only spec)
  │
  │  task_workspace reconciler stamps labels on TerminalSessions:
  │    flotilla.work/convoy, task, task_workspace, role, + user labels
  │
  ▼
Presentation reconciler watches:
  - Presentation
  - TerminalSession matching selector
  - TaskWorkspace by convoy label
  │
  ▼  fetch_dependencies:
  │   list sessions via selector → filter to Ready TaskWorkspaces
  │   walk Environment / Host / Checkout → hop-chain resolve attach commands
  │   compute spec_hash → compare to observed
  │
  ▼  if hash differs:
  PresentationRuntime.apply(PresentationPlan { previous, ... })
    ├─ PresentationPolicy.render → WorkspaceAttachRequest
    ├─ PresentationManager.delete_workspace(previous)   ← replace-on-change
    └─ PresentationManager.create_workspace(req)
  │
  ▼  reconcile:
  PresentationStatusPatch::MarkActive { manager, ws_ref, spec_hash, ready_at }
```

## Rename

Internal only. No user-facing string changes.

**Touched:**

- `crates/flotilla-core/src/providers/workspace/` → `providers/presentation/`
- Trait `WorkspaceManager` → `PresentationManager`
- Registry field `workspace_managers` → `presentation_managers`
- Config key `workspace_manager` → `presentation_manager` (field in `RepoConfig`, type `WorkspaceManagerConfig` → `PresentationManagerConfig`)
- Implementations renamed: `CmuxWorkspaceManager` → `CmuxPresentationManager`, etc.

**Unchanged:**

- TUI string labels ("workspace" still means the multiplexer concept in UI copy).
- `WorkspaceAttachRequest`, `Workspace` protocol types — these are what the trait consumes and emits; they're the multiplexer-level concept, not the Presentation-level concept.
- `WorkspaceOrchestrator` and the `AttachableSet` path. Still invokes the renamed trait. Removed in stage 7.

## Trait Changes

```rust
#[async_trait]
pub trait PresentationManager: Send + Sync {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String>;
    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String>;
    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String>;   // NEW
    fn binding_scope_prefix(&self) -> String;
}
```

`delete_workspace` is the only new method — required for replace-on-change. Each implementation (cmux, tmux, zellij) gets a straightforward implementation in terms of the underlying multiplexer's workspace / tab / screen destroy verb.

## `Presentation` Resource

```rust
define_resource!(
    Presentation, "presentations",
    PresentationSpec, PresentationStatus, PresentationStatusPatch
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationSpec {
    pub convoy_ref: String,
    pub presentation_policy_ref: String,
    pub name: String,
    pub process_selector: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresentationPhase {
    #[default]
    Pending,
    Active,
    Reconfiguring,
    TornDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationStatus {
    pub phase: PresentationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workspace_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_presentation_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_spec_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationStatusPatch {
    BeginReplace,
    MarkActive {
        presentation_manager: String,
        workspace_ref: String,
        spec_hash: String,
        ready_at: DateTime<Utc>,
    },
    MarkFailed { message: String },
    MarkTornDown,
}
```

`observed_spec_hash` compares cheaply; no need to store the full last-applied spec.

**Ownership:** `OwnerReference` to the Convoy. Convoy delete cascades. The Presentation's `metadata.labels` includes `flotilla.work/convoy: <convoy-name>` so secondary watches can key off it.

## Labels

New shared module `crates/flotilla-resources/src/labels.rs` exports the well-known keys:

```rust
pub const CONVOY_LABEL: &str = "flotilla.work/convoy";
pub const TASK_LABEL: &str = "flotilla.work/task";
pub const TASK_WORKSPACE_LABEL: &str = "flotilla.work/task_workspace";  // already used by task_workspace reconciler
pub const ROLE_LABEL: &str = "flotilla.work/role";                       // already used
```

(Note the inconsistency with existing `task_workspace` / `repo-key` delimiters — leave as-is for existing labels; new ones follow underscore convention.)

### `ProcessDefinition` schema change

```rust
pub struct ProcessDefinition {
    pub role: String,
    #[serde(flatten)]
    pub source: ProcessSource,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,   // NEW
}
```

Optional, defaults empty. No existing templates need to change.

### Label propagation in `task_workspace` reconciler

Extend the existing `TerminalSession` creation site to stamp:

- `CONVOY_LABEL: <TaskWorkspace.spec.convoy_ref>`
- `TASK_LABEL: <TaskWorkspace.spec.task>`
- `TASK_WORKSPACE_LABEL: <TaskWorkspace.metadata.name>` *(already present)*
- `ROLE_LABEL: <ProcessDefinition.role>` *(already present)*
- Plus all entries from `ProcessDefinition.labels`

A single helper (`build_session_labels` in `flotilla-controllers::reconcilers::task_workspace`) consolidates this so no caller forgets a key.

## `PresentationPolicy` (code-level)

```rust
pub trait PresentationPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn render(&self, processes: &[ResolvedProcess], context: &PolicyContext) -> RenderedWorkspace;
}

pub struct PolicyContext {
    pub name: String,
    pub working_directory: ExecutionEnvironmentPath,
}

pub struct ResolvedProcess {
    pub role: String,
    pub labels: BTreeMap<String, String>,
    pub attach_command: String,
}

pub struct RenderedWorkspace {
    pub attach_request: WorkspaceAttachRequest,
}

pub struct PresentationPolicyRegistry {
    policies: HashMap<String, Arc<dyn PresentationPolicy>>,
}

impl PresentationPolicyRegistry {
    pub fn with_defaults() -> Self { /* registers DefaultPolicy */ }
    pub fn resolve(&self, name: &str) -> Option<&Arc<dyn PresentationPolicy>>;
}
```

### `DefaultPolicy`

Replicates today's `default_template()` behaviour:

1. Group processes by `role` (first-seen order).
2. One pane per role. Multiple processes for the same role → tabs inside the pane (overflow=tab, matching `build_pane_layout`).
3. First role → main pane; later roles → split right.
4. Emits `WorkspaceAttachRequest { template_yaml: None, attach_commands: Vec<(role, attach_command)>, working_directory, name }`. `PresentationManager.create_workspace` then walks its existing `resolve_template` → `build_pane_layout` path.

Effect: a single-task convoy with one agent role and one build role renders identically to today's `.flotilla/workspace.yaml` default.

### Unknown policy name

Reconciler emits `PresentationStatusPatch::MarkFailed { message: "unknown presentation policy '{name}'" }`. No runtime invocation. Mirrors stage 4a's rejection of unsupported process sources.

## Presentation Reconciler

**Location:** `crates/flotilla-controllers/src/reconcilers/presentation.rs`.

```rust
#[async_trait]
pub trait PresentationRuntime: Send + Sync {
    async fn apply(&self, plan: &PresentationPlan) -> Result<AppliedPresentation, String>;
    async fn tear_down(&self, manager: &str, workspace_ref: &str) -> Result<(), String>;
}

pub struct PresentationPlan {
    pub policy: String,
    pub name: String,
    pub processes: Vec<ResolvedProcess>,
    pub working_directory: ExecutionEnvironmentPath,
    pub previous: Option<PreviousWorkspace>,
    pub spec_hash: String,
}

pub struct PreviousWorkspace {
    pub presentation_manager: String,
    pub workspace_ref: String,
}

pub struct AppliedPresentation {
    pub presentation_manager: String,
    pub workspace_ref: String,
    pub spec_hash: String,
}

pub struct PresentationReconciler<R> {
    runtime: Arc<R>,
    task_workspaces: TypedResolver<TaskWorkspace>,
    terminal_sessions: TypedResolver<TerminalSession>,
    environments: TypedResolver<Environment>,
    checkouts: TypedResolver<Checkout>,
    hosts: TypedResolver<Host>,
    hop_chain: HopChainContext,   // encapsulates flotilla-core::hop_chain resolver + local_host + config_base
}

pub enum PresentationDeps {
    InSync,
    Applied(AppliedPresentation),
    Failed(String),
    UnknownPolicy(String),
}
```

### `fetch_dependencies`

1. `terminal_sessions.list_matching_labels(&spec.process_selector)`.
2. Group sessions by `TASK_WORKSPACE_LABEL`. For each distinct ref, `task_workspaces.get(...)` and keep only Ready ones.
3. For each surviving session, resolve routing:
   - `environments.get(&session.spec.env_ref)` → host_ref + docker_container_id
   - `hosts.get(host_ref)` → HostName
   - `checkouts.get(...)` via the TaskWorkspace's `checkout_ref` → path
4. Build hop-chain plan per session → resolve to attach command string via `flotilla-core::hop_chain` (same machinery `WorkspaceOrchestrator::resolve_prepared_commands_via_hop_chain` uses today). The `HopChainContext` bundles the SSH config base path, local `HostName`, and environment/terminal resolver construction. Details are implementation-stage concerns; the shape is "given session + env + host + checkout, produce a fully-qualified attach command."
5. Compute `spec_hash = hash((policy_ref, sorted_session_refs, sorted_attach_commands, sorted_labels))`.
6. If `status.observed_spec_hash == spec_hash` → `Deps::InSync`.
7. Else `runtime.apply(PresentationPlan { previous: status_derived, ... }).await`:
   - `Ok(applied)` → `Deps::Applied(applied)`.
   - `Err(msg)` if unknown policy → `Deps::UnknownPolicy(name)`.
   - `Err(msg)` otherwise → `Deps::Failed(msg)`.

### `reconcile`

Pure. Deps → status patch:

- `Deps::InSync` → `None`.
- `Deps::Applied(a)` → `Some(MarkActive { ... })`.
- `Deps::Failed(msg)` → `Some(MarkFailed { message: msg })`.
- `Deps::UnknownPolicy(name)` → `Some(MarkFailed { message: format!("unknown presentation policy '{name}'") })`.

### `run_finalizer`

```rust
async fn run_finalizer(&self, obj: &ResourceObject<Presentation>) -> Result<(), ResourceError> {
    if let Some(status) = &obj.status {
        if let (Some(mgr), Some(ws)) = (
            status.observed_presentation_manager.as_deref(),
            status.observed_workspace_ref.as_deref(),
        ) {
            self.runtime.tear_down(mgr, ws).await.map_err(ResourceError::other)?;
        }
    }
    Ok(())
}

fn finalizer_name(&self) -> Option<&'static str> { Some("flotilla.work/presentation-teardown") }
```

### Secondary watches

```rust
pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Presentation>>> {
    vec![
        Box::new(LabelJoinWatch::<TerminalSession, Presentation>  { label_key: CONVOY_LABEL, _marker: PhantomData }),
        Box::new(LabelJoinWatch::<TaskWorkspace, Presentation>    { label_key: CONVOY_LABEL, _marker: PhantomData }),
    ]
}
```

`TerminalSession` events fire on membership changes; `TaskWorkspace` events fire on phase transitions. Both route to Presentations carrying the same convoy label.

## `PresentationRuntime` Implementation

Lives alongside other runtime impls in `flotilla-controllers`.

```rust
pub struct ProviderPresentationRuntime {
    registry: Arc<ProviderRegistry>,
    policies: Arc<PresentationPolicyRegistry>,
}

#[async_trait]
impl PresentationRuntime for ProviderPresentationRuntime {
    async fn apply(&self, plan: &PresentationPlan) -> Result<AppliedPresentation, String> {
        let policy = self.policies.resolve(&plan.policy)
            .ok_or_else(|| format!("unknown presentation policy '{}'", plan.policy))?;
        let (manager_name, manager) = self.registry.presentation_managers.preferred_with_desc()
            .ok_or_else(|| "no presentation manager configured".to_string())?;

        if let Some(prev) = &plan.previous {
            // best-effort delete; failure to delete the old workspace shouldn't block re-create
            let _ = manager.delete_workspace(&prev.workspace_ref).await;
        }

        let RenderedWorkspace { attach_request } = policy.render(&plan.processes, &PolicyContext {
            name: plan.name.clone(),
            working_directory: plan.working_directory.clone(),
        });
        let (ws_ref, _) = manager.create_workspace(&attach_request).await?;

        Ok(AppliedPresentation {
            presentation_manager: manager_name.to_string(),
            workspace_ref: ws_ref,
            spec_hash: plan.spec_hash.clone(),
        })
    }

    async fn tear_down(&self, manager: &str, workspace_ref: &str) -> Result<(), String> {
        let mgr = self.registry.presentation_managers.get(manager)
            .ok_or_else(|| format!("presentation manager '{manager}' no longer available"))?;
        mgr.delete_workspace(workspace_ref).await
    }
}
```

Best-effort delete for the previous workspace means a partial failure (delete ok, create fails) leaves the Presentation in `Reconfiguring` with no live workspace — next reconcile retries with `previous: None` and eventually converges. Documented as an accepted v1 limitation.

## Convoy Reconciler Extension

```rust
pub enum Actuation {
    // ... existing variants
    CreatePresentation { meta: InputMeta, spec: PresentationSpec },
    DeletePresentation { name: String },
}
```

In the existing convoy reconcile function:

- Convoy transitions to `Active` with no existing Presentation (checked via label-indexed read or one-shot list) → emit `CreatePresentation` actuation with:
  - `meta.name = format!("{}-presentation", convoy.metadata.name)` (or similar — single deterministic derivation)
  - `meta.labels = { CONVOY_LABEL: convoy.metadata.name }`
  - `meta.owner_references = [OwnerReference::for(convoy)]`
  - `spec.convoy_ref = convoy.metadata.name`
  - `spec.presentation_policy_ref = "default"`
  - `spec.name = convoy.metadata.name`
  - `spec.process_selector = { CONVOY_LABEL: convoy.metadata.name }`
- Convoy transitions to `Completed` / `Failed` / `Cancelled` → emit `DeletePresentation { name }`.

This is the single swap point for future explicit-trigger creation policies.

## Testing

### Unit tests

- **Policy**: `DefaultPolicy::render` with one role, multiple roles, duplicate roles. Assert parity with `build_pane_layout` for equivalent inputs.
- **Label constants**: a trivial compile-time test that the expected keys exist (guards accidental renames).

### Reconciler tests (in-memory backend)

- Presentation with no matching sessions → `Pending`, no `runtime.apply` calls.
- Presentation with matching sessions, all TaskWorkspaces Ready → `apply` called once, status `Active` with recorded hash.
- Second reconcile, unchanged world → no `apply` call, status unchanged.
- TaskWorkspace transitions Ready→Completed → next reconcile recomputes, new hash, `apply` called with `previous` populated, status updates.
- Unknown policy → `Failed`, no runtime call.
- Finalizer on Presentation delete → `tear_down` called with recorded manager + ws_ref.

### Convoy reconciler tests

- Convoy → Active → `CreatePresentation` actuation emitted.
- Convoy → Active twice (re-reconcile) → only one Presentation created (idempotent).
- Convoy → Completed → `DeletePresentation { name }` emitted.

### Label propagation tests

Extension to existing `task_workspace_reconciler.rs`: created TerminalSessions carry `CONVOY_LABEL`, `TASK_LABEL`, `TASK_WORKSPACE_LABEL`, `ROLE_LABEL`, plus propagated `ProcessDefinition.labels`.

### Integration (optional, non-blocking)

`InProcessDaemon`-level test: create a single-task Convoy → wait for Presentation to reach `Active` → inspect recorded `PresentationRuntime` calls. Uses a mock `PresentationRuntime`. Validates wiring, not multiplexer behaviour.

### Live-multiplexer coverage

Deferred. The new `delete_workspace` method gets a focused replay-style test per implementation (cmux, tmux, zellij). Existing `create_workspace` replay coverage is unchanged.

## Out of Scope

- **Reconfigure-in-place** (stage boundary: PresentationPolicy variant + new trait methods).
- **`PresentationPolicy` as a CRD** (reify when Yeoman / multi-policy demand arrives).
- **Multiple presentation managers per Presentation** (registry still returns one preferred).
- **Convoy TUI pane** (stage 6).
- **TUI convoy view** (stage 6).
- **AttachableSet removal** (stage 7).
- **Label-selector-driven layout policies / signals-and-slots** (Yeoman-era).
- **Artifact resource** (future — selector shape extends naturally when it lands).
- **User-facing terminology changes** (TUI still says "workspace").
- **Explicit presentation-trigger UX** (auto-present-on-Active stays; single swap point in convoy reconciler).

## Open Risks

1. **Visible gap during replace.** Task transitions cause a brief workspace disappearance. In practice stage 4a agent processes can't run end-to-end yet, so only hand-written multi-task tool workflows hit this during testing.
2. **Silent selector breakage.** A missed well-known label on a TerminalSession silently excludes it from the Presentation. Mitigation: `flotilla-resources::labels` constants + `build_session_labels` helper used uniformly by the task_workspace reconciler.
3. **Non-atomic replace.** Delete succeeds then create fails → `Reconfiguring` with no live workspace. Next reconcile retries with `previous: None`. Acceptable for v1, documented.

