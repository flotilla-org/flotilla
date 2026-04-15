# Presentation Manager and Presentation Resource (Stage 5)

## Context

Stages 1–4a established the resource-oriented convoy stack: `WorkflowTemplate`, `Convoy`, `TaskWorkspace`, `Environment`, `Clone`, `Checkout`, `TerminalSession`, each with a reconciler. Tool processes run end-to-end on a single task; agent processes are rejected until selector resolution lands.

What's missing is the **presentation side**. A convoy can be created, tasks can provision Environments/Checkouts/TerminalSessions, but the live multiplexer workspace the user interacts with is still wired through the pre-resource `WorkspaceOrchestrator` / `AttachableSet` machinery. Nothing in the convoy stack talks to `WorkspaceManager`.

Stage 5 closes that gap. It renames `WorkspaceManager` to `PresentationManager`, introduces a `Presentation` resource that declares "a slice of the convoy graph is being shown, governed by this policy," and adds a reconciler that keeps a live workspace aligned with the current set of live `TerminalSession`s for the convoy.

It also changes one stage-4a decision: `TaskWorkspace` lifecycle is tied to task lifecycle — the convoy controller deletes a `TaskWorkspace` when its task reaches a terminal phase, and the `TaskWorkspace` finalizer cascades to `TerminalSession` deletion. This replaces the "persist until convoy deletion" model, which was incompatible with passthrough terminal pools anyway (tearing down the workspace kills passthrough processes regardless).

## Design Decisions

### `Presentation` is owned by Convoy (not by Task)

Each convoy gets one `Presentation` resource. The alternative — one Presentation per Task, or Presentation-as-direct-call-from-task-workspace-reconciler — was rejected because it bakes task-scoped workspace lifecycles into the design exactly when we want the presentation unit to be something that can later span multiple tasks (reconfigure-in-place) or even multiple convoys (Yeoman-era multi-convoy views).

### Declarative subscription, not active task list

`Presentation.spec` is stable day-to-day. It carries a selector that matches `TerminalSession`s by convoy. The convoy controller does not rewrite the spec as tasks transition. Instead, reconciliation is driven by a single label-based watch on `TerminalSession` — membership changes (sessions appear when tasks launch, disappear when tasks complete and their TaskWorkspace cascades) are the only signal the reconciler needs.

This is the k8s-Deployment analogy: a Deployment spec says "I want N replicas of image X"; it doesn't get rewritten every time a Pod comes up. Similarly, a Presentation spec says "present the sessions for this convoy using this policy"; the world (session graph) changes, the reconciler responds.

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
  │    flotilla.work/convoy, task, task_workspace, role,
  │    task_ordinal, process_ordinal, + user labels
  │
  ▼
Presentation reconciler watches:
  - Presentation
  - TerminalSession matching selector
  │
  ▼  fetch_dependencies:
  │   list sessions via selector
  │   walk Environment / Host → hop-chain resolve per-session attach commands
  │   (per-session cwd = TerminalSession.spec.cwd, baked into attach_command)
  │   resolve presentation_local_cwd via fallback chain (home → cwd → config_base)
  │   sort by (task_ordinal, process_ordinal, session_name)
  │   compute spec_hash → compare to observed
  │
  ▼  if sessions non-empty AND hash differs:
  │     runtime.apply(PresentationPlan { previous, ... })
  │       ├─ PresentationPolicy.render → WorkspaceAttachRequest
  │       ├─ prev_mgr.delete_workspace(previous.ws_ref)   ← via prev's manager
  │       └─ current_mgr.create_workspace(req)
  │     status → Active
  │
  ▼  if sessions empty AND observed_workspace is Some:
  │     runtime.tear_down(observed_manager, observed_ws_ref)
  │     status → TornDown

Convoy task → Completed/Failed/Cancelled:
  Convoy reconciler deletes the TaskWorkspace (new extension)
  TaskWorkspace finalizer cascades to TerminalSession deletion
  → selector watch fires → reconciler recomputes → replace/tear down
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
    TornDown,   // no live processes right now; may transition back to Active when sessions reappear
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
    MarkActive {
        presentation_manager: String,
        workspace_ref: String,
        spec_hash: String,
        ready_at: DateTime<Utc>,
    },
    MarkTornDown,                       // workspace deleted; no observed_workspace_ref/manager/hash
    MarkFailed { message: String },
}
```

`observed_spec_hash` compares cheaply; no need to store the full last-applied spec.

**Ownership:** `OwnerReference` to the Convoy (recorded for discoverability / future owner-ref GC; flotilla-cp backends do **not** implement owner-ref cascade today — see Convoy finalizer below). The Presentation's `metadata.labels` includes `flotilla.work/convoy: <convoy-name>` so secondary watches and the Convoy finalizer can key off it.

## Labels

### Reserved namespace

The `flotilla.work/` label prefix is **reserved for system use**. User-authored labels on `ProcessDefinition` (or any future workflow-template field) must not use it. Two-layer enforcement:

1. **Validation.** `WorkflowTemplate::validate` adds a `ReservedLabelKey { task, role, key }` error variant that fires when any `ProcessDefinition.labels` key starts with `flotilla.work/`. Templates carrying reserved keys are rejected at validation time (mirrors existing validation errors).
2. **Runtime guard.** `build_session_labels` applies user labels first, then system labels, so even if validation is bypassed the system labels win deterministically.

### Well-known keys

New shared module `crates/flotilla-resources/src/labels.rs` exports the constants:

```rust
pub const CONVOY_LABEL: &str = "flotilla.work/convoy";
pub const TASK_LABEL: &str = "flotilla.work/task";
pub const TASK_WORKSPACE_LABEL: &str = "flotilla.work/task_workspace";  // already used
pub const ROLE_LABEL: &str = "flotilla.work/role";                       // already used
pub const TASK_ORDINAL_LABEL: &str = "flotilla.work/task_ordinal";       // NEW
pub const PROCESS_ORDINAL_LABEL: &str = "flotilla.work/process_ordinal"; // NEW

pub const RESERVED_PREFIX: &str = "flotilla.work/";
```

(Delimiter note: existing labels mix underscores and hyphens — new labels use underscores; existing ones stay as-is.)

### Ordinals drive layout ordering

`TASK_ORDINAL_LABEL` and `PROCESS_ORDINAL_LABEL` carry the task's position in `WorkflowTemplateSpec.tasks` and the process's position in `TaskDefinition.processes`, zero-padded to fixed width so lexicographic sort matches numeric:

- `TASK_ORDINAL_LABEL: "003"` — third task in the workflow template
- `PROCESS_ORDINAL_LABEL: "001"` — second process in that task

The presentation reconciler sorts by `(task_ordinal, process_ordinal, session_name)`. The `DefaultPolicy` consumes that same sorted list. Both `spec_hash` and visible layout key off author-intent ordering — no dependence on backend list order.

### `ProcessDefinition` schema change

```rust
pub struct ProcessDefinition {
    pub role: String,
    #[serde(flatten)]
    pub source: ProcessSource,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,   // NEW — user labels; reserved prefix forbidden
}
```

Optional, defaults empty. No existing templates need to change.

### Label propagation in `task_workspace` reconciler

Extend the existing `TerminalSession` creation site to stamp (system labels applied LAST so they override any stray user value):

- `TASK_WORKSPACE_LABEL: <TaskWorkspace.metadata.name>` *(already present)*
- `ROLE_LABEL: <ProcessDefinition.role>` *(already present)*
- `CONVOY_LABEL: <TaskWorkspace.spec.convoy_ref>`
- `TASK_LABEL: <TaskWorkspace.spec.task>`
- `TASK_ORDINAL_LABEL: <zero-padded task index>`
- `PROCESS_ORDINAL_LABEL: <zero-padded process index>`
- Plus all entries from `ProcessDefinition.labels` (applied first, overwritten by system labels above)

A single helper (`build_session_labels` in `flotilla-controllers::reconcilers::task_workspace`) consolidates this so no caller forgets a key and the precedence order is guaranteed.

## `PresentationPolicy` (code-level)

```rust
pub trait PresentationPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn render(&self, processes: &[ResolvedProcess], context: &PolicyContext) -> RenderedWorkspace;
}

pub struct PolicyContext {
    pub name: String,
    pub presentation_local_cwd: ExecutionEnvironmentPath,   // multiplexer's own cwd; valid local path
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

Replicates today's `default_template()` behaviour over the **sorted** process list (already ordered by the reconciler):

1. Group processes by `role`, preserving the input order — i.e., ordered by the first appearance of each role in the sorted list.
2. One pane per role. Multiple processes for the same role → tabs inside the pane (overflow=tab, matching `build_pane_layout`).
3. First role → main pane; later roles → split right.
4. Emits `WorkspaceAttachRequest { template_yaml: None, attach_commands: Vec<(role, attach_command)>, working_directory: presentation_local_cwd, name }`. `PresentationManager.create_workspace` then walks its existing `resolve_template` → `build_pane_layout` path. Per-session cwds are already inside each `attach_command`.

Because the reconciler sorts by `(task_ordinal, process_ordinal, session_name)` before the policy sees the list, the "first appearance" in step 1 is deterministic and backend-independent. A single-task convoy renders identically to today's `.flotilla/workspace.yaml` default.

### Unknown policy name

Reconciler emits `PresentationStatusPatch::MarkFailed { message: "unknown presentation policy '{name}'" }`. No runtime invocation. Mirrors stage 4a's rejection of unsupported process sources.

### Working directory model — two distinct cwds

The presentation pipeline carries **two unrelated working directories**. They must not be conflated; today's `WorkspaceOrchestrator` already keeps them apart and the new pipeline preserves that.

- **Per-session cwd** (`TerminalSession.spec.cwd`). The directory the inner command runs in. May live inside a Docker container, on a remote host via SSH, or on the local host. The hop-chain resolver bakes this into the `attach_command` for each process — no consumer downstream of `ResolvedProcess.attach_command` needs to look at it again. Set by the task_workspace reconciler when it provisions the session.

- **Presentation-host local cwd** (`PresentationPlan.presentation_local_cwd`). The cwd the multiplexer process itself opens with — passed to tmux/zellij/cmux as `WorkspaceAttachRequest.working_directory`, which they use as the local pane cwd before the attach command runs. This **must be a valid local path on the presentation host**. It's not the per-session cwd (which may not exist locally — e.g. a docker mount path or remote checkout).

Today's `WorkspaceOrchestrator::attach_prepared_workspace` resolves this via a fallback chain: `repo_root` if locally present → `home_dir()` → `current_dir()` → `config_base`. The presentation reconciler uses **the same fallback chain** in `fetch_dependencies`. v1 does not try to discover a convoy-specific local repo path — `ConvoySpec.repository: { url }` is URL-level, not path-level, so there's no convoy-derived local root to use yet.

When the deferred stage-4a item "Convoy launched against an existing Checkout" lands, it brings a presentation-host-local checkout path with it, and the reconciler can prefer that over the home-dir fallback. No schema change to `Presentation` is needed for that — it's a one-line preference change in the reconciler.

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
    pub presentation_local_cwd: ExecutionEnvironmentPath,   // multiplexer's own cwd; valid local path
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
    terminal_sessions: TypedResolver<TerminalSession>,
    environments: TypedResolver<Environment>,
    hosts: TypedResolver<Host>,
    hop_chain: HopChainContext,   // encapsulates flotilla-core::hop_chain resolver + local_host + config_base
}

pub enum PresentationDeps {
    InSync,                      // sessions match observed_spec_hash; no action
    Applied(AppliedPresentation),
    TornDown,                    // sessions empty; tear_down succeeded (or there was nothing to tear down)
    Failed(String),
    UnknownPolicy(String),
}
```

### `fetch_dependencies`

1. `terminal_sessions.list_matching_labels(&spec.process_selector)`. No TaskWorkspace or Convoy read — session existence is the liveness signal (since the task_workspace cascade removes sessions when tasks complete).
2. For each matched session, resolve **per-session routing** (used by hop chain only):
   - `environments.get(&session.spec.env_ref)` → `host_ref` + `docker_container_id`
   - `hosts.get(host_ref)` → `HostName`
   - Per-session cwd for the hop chain = `session.spec.cwd` (set by the task_workspace reconciler when the session is provisioned). No Checkout read needed.
3. Build hop-chain plan per session → resolve to `attach_command` string via `flotilla-core::hop_chain` (same machinery `WorkspaceOrchestrator::resolve_prepared_commands_via_hop_chain` uses today). The `HopChainContext` bundles the SSH config base path, local `HostName`, and environment/terminal resolver construction. The per-session cwd is now baked into `attach_command` and not threaded any further.
4. Resolve `presentation_local_cwd` (the multiplexer's own cwd, distinct from any per-session cwd) via the fallback chain documented in "Working directory model": local repo root if known → `home_dir()` → `current_dir()` → `config_base`. v1 has no convoy-derived local repo root, so the chain effectively starts at `home_dir()`.
5. Sort the resolved process list by `(task_ordinal_label, process_ordinal_label, session_name)` — deterministic regardless of backend list order.
6. Compute `spec_hash = hash((policy_ref, sorted_process_list))` over the sorted list (role, command, labels).
7. **If the sorted process list is empty:**
   - If `status.observed_workspace_ref` is `Some` → `runtime.tear_down(manager, ws_ref)` → `Deps::TornDown`.
   - Else → `Deps::InSync` (nothing live, nothing to tear down).
8. **Else:**
   - If `status.observed_spec_hash == spec_hash` → `Deps::InSync`.
   - Else `runtime.apply(PresentationPlan { previous: status_derived, ... }).await`:
     - `Ok(applied)` → `Deps::Applied(applied)`.
     - `Err(msg)` if unknown policy → `Deps::UnknownPolicy(name)`.
     - `Err(msg)` otherwise → `Deps::Failed(msg)`.

### `reconcile`

Pure. Deps → status patch:

- `Deps::InSync` → `None`.
- `Deps::Applied(a)` → `Some(MarkActive { ... })`.
- `Deps::TornDown` → `Some(MarkTornDown)`.
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
        Box::new(LabelJoinWatch::<TerminalSession, Presentation> { label_key: CONVOY_LABEL, _marker: PhantomData }),
    ]
}
```

One watch. `TerminalSession` appearance, deletion, or label change fires the reconciler. Task completion shows up as session deletion (via the task_workspace cascade) — no separate TaskWorkspace or Convoy watch needed.

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

        // Delete the old workspace via the manager that CREATED it — not the current preferred.
        // If the preferred manager changed between apply() calls, a cross-manager replace is correct:
        // old teardown happens via old manager, new creation via current preferred.
        if let Some(prev) = &plan.previous {
            if let Some(old_mgr) = self.registry.presentation_managers.get(&prev.presentation_manager) {
                let _ = old_mgr.delete_workspace(&prev.workspace_ref).await;
            } else {
                // Old manager no longer configured. Log; the workspace is effectively leaked on that backend.
                tracing::warn!(manager = %prev.presentation_manager, ws = %prev.workspace_ref,
                    "previous presentation manager unavailable; old workspace may be leaked");
            }
        }

        let (new_manager_name, new_manager) = self.registry.presentation_managers.preferred_with_desc()
            .ok_or_else(|| "no presentation manager configured".to_string())?;
        let RenderedWorkspace { attach_request } = policy.render(&plan.processes, &PolicyContext {
            name: plan.name.clone(),
            presentation_local_cwd: plan.presentation_local_cwd.clone(),
        });
        let (ws_ref, _) = new_manager.create_workspace(&attach_request).await?;

        Ok(AppliedPresentation {
            presentation_manager: new_manager_name.to_string(),
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

Best-effort delete for the previous workspace means a partial failure (delete ok, create fails) leaves the Presentation without a live workspace but with the old `observed_*` fields cleared; next reconcile sees no observed workspace and attempts apply with `previous: None`. Documented as an accepted v1 limitation.

## Convoy Reconciler Extension

```rust
pub enum Actuation {
    // ... existing variants
    CreatePresentation { meta: InputMeta, spec: PresentationSpec },
    DeletePresentation { name: String },
    DeleteTaskWorkspace { name: String },          // NEW — per-task lifecycle tie-in
}
```

### Presentation creation/deletion on phase transitions

- Convoy transitions to `Active` with no existing Presentation (checked via label-indexed read or one-shot list) → emit `CreatePresentation` actuation with:
  - `meta.name = format!("{}-presentation", convoy.metadata.name)` (or similar — single deterministic derivation)
  - `meta.labels = { CONVOY_LABEL: convoy.metadata.name }`
  - `meta.owner_references = [OwnerReference::for(convoy)]`
  - `spec.convoy_ref = convoy.metadata.name`
  - `spec.presentation_policy_ref = "default"`
  - `spec.name = convoy.metadata.name`
  - `spec.process_selector = { CONVOY_LABEL: convoy.metadata.name }`
- Convoy transitions to `Completed` / `Failed` / `Cancelled` → emit `DeletePresentation { name }`.

The presentation creation site is the single swap point for future explicit-trigger creation policies.

### Convoy finalizer (new in stage 5)

Today `ConvoyReconciler::run_finalizer` is a no-op and `finalizer_name` returns `None`. That makes the stage 4a claim "TaskWorkspace stays alive until it cascades on Convoy deletion" aspirational — nothing actually implements cascade. Stage 5 closes this by adding:

- `finalizer_name` returns `Some("flotilla.work/convoy-teardown")`.
- `run_finalizer` enumerates owned resources by label and deletes them:
  - `presentations.list_matching_labels({CONVOY_LABEL: name})` → delete each
  - `task_workspaces.list_matching_labels({CONVOY_LABEL: name})` → delete each

Each deleted TaskWorkspace runs its own finalizer (above), which deletes its per-task Environment/Checkout/TerminalSessions. Each of those runs its own finalizer (existing stage 4a code), which performs the actual runtime teardown. A direct `DELETE convoys/<name>` now propagates all the way through the chain — no orphans.

The pair of finalizer semantics: phase transitions (Completed/Failed/Cancelled) + actuations remain the "natural" lifecycle path. The Convoy finalizer handles the explicit-delete-a-running-convoy path, which is otherwise a leak.

### Per-task TaskWorkspace lifecycle (stage 4a semantics change)

When the convoy reconciler observes a task transition to `Completed`, `Failed`, or `Cancelled`, it emits `DeleteTaskWorkspace { name }` for that task's TaskWorkspace.

### TaskWorkspace finalizer

Today `TaskWorkspaceReconciler::run_finalizer` is a no-op (`finalizer_name` returns `None`). Stage 5 adds:

- `finalizer_name` returns `Some("flotilla.work/task-workspace-teardown")`.
- `run_finalizer` enumerates children **by label selector**, not by status refs:
  - `terminal_sessions.list_matching_labels({TASK_WORKSPACE_LABEL: name})` → delete each
  - `checkouts.list_matching_labels({TASK_WORKSPACE_LABEL: name})` → delete each
  - `environments.list_matching_labels({TASK_WORKSPACE_LABEL: name})` → delete each

Using label selectors (rather than `status.environment_ref` / `status.checkout_ref`) is deliberate: stage 4a's TaskWorkspace only records those refs on `MarkReady`, so a partially-provisioned TaskWorkspace that failed before `MarkReady` would leak child resources if the finalizer read status. Labels are stamped when the children are created (already visible in existing `LabelMappedWatch::<Environment, TaskWorkspace>` wiring), so label-selector lookup finds them regardless of provisioning progress.

Each child resource (Environment, Checkout, TerminalSession) has its own existing finalizer that handles the actual runtime teardown (Docker destroy, shpool kill, etc.). The TaskWorkspace finalizer only does resource-level deletion; runtime effects chain through each child's finalizer.

Stage 4a's per-task placement model means each TaskWorkspace has its own Environment/Checkout/TerminalSessions, so blanket deletion by label is safe. The shared-environment placement variant (deferred from stage 4a) will need owner-list / reference-count semantics when it lands; stage 5's cleanup is explicitly scoped to per-task placement.

The disappearing sessions fire the presentation reconciler's selector watch, which recomputes membership and either replaces the workspace (new set of active-task sessions) or tears it down (no active tasks left).

This revises the stage 4a deferred item "Auto-cleanup of stopped sessions on terminal task transitions" — it's now partially resolved. Cleanup of sessions whose *inner command* crashed while the task is still Running remains deferred (that requires process-restart semantics).

## Testing

### Unit tests

- **Policy**: `DefaultPolicy::render` with one role, multiple roles, duplicate roles. Assert parity with `build_pane_layout` for equivalent inputs.
- **Label constants**: a trivial compile-time test that the expected keys exist (guards accidental renames).

### Reconciler tests (in-memory backend)

- Presentation with no matching sessions, no observed workspace → `Deps::InSync`, stays `Pending`, no runtime calls.
- Presentation with matching sessions → `apply` called once, status → `Active` with recorded hash, manager, ws_ref.
- Second reconcile, unchanged world → `Deps::InSync`, no runtime call, status unchanged.
- Session deleted (task completed → cascade) leaving some remaining → next reconcile recomputes, new hash, `apply` called with `previous` populated from current status, status updates.
- **All sessions deleted, observed workspace present** → `tear_down` called with observed manager + ws_ref, status → `TornDown` with `observed_*` cleared.
- From `TornDown`, sessions reappear → `apply` called with `previous: None`, status → `Active`.
- **Cross-manager replace**: simulate `prev.presentation_manager` differs from current preferred → `delete_workspace` goes to prev's manager, `create_workspace` goes to current preferred.
- **Previous manager no longer configured**: `apply` logs a warning, proceeds with create via current preferred (old workspace leaked on absent backend — asserted via recorded call list).
- **Deterministic ordering**: two sessions with `(task_ordinal=0, process_ordinal=1)` and `(task_ordinal=0, process_ordinal=0)` — regardless of list order, `apply` receives them sorted by process_ordinal.
- **Working-directory separation**: a session with `spec.cwd = "/workspace/repo"` (a docker-internal path that doesn't exist locally) → `attach_command` contains the path; `PresentationPlan.presentation_local_cwd` resolves to the home-dir fallback (a real local path). The two cwds are independent and don't leak into each other.
- Unknown policy → `Failed`, no runtime call.
- Finalizer on Presentation delete → `tear_down` called with recorded manager + ws_ref.

### Convoy reconciler tests

- Convoy → Active → `CreatePresentation` actuation emitted.
- Convoy → Active twice (re-reconcile) → only one Presentation created (idempotent).
- Convoy → Completed → `DeletePresentation { name }` emitted.
- **Per-task lifecycle**: task transitions to Completed → `DeleteTaskWorkspace { name }` emitted for that task's workspace; sibling tasks still Running do not have their workspaces deleted.
- **Convoy finalizer**: direct Convoy delete while Active → finalizer deletes Presentation + all labelled TaskWorkspaces; finalizer is removed and Convoy object deletes.

### TaskWorkspace finalizer tests

- **Fully-provisioned TaskWorkspace** delete → all labelled Environment/Checkout/TerminalSession resources deleted.
- **Partially-provisioned TaskWorkspace** (Environment created, MarkReady never reached, `status.environment_ref` is `None`) → finalizer still deletes the Environment because it's found by `TASK_WORKSPACE_LABEL` selector, not by status. Asserts no orphaned children.
- **Missing children** (already deleted by something else) → finalizer tolerates NotFound and completes successfully.

### Label propagation tests

Extension to existing `task_workspace_reconciler.rs`: created TerminalSessions carry `CONVOY_LABEL`, `TASK_LABEL`, `TASK_WORKSPACE_LABEL`, `ROLE_LABEL`, `TASK_ORDINAL_LABEL`, `PROCESS_ORDINAL_LABEL`, plus propagated `ProcessDefinition.labels`. A user label that collides with a reserved key gets overwritten by the system value (runtime guard).

### Validation tests

`WorkflowTemplate::validate` rejects `ProcessDefinition.labels` with a key starting with `flotilla.work/` — new `ReservedLabelKey` variant in `ValidationError`.

### Integration (optional, non-blocking)

`InProcessDaemon`-level test: create a single-task Convoy → wait for Presentation to reach `Active` → inspect recorded `PresentationRuntime` calls. Uses a mock `PresentationRuntime`. Validates wiring, not multiplexer behaviour.

### Live-multiplexer coverage

Deferred. The new `delete_workspace` method gets a focused replay-style test per implementation (cmux, tmux, zellij). Existing `create_workspace` replay coverage is unchanged.

## Scope Summary

Stage 5 ships:

1. Rename: `WorkspaceManager` → `PresentationManager` (trait + module + config key + registry field + impls).
2. `delete_workspace` method on `PresentationManager` trait + impl per multiplexer.
3. `Presentation` resource + reconciler + `PresentationRuntime` trait + `ProviderPresentationRuntime` impl + `PresentationPolicyRegistry` with `DefaultPolicy`.
4. `labels: BTreeMap<String, String>` on `ProcessDefinition`; reserved `flotilla.work/` prefix with `WorkflowTemplate::validate` enforcement.
5. `build_session_labels` helper in the task_workspace reconciler; `TASK_ORDINAL_LABEL` / `PROCESS_ORDINAL_LABEL` + existing labels.
6. Convoy reconciler extensions: `CreatePresentation` / `DeletePresentation` / `DeleteTaskWorkspace` actuations on phase transitions.
7. **Convoy finalizer** (new): deletes Presentation and all labelled TaskWorkspaces on direct Convoy delete.
8. **TaskWorkspace finalizer** (new): deletes child TerminalSession / Environment / Checkout resources **by label selector** (robust to partial provisioning).
9. Test coverage above.

## Out of Scope

- **Reconfigure-in-place** (stage boundary: PresentationPolicy variant + new trait methods).
- **`PresentationPolicy` as a CRD** (reify when Yeoman / multi-policy demand arrives).
- **Multiple presentation managers per Presentation** (registry still returns one preferred; cross-manager replace during `apply` works but a single Presentation doesn't span multiple managers simultaneously).
- **Convoy TUI pane** (stage 6).
- **TUI convoy view** (stage 6).
- **AttachableSet removal** (stage 7).
- **Label-selector-driven layout policies / signals-and-slots** (Yeoman-era).
- **Artifact resource** (future — selector shape extends naturally when it lands).
- **User-facing terminology changes** (TUI still says "workspace").
- **Explicit presentation-trigger UX** (auto-present-on-Active stays; single swap point in convoy reconciler).
- **Vessel lifecycle beyond existence** — the option-2 model ties TaskWorkspace existence to "task is active and presentable." Future workflows that bounce between states (e.g., dev → GPU runner → iOS tester) will want an independent "presentable" bit distinct from existence, so a task can be alive but hidden and later re-shown without losing state. Not addressed here.

## Open Risks

1. **Visible gap during replace.** Task transitions cause a brief workspace disappearance. For single-task convoys (all stage 4a can run end-to-end), the workspace is only replaced on task completion anyway, so the gap is invisible. Multi-task tool workflows will see it; acceptable for v1.
2. **Passthrough + multi-task is incompatible.** Passthrough terminal pools run processes inside the attached workspace; replace-on-change kills them on every task transition. Fine for single-task convoys. Multi-task passthrough compatibility arrives with reconfigure-in-place (no workspace teardown during transitions).
3. **Silent selector breakage.** A missed well-known label on a TerminalSession silently excludes it from the Presentation. Mitigation: `flotilla-resources::labels` constants + `build_session_labels` helper used uniformly by the task_workspace reconciler.
4. **Non-atomic replace.** Delete succeeds then create fails → Presentation has no live workspace but `observed_*` are cleared. Next reconcile sees no observed workspace and retries `apply` with `previous: None`. Acceptable for v1.
5. **Cross-manager replace in flight.** If a user changes the preferred presentation manager mid-flight, the next `apply` tears down the old via the old manager and creates new via current preferred — correct but briefly disorienting. If the old manager was removed from config entirely, the old workspace is leaked on that backend (logged). Rare, and no-backcompat phase tolerates it.

