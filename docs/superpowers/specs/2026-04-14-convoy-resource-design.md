# Convoy Resource and Controller — Design

## Context

Convoy is Stage 3 of the convoy implementation (see `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md`). A Convoy is a named workflow instance: it references a `WorkflowTemplate`, carries inputs, and tracks per-task state as the DAG advances.

Stage 3 ships the resource, a reconciliation controller that advances tasks through the DAG, and a runnable example binary against minikube. It deliberately stops at the "task becomes Ready" boundary — actual provisioning (Stage 4) is the first consumer of that state. Presentation, TUI, CLI, and the `PersistentAgent` / policy work all live in later stages.

## Crate

Lives in the existing `crates/flotilla-resources` crate alongside `WorkflowTemplate`. New `convoy` module. Replaces the existing stub CRD at `src/crds/convoy.crd.yaml`.

Stage 3 also makes a small revision to the Stage 1 resource-client surface — see "Resource-client revision: typed status patches" below.

## Resource-client revision: typed status mutations via `StatusPatch`

Multi-writer safety on `/status` needs documented optimistic-concurrency guarantees. Partial-patch transport on top of k8s is subtle: `application/merge-patch+json` has no documented precondition mechanism, `application/json-patch+json` needs an explicit `test` op, and Server-Side Apply brings its own managed-fields model. For Stage 3 we keep the transport simple — full-status replacement via `update_status`, which **is** documented to reject stale `resourceVersion` with 409 Conflict — and add a small layer above it that keeps mutations typed:

```rust
pub trait Resource: Send + Sync + 'static {
    type Spec: ...;
    type Status: ...;
    type StatusPatch: StatusPatch<Self::Status>;
    const API_PATHS: ApiPaths;
}

pub trait StatusPatch<S>: Send + Sync {
    /// Apply the patch directly to a status value.
    /// Used by every backend; the HTTP backend then calls update_status
    /// with the resulting full status.
    fn apply(&self, status: &mut S);
}
```

No new resolver method. Writers use the existing `update_status(name, rv, new_status)`, plus this read-modify-write helper that every controller loop needs anyway:

```rust
pub async fn apply_status_patch<T>(
    resolver: &TypedResolver<T>,
    name: &str,
    patch: &T::StatusPatch,
) -> Result<ResourceObject<T>, ResourceError>
where
    T: Resource,
    T::Status: Default,
{
    for _ in 0..MAX_RETRIES {
        let current = resolver.get(name).await?;
        let mut new_status = current.status.clone().unwrap_or_default();
        patch.apply(&mut new_status);
        match resolver
            .update_status(name, &current.metadata.resource_version, &new_status)
            .await
        {
            Ok(updated) => return Ok(updated),
            Err(ResourceError::Conflict { .. }) => continue,   // re-read, re-apply
            Err(other) => return Err(other),
        }
    }
    Err(ResourceError::Conflict { /* retry budget exhausted */ })
}
```

The `T::Status: Default` bound is placed on the helper only, not on the `Resource` trait. This keeps the trait unchanged and leaves the door open for resources that can't or don't want to implement `Default` (they can still use `update_status` directly). `ConvoyStatus` derives `Default` (see the type definition below); `WorkflowTemplate::Status = ()` trivially has `Default` but also cannot be patched (`NoStatusPatch` is uninhabited).

### Why this shape

- **Documented concurrency guarantees.** PUT with `resourceVersion` is k8s's explicit lost-update protection mechanism (API Concepts §"HTTP PUT to replace existing resource"). 409-on-stale-rv is part of the contract.
- **Typed vocabulary still runs the show.** `StatusPatch` variants name legitimate mutations; owner-scoped constructor modules gate who can build which variant; `apply(&mut status)` is the single authoritative mutator. The transport just carries the resulting full status.
- **Concurrent disjoint writers still compose correctly.** Two writers constructing different variants both read state, apply their patch, try to update; one wins, the other gets 409, re-reads, re-applies, writes. Because each variant's `apply` touches disjoint fields, the re-applied patch on the newer state is still semantically correct.
- **Concurrent same-field writers collide loudly.** Two external actors each marking a task Completed: one wins, the other retries on updated state (which now shows the task Completed) and either no-ops or reconciles its view.
- **One backend contract, no new transport primitives.** `update_status` and `get` already exist. The in-memory backend works out of the box; the HTTP backend doesn't need to learn any new verbs. Stage 1 revision is limited to adding the associated type + trait; no HTTP or in-memory changes.

### Resources with no status

`WorkflowTemplate` has `type Status = ()`. Its `StatusPatch` is uninhabited:

```rust
pub enum NoStatusPatch {}

impl StatusPatch<()> for NoStatusPatch {
    fn apply(&self, _: &mut ()) { match *self {} }
}

impl Resource for WorkflowTemplate {
    // ... as today
    type StatusPatch = NoStatusPatch;
}
```

Since `NoStatusPatch` has no variants, no caller can construct one, and `apply_status_patch::<WorkflowTemplate>` is compile-time unreachable. Stronger than a runtime "not supported" error.

### Why this over `serde_json::Value` patches

A call-site `Value` lets any caller write any status field. An associated enum makes the legitimate mutations a declared vocabulary: unknown mutations are compile errors. Ownership partitioning is enforced by owner-scoped constructor modules with private variant fields.

### Cost we accept

Full status replacement per write — every writer serialises and sends the whole `ConvoyStatus` back. At Stage 3 scale (low-write-rate controllers and small status bodies, a few KB), the wire overhead is immaterial. When/if it becomes a problem (Stage 7+ with AttachableSet migration and many external writers), we can upgrade to `application/json-patch+json` with an explicit `test` op on `/metadata/resourceVersion` — the typed `StatusPatch` vocabulary is already in place to back that transport. Server-Side Apply (`application/apply-patch+yaml` with managed-fields) is a further future option for much-larger-scale coordination.

## Scope

### In scope

- Rust `Convoy` type implementing `Resource`, with `ConvoySpec` / `ConvoyStatus` and the task state machine.
- Hand-written CRD YAML replacing the stub; namespaced, status subresource enabled, printer columns for `kubectl get cvy`.
- Pure `reconcile(convoy, spec, status, template, now) -> ReconcileOutcome` function.
- Example controller binary (`examples/convoy_controller.rs`) using list-then-watch + periodic resync.
- Table tests for `reconcile`, in-memory backend end-to-end test, HTTP backend integration test against minikube.
- Template snapshotting on first successful reconcile — the DAG is frozen into `convoy.status` at init.

### Out of scope (for this stage)

- Task provisioning, placement-policy resolution, container/environment creation.
- `PlacementPolicy` resource (Stage 4 or a sibling concern).
- `PersistentAgent` resource (future — houses Quartermaster, Yeoman, custom SDLC agents).
- Presentation / workspace integration (Stage 5).
- TUI / CLI surface (Stage 6+).
- Interactive launch UX (fetch template → auto-fill from context → approve).
- AttachableSet migration (Stage 7).
- Workflow composition (`includes`) and typed inputs — still deferred from Stage 2.

## Blue-sky Model (for orientation)

Stage 3's seams are designed around the following future split, captured here so the shape doesn't paint us in:

- **`WorkflowTemplate`** — shared, portable. *What to run, in what order.* Identical across deployment contexts.
- **`Convoy`** — workflow instance. *Which template, what inputs, which policy.*
- **`PlacementPolicy`** (future) — *where and how.* Named, with a default, possibly auto-discovered (today's `docker@host` style). Eventually delegates to or is implemented by a `PersistentAgent` (Quartermaster).
- **`PersistentAgent`** (future) — a single resource type with k8s-style labels/selectors. Conventional instances (Quartermaster, Yeoman, TestCoach, SecurityReviewer, …) are just labeled realizations. Agent runtime shape deliberately not committed: managed CLI (input-send), external CLI (shell-out), headless JSON/ACP, or internal LLM loop. All of them are presentable.
- **`PresentationManager`** (future) — scope-decoupled: full-flotilla / repo / convoy views.

Everything after `Convoy` is deferred. Stage 3's convoy carries an opaque `placement_policy: Option<String>` reference so Stage 4 can take over without a schema break.

## Resource Definition

### Rust

```rust
pub struct Convoy;
impl Resource for Convoy {
    type Spec = ConvoySpec;
    type Status = ConvoyStatus;
    const API_PATHS: ApiPaths = ApiPaths {
        group: "flotilla.work",
        version: "v1",
        plural: "convoys",
        kind: "Convoy",
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvoySpec {
    pub workflow_ref: String,                         // WorkflowTemplate name in same namespace
    #[serde(default)]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    pub placement_policy: Option<String>,             // opaque; Stage 4 resolves
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    String(String),
    // Future: Issue(IssueRef), IssueList(Vec<IssueRef>), Branch(BranchRef), ...
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConvoyStatus {
    pub phase: ConvoyPhase,

    /// Frozen at init from the referenced WorkflowTemplate. Holds the complete
    /// executable task definitions so Stage 4 can launch deterministically
    /// without re-reading the live template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_snapshot: Option<WorkflowSnapshot>,

    /// Per-task runtime state, keyed by task name. Definitions live in
    /// `workflow_snapshot.tasks`. `spec.inputs` is enforced immutable at
    /// the API layer (CRD CEL validations), so no snapshot of inputs is
    /// required — consumers can safely read `spec.inputs`.
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskState>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflows: Option<BTreeMap<String, String>>, // ref → resourceVersion
}

/// Snapshot of the referenced WorkflowTemplate's executable content at init.
/// Mirrors the subset of `WorkflowTemplateSpec` Stage 4 needs to launch tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub tasks: Vec<SnapshotTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotTask {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub processes: Vec<ProcessDefinition>, // re-exported from workflow_template
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConvoyPhase {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
}

impl Default for ConvoyPhase {
    fn default() -> Self { ConvoyPhase::Pending }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub phase: TaskPhase,
    /// Pending → Ready. Written by the convoy controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    /// Ready → Launching (actual provisioning start). Written by Stage 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Any terminal transition (Completed/Failed/Cancelled).
    /// Written by whoever drives the terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementStatus>,           // Stage 4 populates
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Placement metadata written by Stage 4's provisioning controller.
/// Shape is deferred; Stage 3 only reserves the field.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlacementStatus {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}
```

### YAML

```yaml
apiVersion: flotilla.work/v1
kind: Convoy
metadata:
  name: fix-bug-123
  namespace: flotilla
spec:
  workflow_ref: review-and-fix
  inputs:
    feature: "Retry logic for the poller"
    branch: "fix-bug-123"
  placement_policy: laptop-docker
status:
  phase: Active
  observed_workflow_ref: review-and-fix
  observed_workflows:
    review-and-fix: "42"
  workflow_snapshot:
    tasks:
      - name: implement
        depends_on: []
        processes:
          - role: coder
            selector: { capability: code }
            prompt: |
              Implement {{inputs.feature}} on branch {{inputs.branch}}.
          - role: build
            command: "cargo watch -x check"
      - name: review
        depends_on: [implement]
        processes:
          - role: reviewer
            selector: { capability: code-review }
            prompt: "Review branch {{inputs.branch}}."
  started_at: "2026-04-14T10:00:00Z"
  tasks:
    implement:
      phase: Running
      ready_at: "2026-04-14T10:00:00Z"
      started_at: "2026-04-14T10:00:05Z"
    review:
      phase: Pending
```

### Notes on shape

- **`observed_workflow_ref` + `observed_workflows`** are populated only after the controller successfully resolves the template and bootstraps task state. Callers watching "is this convoy actually tied to the template?" check status, not spec.
- **`observed_workflows` is a map**, not a single version field, so the future `includes` case (a workflow that pulls in other workflows) extends naturally — each snapshotted template gets an entry.
- **`workflow_snapshot` holds the complete task definitions** (names, deps, processes, selectors, prompts, commands) taken from the template at init. This is what Stage 4 reads when launching a task. The live template is never re-fetched after bootstrap. A snapshot is required because k8s doesn't permit retrieving past `resourceVersion`s of an object — `observed_workflows` records the version seen but is not a retrieval key.
- **Inputs are immutable at the API layer.** `spec.workflow_ref` and `spec.inputs` are locked by CRD CEL validations (`x-kubernetes-validations: self == oldSelf`) — the k8s API server rejects updates that change them. Consumers can safely read `spec.inputs` without fear of mid-flight change; no snapshot is needed.
- **Timestamps are single-writer.** `ready_at` is written by the convoy controller when a task transitions Pending→Ready. `started_at` is written by Stage 4 at Ready→Launching. `finished_at` is written by whoever drives the terminal transition (external actor, Stage 4 on launch failure, or the convoy controller during fail-fast cancellation).
- **`TaskState.placement`** is reserved for Stage 4. Stage 3 leaves it unset.
- **`ConvoyPhase::Cancelled`** is reserved for future user-initiated cancel; Stage 3 never produces it directly.
- **`InputValue` is untagged**, so today's YAML reads as plain scalars. When typed variants (`Issue`, `IssueList`, `Branch`) land, richer shapes slot in without a schema break.

## CRD YAML

Replaces `crates/flotilla-resources/src/crds/convoy.crd.yaml`. Namespaced, group `flotilla.work`, v1, status subresource enabled.

```yaml
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: convoys.flotilla.work
spec:
  group: flotilla.work
  scope: Namespaced
  names:
    plural: convoys
    singular: convoy
    kind: Convoy
    shortNames: [cvy]
  versions:
    - name: v1
      served: true
      storage: true
      subresources:
        status: {}
      additionalPrinterColumns:
        - name: Workflow
          type: string
          jsonPath: .spec.workflow_ref
        - name: Phase
          type: string
          jsonPath: .status.phase
        - name: Age
          type: date
          jsonPath: .metadata.creationTimestamp
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              required: [workflow_ref]
              properties:
                workflow_ref:
                  type: string
                  minLength: 1
                  x-kubernetes-validations:
                    - rule: "self == oldSelf"
                      message: "workflow_ref is immutable after creation"
                inputs:
                  type: object
                  additionalProperties: true
                  x-kubernetes-validations:
                    - rule: "self == oldSelf"
                      message: "inputs are immutable after creation"
                placement_policy: { type: string, minLength: 1 }
            status:
              type: object
              properties:
                phase:
                  type: string
                  enum: [Pending, Active, Completed, Failed, Cancelled]
                observed_workflow_ref: { type: string }
                observed_workflows:
                  type: object
                  additionalProperties: { type: string }
                workflow_snapshot:
                  type: object
                  x-kubernetes-preserve-unknown-fields: true
                  properties:
                    tasks:
                      type: array
                      items:
                        type: object
                        required: [name, processes]
                        properties:
                          name: { type: string, minLength: 1 }
                          depends_on:
                            type: array
                            items: { type: string }
                          processes:
                            type: array
                            items:
                              type: object
                              x-kubernetes-preserve-unknown-fields: true
                message: { type: string }
                started_at: { type: string, format: date-time }
                finished_at: { type: string, format: date-time }
                tasks:
                  type: object
                  additionalProperties:
                    type: object
                    required: [phase]
                    properties:
                      phase:
                        type: string
                        enum: [Pending, Ready, Launching, Running, Completed, Failed, Cancelled]
                      ready_at: { type: string, format: date-time }
                      started_at: { type: string, format: date-time }
                      finished_at: { type: string, format: date-time }
                      message: { type: string }
                      placement:
                        type: object
                        x-kubernetes-preserve-unknown-fields: true
```

- `subresources.status: {}` enables the `/status` subresource so status patches don't contend with spec edits.
- `inputs.additionalProperties: true` keeps the schema open for future typed `InputValue` variants. Rust holds the real shape.
- `placement` uses `x-kubernetes-preserve-unknown-fields: true` so Stage 4 can populate arbitrary metadata without a CRD bump.
- **CEL validations** (`x-kubernetes-validations`) enforce immutability of `workflow_ref` and `inputs` at the API server. Requires k8s 1.25+ (stable in 1.30+); minikube defaults are fine. A cluster without CEL support would need an admission webhook for equivalent enforcement — deferred.

## Reconciliation

### ConvoyStatusPatch enum

All status mutations pass through a single typed vocabulary:

```rust
pub enum ConvoyStatusPatch {
    /// First successful reconcile: snapshot the template, initialize task map.
    Bootstrap {
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        tasks: BTreeMap<String, TaskState>,  // all Pending
        phase: ConvoyPhase,                  // typically Pending or Active
        started_at: Option<DateTime<Utc>>,
    },

    /// Bootstrap-time fatal error (template not found, template invalid,
    /// missing input). Convoy terminal.
    FailInit { phase: ConvoyPhase /* = Failed */, message: String, finished_at: DateTime<Utc> },

    /// Convoy-controller transitions: advance 0+ tasks Pending→Ready.
    AdvanceTasksToReady { ready: BTreeMap<String, DateTime<Utc>> },

    /// Fail-fast: a task is Failed; cancel 0+ non-terminal siblings, roll up
    /// convoy to Failed.
    FailConvoy {
        cancelled_tasks: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },

    /// Phase roll-up: set convoy phase + optionally started_at/finished_at.
    RollUpPhase { phase: ConvoyPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>> },

    /// Stage 4 (defined in shape; no Stage 3 code produces them):
    TaskLaunching { task: String, started_at: DateTime<Utc>, placement: PlacementStatus },
    TaskRunning   { task: String },

    /// External-actor terminal transitions:
    MarkTaskCompleted { task: String, finished_at: DateTime<Utc>, message: Option<String> },
    MarkTaskFailed    { task: String, finished_at: DateTime<Utc>, message: String },
    MarkTaskCancelled { task: String, finished_at: DateTime<Utc> },
}
```

`impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch` implements `apply(&mut status)`. Writers then call `apply_status_patch(resolver, name, &patch)`, which reads the current object, applies the patch to a clone, and writes via `update_status` with `resourceVersion` as precondition (retrying on 409). Ownership partitioning is enforced by owner-scoped constructor modules (details in implementation); Stage 3 code only constructs `Bootstrap`, `FailInit`, `AdvanceTasksToReady`, `FailConvoy`, `RollUpPhase`.

### Pure reconcile function

```rust
pub fn reconcile(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome;

pub struct ReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>, // None = no change
    pub events: Vec<ConvoyEvent>,         // observability only
}

pub enum ConvoyEvent {
    PhaseChanged       { from: ConvoyPhase, to: ConvoyPhase },
    TaskPhaseChanged   { task: String, from: TaskPhase, to: TaskPhase },
    TemplateNotFound   { name: String },
    TemplateInvalid    { name: String, errors: Vec<workflow_template::ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput       { name: String },
}
```

`ConvoyEvent` is observability only — the watch loop logs via `tracing`; events are not persisted in the resource. Future addition may emit k8s `Event` resources.

Pure, no I/O. The watch loop reads the convoy (and the live template on first resolve only), calls `reconcile`, applies the returned patch via `apply_status_patch`. Tests drive it directly.

### Reconcile steps (single pass)

Reconcile is a pure decision function: given the current convoy (and template on init), produce zero or one `ConvoyStatusPatch`. The watch loop applies whatever patch is returned.

1. **`workflow_ref` change guard (post-init).**
   - If `status.observed_workflow_ref` is set and `spec.workflow_ref` differs — the CRD's CEL validation should have prevented this, but handle defensively — emit `WorkflowRefChanged`, produce `FailInit { phase: Failed, message: "workflow_ref changed after init; not supported" }`.

2. **Bootstrap (`status.observed_workflow_ref` unset).**
   - Look up the template by `spec.workflow_ref`.
     - **Not found** → emit `TemplateNotFound`, produce `FailInit { phase: Failed, message: "WorkflowTemplate '<ref>' not found" }`.
     - **Found but fails `workflow_template::validate()`** → emit `TemplateInvalid { errors }`, produce `FailInit { phase: Failed, message: "WorkflowTemplate '<ref>' is invalid: <summary>" }`. This is the Stage-2-mandated consumer revalidation.
     - **Found and valid** → continue.
   - **Input completeness check**: every declared template input has a value in `spec.inputs`. Missing → emit `MissingInput { name }`, produce `FailInit { phase: Failed, message: "missing input '<name>'" }`. Extra inputs (in spec but not declared) → informational event only.
   - **Produce `Bootstrap`**: snapshot the full task definitions into `workflow_snapshot`, initialize `tasks` with every template task at `TaskPhase::Pending` (no `ready_at`), record `observed_workflow_ref` and `observed_workflows`, set convoy `phase = Pending` (or `Active` directly if step 4 would transition any task this pass — compute inside the same outcome).

3. **Post-init: compute tasks that should transition Pending → Ready.**
   - For each `Pending` task whose every `depends_on` entry maps to a task in `Completed`: include in an `AdvanceTasksToReady` patch with `ready_at = now`.

4. **Fail-fast.**
   - If any task is `TaskPhase::Failed`: compute set of non-terminal siblings (not `Completed`/`Failed`/`Cancelled`). Produce `FailConvoy { cancelled_tasks: <siblings → now>, finished_at: now, message }`. The failed task itself retains its `finished_at` from whoever marked it Failed.

5. **Phase roll-up.**
   - All tasks `Completed` → produce `RollUpPhase { phase: Completed, finished_at: Some(now), started_at: None }`.
   - Any task past `Pending` but no terminal convoy state, and current `phase` is still `Pending` → produce `RollUpPhase { phase: Active, started_at: Some(now), finished_at: None }`.
   - Otherwise no phase roll-up.

**One patch per reconcile — priority-driven, multi-pass convergence.**

Each reconcile returns at most one `ConvoyStatusPatch`. When multiple transitions are eligible in a single pass, pick the one highest in this priority order:

1. `FailConvoy` — fail-fast dominates everything else.
2. `FailInit` — init-time failures are terminal and take precedence over advancement.
3. `Bootstrap` — init before any advancement.
4. `AdvanceTasksToReady` — DAG advancement before phase roll-up.
5. `RollUpPhase` — lowest priority; only when no structural change is pending.

Each successful patch write bumps the convoy's `resourceVersion`, which produces a watch event that triggers the next reconcile; the controller converges in multiple passes. Two common cases:

- *Last task completes, convoy should now roll to Completed.* Reconcile sees all tasks terminal, emits `RollUpPhase { phase: Completed, finished_at }`. One patch.
- *Root tasks become Ready immediately after bootstrap.* Two reconciles: first emits `Bootstrap`, second (triggered by the bootstrap status write) emits `AdvanceTasksToReady`.

This is standard k8s controller-loop behavior. No composite variants; no claims of single-atomic-multi-transition updates.

### Post-init behavior and user edits

Because CRD CEL validations lock `spec.workflow_ref` and `spec.inputs` post-creation, the controller never needs to re-validate them on each reconcile. Any reconcile after Bootstrap simply reads `status.workflow_snapshot` and `status.tasks` for DAG work.

### Watch loop (example binary)

```rust
async fn run(backend: &ResourceBackend, namespace: &str) -> Result<()> {
    let convoys = backend.using::<Convoy>(namespace);
    let templates = backend.using::<WorkflowTemplate>(namespace);

    // Catch-up: list then watch from the collection resourceVersion.
    let list = convoys.list().await?;
    for convoy in &list.items {
        reconcile_and_apply(&convoys, &templates, convoy).await?;
    }
    let mut events = convoys.watch(WatchStart::FromVersion(list.resource_version)).await?;

    let mut resync = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            Some(event) = events.next() => { reconcile_from_event(&convoys, &templates, event?).await?; }
            _ = resync.tick() => { resync_all(&convoys, &templates).await?; }
        }
    }
}
```

- **List-then-watch** (`WatchStart::FromVersion(collection_rv)`) ensures no gap if the controller starts after convoys already exist. `WatchStart::Now` would miss pre-existing convoys — wrong for a controller.
- **Templates are not watched.** Once snapshotted, they are read only at convoy init. Template edits do not affect running convoys. This removes a whole class of "what if the template changes under me?" failure modes.
- **Periodic resync** (~60s) guards against missed events / cache drift. Standard k8s controller pattern.

### Conflict handling

`update_status` returns `ResourceError::Conflict` if `resourceVersion` is stale. `apply_status_patch` re-reads the object, re-applies the patch to the new state, and retries (bounded at 3 attempts). If the retry budget is exhausted, the helper returns `Conflict` — the next watch event or resync tick will produce a fresh reconcile.

### Ownership contract — enforced by the patch vocabulary

Ownership is expressed in the `ConvoyStatusPatch` enum, not in prose. Each writer constructs only the variants that correspond to its owned transitions. Variant construction is gated by owner-scoped constructor modules (private variant fields; public constructor functions in the owning module), so misuse is a compile error rather than a convention.

| Writer | Variants it constructs |
|--------|-----------------------|
| Convoy controller (Stage 3) | `Bootstrap`, `FailInit`, `AdvanceTasksToReady`, `FailConvoy`, `RollUpPhase` |
| Provisioning controller (Stage 4) | `TaskLaunching`, `TaskRunning` |
| External actors (CLI, TUI, agent-side CLI) | `MarkTaskCompleted`, `MarkTaskFailed`, `MarkTaskCancelled` |

Each variant's `apply(&mut ConvoyStatus)` touches a disjoint subset of fields — the partition is visible in code, not just documented. Two concurrent writers constructing different variants collide on `resourceVersion` in `update_status`; the loser retries by re-reading the updated base and re-applying its patch. Because the two variants touch disjoint fields, the re-applied patch on the newer state is still semantically correct and the writers compose. Concurrent writers constructing variants that touch the same field (e.g. two external actors both marking a task Completed) also collide on `resourceVersion`; the retry sees the task already in the terminal state and the second write is either a no-op or converges to the same value.

## Tests

### Table tests (pure `reconcile`)

- Fresh convoy, template found + valid → returns `Bootstrap` with full `workflow_snapshot`, all tasks Pending, `observed_workflow_ref` and `observed_workflows` set.
- Template not found → returns `FailInit` with clear message; event `TemplateNotFound`.
- **Template invalid** (fails `workflow_template::validate()`) → returns `FailInit`; event `TemplateInvalid` carrying the validation errors.
- Missing input → returns `FailInit`; event `MissingInput`.
- Extra input (not declared) → informational event only; no failure patch.
- All deps satisfied on a Pending task → returns `AdvanceTasksToReady` with `ready_at = now`.
- Fan-out: three tasks with no deps → a single `AdvanceTasksToReady` carries all three.
- Fan-in: A→C, B→C, A=Completed, B=Running → C stays Pending. B completes → next reconcile returns `AdvanceTasksToReady` for C.
- One task Failed → returns `FailConvoy` with all non-terminal siblings in `cancelled_tasks`.
- All tasks Completed → returns `RollUpPhase { phase: Completed, finished_at: Some(now) }`.
- `spec.workflow_ref` changed after init (defensive, post-CEL) → returns `FailInit` with the "workflow_ref changed" message.
- Template refetch does not happen after init (verify by passing `None` for template on second call after snapshot; reconcile returns a DAG-advancement patch from status alone).

### `StatusPatch::apply` unit tests

For every `ConvoyStatusPatch` variant: construct a representative starting `ConvoyStatus`, apply the patch, assert the resulting fields match expectation. No cross-serialiser parity test needed — since there is only one serialiser path (`apply` → full status → `update_status`), there is no second implementation to disagree with.

### `apply_status_patch` retry behavior

- Simulated conflict: resolver returns `Conflict` on first write; helper re-reads and retries; second attempt succeeds. Assert the final state reflects the patch applied to the updated base.
- Retry budget exhausted: resolver returns `Conflict` repeatedly; helper returns `ResourceError::Conflict` after `MAX_RETRIES`. Assert the caller sees the error (and can back off / wait for the next watch tick).

### In-memory backend end-to-end

- Create `WorkflowTemplate` + `Convoy` in the in-memory backend.
- Run the controller loop against simulated `MarkTaskCompleted` patches (applied via `apply_status_patch`) that drive tasks through completion.
- Assert the observed sequence of convoy phases and task phases matches expectation.

### HTTP backend integration (minikube, gated)

- Apply both CRDs. Confirm CEL validations reject edits to `spec.workflow_ref` and `spec.inputs` (one negative test per field).
- Create a WorkflowTemplate with a two-task DAG (`implement` → `review`).
- Create a Convoy referencing it.
- Run the example controller binary in a background task.
- Drive `tasks.implement.phase = Completed` via `apply_status_patch(MarkTaskCompleted)`; assert `review` moves to Ready.
- Drive `tasks.review.phase = Completed`; assert convoy `phase = Completed`.

## Example Binary

`crates/flotilla-resources/examples/convoy_controller.rs`:

- Accepts `--namespace` flag, defaults to `flotilla`.
- Bootstraps CRDs via `ensure_crd`.
- List-then-watch loop as above.
- Structured logging with `tracing` matching the codebase style.
- Runs against minikube by default via `HttpBackend::from_kubeconfig`.

## Deliverables

### Stage 1 revision (as part of Stage 3 work)

1. `Resource::StatusPatch` associated type and `StatusPatch` trait (`apply(&mut Status)` only — no transport method).
2. `NoStatusPatch` uninhabited enum; `WorkflowTemplate` adopts it (trivial existing-crate revision).
3. `apply_status_patch::<T>(resolver, name, patch)` helper function implementing read-modify-write + conflict retry on top of the existing `get` + `update_status`.

No HTTP or in-memory backend changes are required — the transport primitives stay as-is.

### Stage 3 proper

4. `Convoy` Rust type and `Resource` impl.
5. `ConvoySpec`, `ConvoyStatus`, `ConvoyPhase`, `TaskState`, `TaskPhase`, `InputValue`, `PlacementStatus`, `WorkflowSnapshot`, `SnapshotTask` types.
6. `ConvoyStatusPatch` enum and its `StatusPatch<ConvoyStatus>` impl.
7. Owner-scoped constructor modules (`controller_patches`, `provisioning_patches`, `external_patches`) gating variant construction by visibility.
8. Pure `reconcile(convoy, template, now) -> ReconcileOutcome` function.
9. `ReconcileOutcome`, `ConvoyEvent` types.
10. Convoy CRD YAML with CEL immutability validations (replaces the stub).
11. Table tests for reconcile.
12. `StatusPatch::apply` unit tests per variant.
13. `apply_status_patch` retry-behavior tests.
14. In-memory backend end-to-end test.
15. HTTP backend integration test against minikube, including CEL-rejection checks.
16. `examples/convoy_controller.rs` — runnable controller binary.

No provisioning, no policy resolution, no presentation, no CLI surface beyond what the example needs.

## Design Decisions

### Tasks as convoy sub-status, not independent resources

One Convoy resource carrying a map of task states, versus a separate `ConvoyTask` resource per task. Per the design doc, sub-status is simpler for v1: no resource-per-task proliferation, no cross-resource watches. Promotion to independent resources is a well-understood migration (reachable later if we need per-task independent watches).

### Full template snapshot at init; no template watching

The snapshot includes the complete executable content (task names, deps, processes, selectors, prompts, commands) — not just the DAG structure. Stage 4 reads `status.workflow_snapshot` to launch tasks; the live template is never re-fetched.

This is *required*, not merely defensive: k8s does not permit retrieving a past `resourceVersion` of an object. `observed_workflows: {ref: version}` records what was seen, but is not a retrieval key — the controller can't go back and read "template foo at version 42" later. The snapshot is the only durable record of what was authorised to run.

Cascading template edits into running convoys would produce too many failure modes even if retrievability weren't a problem (task renames, dep reshapes, removed tasks all break observed state). Snapshotting once at init removes the concern.

Re-running a convoy with a newer template version is a future primitive (copy convoy, reset status, re-snapshot). If flotilla-cp gains versioned history and immutable-by-convention templates, the snapshot may become redundant for that deployment — still required against raw k8s.

### `observed_workflows` as a map

Single-entry today (root → resourceVersion). When workflow composition (`includes`) lands, every snapshotted template — root plus includes — gets an entry. Naming the field as a map now avoids a schema change later.

### Typed status mutations (`StatusPatch` associated type), full-status transport

Multi-writer safety on `/status` needs optimistic-concurrency guarantees. K8s documents precondition behavior for PUT-with-resourceVersion (409 on stale); for PATCH, the merge-patch content type has no documented precondition, json-patch requires an explicit `test` op, and Server-Side Apply has its own managed-fields model. For Stage 3 we keep the transport simple and stay on documented ground — every writer does a full `update_status` with the current resourceVersion as precondition. The "I forgot to preserve the field you just wrote" risk is avoided by structure rather than by wire protocol: writers read the current status, call `StatusPatch::apply(&mut cloned_status)` to mutate only their owned fields, then `update_status` with the result. Two concurrent disjoint writers re-read each other's writes on conflict-retry and compose correctly.

`Resource::StatusPatch` makes legitimate mutations a declared vocabulary; owner-scoped constructor modules gate variant construction so "only the convoy controller can advance tasks to Ready" is a compile-time property, not a comment.

Trade-off accepted: every status write serialises the full status body. At Stage 3 scale this is fine. When it isn't, we can upgrade to json-patch with `test` op (the `StatusPatch` vocabulary survives the transport change), or Server-Side Apply for much larger scale.

The alternative of a spec-side command queue (`spec.task_actions: [{task, action: Complete}]`) was rejected: controllers normally don't mutate their own spec, "mark complete" is an event not desired state, and there's no real gain over typed status patches applied via `update_status`.

### List-then-watch on the controller

`WatchStart::Now` would miss convoys that exist before the controller starts. The Stage 1 API was designed exactly for the list-then-watch pattern (collection resourceVersion → `WatchStart::FromVersion`) — use it for any controller that cares about pre-existing state.

### Placement as an opaque field

`TaskState.placement` is present in the schema so Stage 4 has a place to write, but its shape is not modelled in Stage 3 (`BTreeMap<String, serde_json::Value>` + `x-kubernetes-preserve-unknown-fields`). This lets Stage 4 iterate the placement model without CRD bumps. Stage 3 never writes to it.

### `placement_policy` on spec, not per-task

A single policy reference on the convoy, rather than per-task placement overrides inline in the convoy spec. Rationale:

- The policy (future `PlacementPolicy` resource) is the thing that decides per-task details, possibly delegating to a Quartermaster agent.
- Inline per-task overrides would duplicate what a policy controls, and make every consumer re-implement resolution logic.
- Launch-time override is expressed by writing a different policy into `spec.placement_policy` — it *is* the override. No separate override channel is needed.

### `ConvoyPhase::Cancelled` reserved, not produced in Stage 3

User-initiated convoy cancel is a real future feature but adds a control-plane verb (patch spec flag? delete convoy and let finalizer GC?) that deserves its own design round. Stage 3 reserves the phase so consumers can pattern-match today without later breaking them.

### Spec immutability via CRD CEL validations, not a snapshot

Earlier drafts proposed `input_snapshot` to freeze `spec.inputs` against user edits. The correct mechanism is CRD `x-kubernetes-validations` with transition rules (`self == oldSelf`), which make the API server reject mutating requests — the field is actually immutable, not "effectively immutable via a snapshot." Applies to `spec.workflow_ref` and `spec.inputs`. The `workflow_snapshot` remains because the *template*'s content isn't frozen by any validation rule (templates are meant to be edited) — only the convoy's reference to it is.

## Deferred Items (captured in `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md`)

To add under "From Stage 3":

- **`PlacementPolicy` resource** — named, default, auto-discovered; delegates to or is implemented by a `PersistentAgent`. Stage 3 references by opaque string; Stage 4 reifies.
- **`PersistentAgent` resource** — one resource type with k8s-style labels/selectors. Quartermaster, Yeoman, TestCoach, etc. are conventionally-labeled instances. Agent runtime shape deliberately open (managed CLI, external CLI, headless JSON/ACP, internal LLM loop).
- **Presentation scope decoupling** — `PresentationManager` at full-flotilla / repo / convoy scopes.
- **Interactive launch UX** — CLI/TUI flow: fetch template → infer inputs from context (current branch, selected issues) → present for approval → create convoy.
- **Typed `InputValue` variants** — `Issue`, `IssueList`, `Branch`, `ChangeRequest`, etc. Requires matching `InputDefinition.kind` in WorkflowTemplate (Stage 2 revision).
- **Label-based workflow discovery** — e.g. `flotilla.work/accepts: issue` on WorkflowTemplate, for UI surfacing based on user selection context. May be subsumed by typed inputs.
- **Workflow composition (`includes`)** — sub-workflows, transitive snapshotting into `observed_workflows`.
- **Template versioning** — `spec.workflow_ref_revision` for convoys that want a specific template version.
- **Convoy re-run** — copy a convoy, reset status, re-snapshot against newer template.
- **Convoy cancellation** — user-initiated cancel producing `ConvoyPhase::Cancelled`.
- **Admission webhook / fast-feedback validation** — complements the client-side Convoy validator once shared-cluster workflows demand it; also a fallback for clusters without CEL support (k8s < 1.25).
- **Immutable-by-convention templates** — if flotilla-cp introduces versioned-history semantics (retrievable by `resourceVersion`) plus admission enforcement against in-place template edits, `workflow_snapshot` becomes redundant for that deployment and convoys could just reference `(ref, version)`.
- **Controller deployment and leader election.** Stage 3 runs the controller as a standalone example binary — a single process. The intended trajectory, deliberately deferred:
  - (a) Every controller uses a k8s `Lease` resource for leader election. Exactly one replica is active; others stand by.
  - (b) The regular `flotilla` binary carries controllers embedded, activated by default, trying to claim the lease unless explicitly disabled. Single-process flotilla installs get controllers for free.
  - (c) "Cluster-native" mode schedules N flotilla daemon pods into the cluster, all competing for the same leases — standard k8s HA pattern.
  - (d) Open problem: when flotilla-cp itself is embedded across multiple daemons, the Lease mechanism can't elect the leader flotilla-cp — leases depend on the API server whose implementation is what needs electing. Solved separately (consensus, external coordinator, static leader, etc.); not a convoy-controller concern.
