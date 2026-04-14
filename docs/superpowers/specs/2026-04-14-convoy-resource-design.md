# Convoy Resource and Controller — Design

## Context

Convoy is Stage 3 of the convoy implementation (see `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md`). A Convoy is a named workflow instance: it references a `WorkflowTemplate`, carries inputs, and tracks per-task state as the DAG advances.

Stage 3 ships the resource, a reconciliation controller that advances tasks through the DAG, and a runnable example binary against minikube. It deliberately stops at the "task becomes Ready" boundary — actual provisioning (Stage 4) is the first consumer of that state. Presentation, TUI, CLI, and the `PersistentAgent` / policy work all live in later stages.

## Crate

Lives in the existing `crates/flotilla-resources` crate alongside `WorkflowTemplate`. New `convoy` module. Replaces the existing stub CRD at `src/crds/convoy.crd.yaml`.

Stage 3 also makes a small revision to the Stage 1 resource-client surface — see "Resource-client revision: typed status patches" below.

## Resource-client revision: typed status patches

To let multiple actors (convoy controller, future provisioning controller, external CLI/TUI) mutate disjoint parts of a convoy's status without overwriting each other, full-status replacement (`update_status`) is insufficient. Stage 3 adds a typed patch primitive to the Stage 1 API:

```rust
pub trait Resource: Send + Sync + 'static {
    type Spec: ...;
    type Status: ...;
    type StatusPatch: StatusPatch<Self::Status>;
    const API_PATHS: ApiPaths;
}

pub trait StatusPatch<S>: Send + Sync {
    /// Apply the patch directly to an in-memory status value.
    /// Used by the in-memory backend and by a future in-process
    /// flotilla-cp that keeps state in Rust structures, not JSON.
    fn apply(&self, status: &mut S);

    /// Serialise to a JSON merge-patch (RFC 7396) body.
    /// Used by the HTTP backend against real k8s.
    fn to_merge_patch(&self) -> serde_json::Value;
}

// New resolver method:
impl<T: Resource> TypedResolver<T> {
    async fn patch_status(
        &self,
        name: &str,
        resource_version: &str,
        patch: &T::StatusPatch,
    ) -> Result<ResourceObject<T>, ResourceError>;
}
```

### Dispatch

- **HTTP backend**: `PATCH /apis/.../<kind>/<name>/status` with `Content-Type: application/merge-patch+json`, body from `patch.to_merge_patch()`, and `resourceVersion` as an optimistic-concurrency precondition (`?resourceVersion=...` query — k8s honors this for PATCH).
- **In-memory backend**: look up the stored status under the resource lock, check `resource_version`, call `patch.apply(&mut status)`, bump the stored version.
- **Full `update_status` stays.** It's still the right tool for create-time and bootstrap paths where "the whole status is new" is the shape.

### Resources with no status

`WorkflowTemplate` has `type Status = ()`. Its `StatusPatch` is uninhabited:

```rust
pub enum NoStatusPatch {}

impl StatusPatch<()> for NoStatusPatch {
    fn apply(&self, _: &mut ()) { match *self {} }
    fn to_merge_patch(&self) -> Value { match *self {} }
}

impl Resource for WorkflowTemplate {
    // ... as today
    type StatusPatch = NoStatusPatch;
}
```

`patch_status` exists on the resolver for every `T`, but no caller can construct a `NoStatusPatch` — the method is compile-time unreachable for status-less resources. Stronger than a runtime "not supported" error.

### Why this over `serde_json::Value` patches

A call-site-constructed `Value` lets any caller write any status field. An associated enum makes the legitimate mutations a declared vocabulary: unknown mutations are compile errors. Ownership partitioning can be further enforced by gating variant construction behind owner-scoped constructor modules with private variant fields. The in-memory backend avoids implementing JSON merge-patch semantics (subtle: null-as-remove, array replacement, nested merging).

### Parity test

Every `StatusPatch` variant gets a round-trip test: apply the patch via `apply(&mut status)`, serialise a clone of the starting status to JSON, apply `to_merge_patch()` via a JSON merge-patch library (`json-patch` crate, RFC 7396), assert the resulting states match. Keeps the two serialisers provably in sync.

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

`impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch` implements both `apply(&mut status)` and `to_merge_patch()`. Ownership partitioning is enforced by owner-scoped constructor modules (details in implementation); Stage 3 code only constructs `Bootstrap`, `FailInit`, `AdvanceTasksToReady`, `FailConvoy`, `RollUpPhase`.

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

Pure, no I/O. The watch loop reads the convoy (and the live template on first resolve only), calls `reconcile`, applies the returned patch via `patch_status`. Tests drive it directly.

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

**Patch aggregation.** At most one `ConvoyStatusPatch` is returned per reconcile. In the common case (e.g. a task just completed, unblocking two dependents, and that was the last work so convoy rolls to Completed), the patch is a composite — typically `AdvanceTasksToReady` combined with `RollUpPhase`. Rather than chain multiple patches, the variants themselves contain enough information to express a single-pass transition, and the watch loop gets one atomic status update per reconcile.

In practice this is implemented as: produce the structurally most "outer" variant (e.g. `FailConvoy` dominates `RollUpPhase`), and let the next reconcile run handle anything that requires observing the previous patch's effect. K8s-style controllers converge via multiple passes; we don't need one-shot atomicity.

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

`patch_status` returns `ResourceError::Conflict` if `resourceVersion` is stale. The controller re-fetches the convoy and retries up to a bounded number of times (3). If still conflicting, drop — the next watch event or resync tick will re-reconcile.

### Ownership contract — enforced by the patch vocabulary

Ownership is expressed in the `ConvoyStatusPatch` enum, not in prose. Each writer constructs only the variants that correspond to its owned transitions. Variant construction is gated by owner-scoped constructor modules (private variant fields; public constructor functions in the owning module), so misuse is a compile error rather than a convention.

| Writer | Variants it constructs |
|--------|-----------------------|
| Convoy controller (Stage 3) | `Bootstrap`, `FailInit`, `AdvanceTasksToReady`, `FailConvoy`, `RollUpPhase` |
| Provisioning controller (Stage 4) | `TaskLaunching`, `TaskRunning` |
| External actors (CLI, TUI, agent-side CLI) | `MarkTaskCompleted`, `MarkTaskFailed`, `MarkTaskCancelled` |

Each variant's `apply(&mut ConvoyStatus)` touches a disjoint subset of fields — the partition is visible in code, not just documented. Two concurrent writers constructing different variants produce non-overlapping mutations and compose cleanly under merge-patch. Concurrent writers constructing variants that touch the same field (e.g. two external actors both marking a task Completed) collide on `resourceVersion` and retry.

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

### StatusPatch apply/merge-patch parity

For every `ConvoyStatusPatch` variant:

1. Construct a representative starting `ConvoyStatus`.
2. Apply the patch via `apply(&mut status_a)`.
3. Serialise a clone of the starting status to JSON, apply `patch.to_merge_patch()` using the `json-patch` crate's RFC 7396 merge-patch implementation, deserialise back into `ConvoyStatus`.
4. Assert `status_a == status_b`.

Keeps the in-memory and HTTP serialisers provably in sync.

### In-memory backend end-to-end

- Create `WorkflowTemplate` + `Convoy` in the in-memory backend.
- Run the controller loop against simulated `MarkTaskCompleted` patches that drive tasks through completion.
- Assert the observed sequence of convoy phases and task phases matches expectation.

### HTTP backend integration (minikube, gated)

- Apply both CRDs. Confirm CEL validations reject edits to `spec.workflow_ref` and `spec.inputs` (one negative test per field).
- Create a WorkflowTemplate with a two-task DAG (`implement` → `review`).
- Create a Convoy referencing it.
- Run the example controller binary in a background task.
- Patch `tasks.implement.phase = Completed` via `patch_status(MarkTaskCompleted)`; assert `review` moves to Ready.
- Patch `tasks.review.phase = Completed`; assert convoy `phase = Completed`.

### In-memory backend end-to-end

- Create `WorkflowTemplate` + `Convoy` in the in-memory backend.
- Run the controller loop against simulated status patches that advance tasks through Ready → Running → Completed.
- Assert sequence of convoy phase transitions and task-phase transitions.

### HTTP backend integration (minikube, gated)

- Apply both CRDs.
- Create a WorkflowTemplate with a two-task DAG (`implement` → `review`).
- Create a Convoy referencing it.
- Run the example controller binary in a background task.
- Patch `tasks.implement.phase = Completed` via `/status`; assert `review` moves to Ready.
- Patch `tasks.review.phase = Completed`; assert convoy `phase = Completed`.

## Example Binary

`crates/flotilla-resources/examples/convoy_controller.rs`:

- Accepts `--namespace` flag, defaults to `flotilla`.
- Bootstraps CRDs via `ensure_crd`.
- List-then-watch loop as above.
- Structured logging with `tracing` matching the codebase style.
- Runs against minikube by default via `HttpBackend::from_kubeconfig`.

## Deliverables

### Stage 1 revision (as part of Stage 3 work)

1. `Resource::StatusPatch` associated type and `StatusPatch` trait (`apply` + `to_merge_patch`).
2. `TypedResolver::patch_status(name, rv, patch)` method.
3. HTTP backend: `PATCH` against `/status` subresource with merge-patch content-type and resourceVersion precondition.
4. In-memory backend: implement `patch_status` via `StatusPatch::apply` under the store lock.
5. `NoStatusPatch` uninhabited enum; `WorkflowTemplate` adopts it (trivial existing-crate revision).
6. Parity tests for `WorkflowTemplate::StatusPatch` (trivially pass since uninhabited) and for the Convoy variants (detailed above).

### Stage 3 proper

7. `Convoy` Rust type and `Resource` impl.
8. `ConvoySpec`, `ConvoyStatus`, `ConvoyPhase`, `TaskState`, `TaskPhase`, `InputValue`, `PlacementStatus`, `WorkflowSnapshot`, `SnapshotTask` types.
9. `ConvoyStatusPatch` enum and its `StatusPatch<ConvoyStatus>` impl.
10. Owner-scoped constructor modules (`controller_patches`, `provisioning_patches`, `external_patches`) gating variant construction by visibility.
11. Pure `reconcile(convoy, template, now) -> ReconcileOutcome` function.
12. `ReconcileOutcome`, `ConvoyEvent` types.
13. Convoy CRD YAML with CEL immutability validations (replaces the stub).
14. Table tests for reconcile.
15. StatusPatch apply/merge-patch parity tests.
16. In-memory backend end-to-end test.
17. HTTP backend integration test against minikube, including CEL-rejection checks.
18. `examples/convoy_controller.rs` — runnable controller binary.

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

### Typed status patches (`StatusPatch` associated type)

Multi-writer safety on `/status` needs either full-status replacement with optimistic concurrency retry, or partial patches. Full replacement forces every writer to round-trip the entire status to mutate one field — noisy, and susceptible to "I replaced the whole thing but forgot to preserve the field you just wrote" bugs. Partial patches, expressed as typed `ConvoyStatusPatch` variants, scope each mutation to exactly its owned fields.

The associated type (`Resource::StatusPatch`) makes the legitimate mutations a declared vocabulary per resource. Owner-scoped constructor modules gate variant construction so "only the convoy controller can advance tasks to Ready" becomes a compile-time property, not a comment.

The in-memory backend's `StatusPatch::apply(&mut status)` avoids re-implementing JSON merge-patch semantics (null-as-remove, nested merge, array replacement); the HTTP backend's `to_merge_patch()` handles wire serialisation. The two are kept in sync by an exhaustive parity test per variant.

The alternative of a spec-side command queue (`spec.task_actions: [{task, action: Complete}]`) was rejected: controllers normally don't mutate their own spec, "mark complete" is an event not desired state, and there's no real gain over typed status patches.

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
