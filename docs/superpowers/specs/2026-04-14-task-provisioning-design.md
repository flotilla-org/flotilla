# Task Provisioning and Policy — Design (Stage 4a)

## Context

Stage 4 of the convoy implementation (see `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md`). Stage 3 shipped the Convoy resource and a controller that advances tasks through the DAG, stopping at `Ready`. Stage 4 turns `Ready` into `Running` by actually provisioning what a task needs — environment, checkout, processes — and propagating completion back.

Stage 4a is deliberately scoped to the **placement-via-flotilla-daemon** column of the placement matrix:

```
                placement:
state:          flotilla-daemon          k8s-cluster
flotilla-cp     laptop, no k8s           laptop state, cluster workloads
                                         (homelab, employer namespace)
k8s             current prototype        fully cluster-native
```

Stage 4a covers the left column for both state options. The right column (k8s placement backend creating Pods) is Stage 4k, deferred. Selector resolution, agent-side completion, presentation, the `PersistentAgent` resource, and the broader integration with current flotilla state stores are also out of scope and explicitly tracked.

The scoping is honest about the work involved: a "productive" k8s Pod backend needs a runnable image with all tools, a checkout mechanism that crosses into the cluster, per-tool config preparation (`~/.claude`, auth shuttling), and selector resolution. Each of those is a real design problem. Stage 4a reuses flotilla-core's existing launch path (`WorkspaceOrchestrator`, providers) so we ship something productive on day one without solving any of those problems first.

## Scope

### In scope

- Five new resources (`Host`, `Environment`, `Checkout`, `TerminalSession`, `TaskWorkspace`) plus an orthogonal `PlacementPolicy` resource.
- A small controller framework added to `flotilla-resources` (the same crate Stage 3 lives in).
- A new `flotilla-controllers` crate containing four reconcilers (TaskWorkspace, Environment, Checkout, TerminalSession) and three actuators wrapping existing flotilla-core providers (Docker, CheckoutManager, TerminalPool).
- Daemon startup: self-registration as a `Host`, creation of a host-direct `Environment`, creation of default `PlacementPolicy` resources.
- A new `flotillad` binary in the `flotilla-daemon` crate; the existing `flotilla` TUI binary's embedded-daemon mode demoted to a test/dev `--embedded` flag.
- Two `PlacementPolicy` variants: `host_direct` and `docker_per_task`.
- Tests at every layer: pure reconcile, status patch, framework, actuator, in-memory end-to-end, minikube integration.

### Out of scope (Stage 4a)

- K8s cluster-native placement backend (Stage 4k).
- Selector resolution: agent processes still cannot run end-to-end. Tool processes work fine.
- Agent-side task completion CLI.
- Per-tool config preparation in environments.
- AttachableSet migration / deletion of legacy state stores.
- Presentation manager integration (Stage 5).
- Lease-based leader election.

## Crate and binary topology

```
flotilla-protocol      (no flotilla deps)
flotilla-resources     (deps: protocol)              ← gains controller framework
flotilla-core          (deps: protocol)
flotilla-controllers   (deps: resources, core)       ← NEW
flotilla-daemon        (deps: controllers, core, resources)
  binary: flotillad                                  ← NEW
flotilla-client        (deps: protocol)
flotilla-tui           (deps: client)
flotilla        binary (deps: client)                ← TUI/CLI; embedded-daemon mode demoted
```

No backwards dependencies. Each crate has one job. `flotilla-controllers` is the natural home for code that bridges resources and the existing provider system.

The `flotillad` binary becomes the only production path for running controllers. The `flotilla` TUI binary keeps an `--embedded` flag for tests and single-shot dev work, with a deprecation note. Tests that want everything-in-one-process can use `InProcessDaemon` plus a small test-support helper that wires controllers in.

## Blue-sky model (orientation)

For navigators landing in this spec without the brainstorm context:

- **`WorkflowTemplate`** — what to run, in what order. Already exists (Stage 2).
- **`Convoy`** — instance of a workflow with concrete inputs. Already exists (Stage 3).
- **`PlacementPolicy`** — *how* and *where* tasks run. Named, possibly auto-discovered. Eventually delegates to a `PersistentAgent` (the Quartermaster). New in Stage 4a; scoped to two variants for now.
- **`PersistentAgent`** — single resource type with k8s-style labels/selectors; conventionally-labeled instances are Quartermaster, Yeoman, custom SDLC agents. Future.
- **`Host`** — a place tasks can run (a flotilla daemon). New in Stage 4a; self-registered.
- **`Environment`** — a way of running things on a host. Direct-host, Docker, future k8s-pod, etc. New in Stage 4a.
- **`Checkout`** — a working tree on a host. New in Stage 4a.
- **`TerminalSession`** — an individual process session. New in Stage 4a.
- **`TaskWorkspace`** — the per-task bundle that ties a Convoy task to its concrete Environment + Checkout + TerminalSessions. New in Stage 4a.
- **`PresentationManager`** — surface for user interaction with workspaces. Stage 5.

## Resources

### `Host`

```yaml
apiVersion: flotilla.work/v1
kind: Host
metadata:
  name: 01HXYZ...                  # existing persistent host id
  labels:
    flotilla.work/hostname: alice-laptop
spec: {}                           # empty; Host describes self via status
status:
  capabilities:
    docker: true
    git_version: "2.43.0"
    # OS, CPU, memory, GPU, VRAM, additional tool versions to grow over time
  heartbeat_at: "2026-04-14T12:34:56Z"
  ready: true
```

- Cluster-namespaced or per-namespace? Namespaced (matches our existing convention).
- Written by the daemon that *is* this host. On startup the daemon creates-or-updates its own Host record. A periodic task refreshes `heartbeat_at` and recomputes `ready`.
- **No finalizer** — Host has no external state to clean up; the daemon going away just stops heartbeat updates.
- **Staleness**: a Host whose `heartbeat_at` is older than ~60s is treated as not ready by consumers. Bounded TTL.
- **Reusability for scheduling**: capabilities-rich status (CPU, memory, GPU/VRAM) lets a future Quartermaster select hosts. Stage 4a populates the minimum (`docker`, `git_version`) and grows over time.

### `Environment`

One CRD with a tagged-by-presence variant — same pattern as `ProcessSource` in `WorkflowTemplate`. Each variant carries a `host_ref` *if applicable* (some future variants like RunPod or `meta_policy` won't).

```yaml
apiVersion: flotilla.work/v1
kind: Environment
metadata:
  name: alice-docker-dev-task-123
  labels:
    flotilla.work/host: 01HXYZ...
spec:
  # Exactly one variant populated.
  host_direct:
    host_ref: 01HXYZ...
  # docker:
  #   host_ref: 01HXYZ...
  #   image: ghcr.io/flotilla/dev:latest
  #   mounts:
  #     - host_path: /Users/alice/dev/flotilla.feat-foo
  #       container_path: /workspace
  #       mode: rw
  #   env:
  #     FOO: bar
status:
  phase: Ready                     # Pending | Ready | Terminating | Failed
  ready: true
  # Variant-specific:
  # docker_container_id: "abc123..."
  message: null
```

- **Mounts live on `Environment.spec`** as static fields, written at creation by the provisioning controller. The Environment controller never touches them; it reads its own spec and actuates.
- **Direct-host** Environments are pre-existing per host (created at daemon startup) and shared across TaskWorkspaces. Not owned by any TaskWorkspace.
- **`docker_per_task` Environments** are owned by their TaskWorkspace via `ownerReferences` and GC-cascade on TaskWorkspace deletion.
- **Finalizer** `flotilla.work/environment-teardown` runs kind-specific teardown (`docker rm -f` for docker, no-op for host_direct) before deletion completes.

### `Checkout`

```yaml
apiVersion: flotilla.work/v1
kind: Checkout
metadata:
  name: flotilla-fix-bug-123
  ownerReferences:
    - apiVersion: flotilla.work/v1
      kind: TaskWorkspace
      name: convoy-fix-bug-123-implement
      controller: true
  labels:
    flotilla.work/host: 01HXYZ...
    flotilla.work/repo: github.com/flotilla-org/flotilla
spec:
  host_ref: 01HXYZ...
  repo: https://github.com/flotilla-org/flotilla    # canonical git URL
  ref: feat/convoy-resource                         # branch / tag / sha (branches only in v1)
  method: worktree                                  # worktree | clone
status:
  phase: Ready                                      # Pending | Preparing | Ready | Terminating | Failed
  path: /Users/alice/dev/flotilla.fix-bug-123
  commit: 44982740...
  message: null
```

- **Git-shaped in v1.** The "VCS abstraction" is notional in flotilla today (provider trait exists, no real consumer); we don't expose it as a CRD field. Future: a `vcs: git | hg | …` discriminator if we add other backends.
- Default ownership: per-task (owned by TaskWorkspace, GC-cascades). Shared persistent checkouts are deferred.
- Method `worktree` matches today's default; `clone` covers the "no parent clone exists" case (for k8s pod backends later). Branch refs only in v1; sha/tag/detached-head deferred.
- **Finalizer** `flotilla.work/checkout-cleanup` runs `git worktree remove` (or `rm -rf` for `clone`) before deletion.

### `TerminalSession`

Models the **outer shell wrapper**, not the inner command. The pool implementation (cleat, shpool, passthrough) wraps the configured command in a shell so process exits don't leave a hung terminal.

```yaml
apiVersion: flotilla.work/v1
kind: TerminalSession
metadata:
  name: convoy-fix-bug-123-implement-coder-0
  ownerReferences:
    - apiVersion: flotilla.work/v1
      kind: TaskWorkspace
      name: convoy-fix-bug-123-implement
      controller: true
  labels:
    flotilla.work/task_workspace: convoy-fix-bug-123-implement
    flotilla.work/role: coder
spec:
  environment_ref: alice-docker-dev-task-123
  role: coder                                       # informational
  command: "claude --prompt '…'"                    # literal command to wrap
  cwd: /workspace                                   # always explicit; controller fills it in
  pool: cleat                                       # cleat | shpool | passthrough
status:
  phase: Running                                    # Starting | Running | Stopped
  session_id: "abc123..."
  pid: 12345                                        # outer shell
  started_at: "2026-04-14T12:35:00Z"
  stopped_at: null
  inner_command_status: Running                     # Running | Exited (informational only)
  inner_exit_code: null
  message: null
```

- **Lifecycle is the outer shell's lifecycle.** `phase: Running` means the wrapper is alive, regardless of whether the inner command is still running. The inner command exiting is observed (`inner_command_status`, `inner_exit_code`) but is not a session lifecycle event.
- **Pool selection** is per-host capability + per-policy preference: Host advertises which pools are available, PlacementPolicy says which to use, the controller writes the choice into TerminalSession spec. If the chosen pool isn't available on the host, provisioning fails with a clear message.
- **No automatic restart** in Stage 4a. A `Stopped` session stays Stopped; future Bosun-style restart behavior is a separate concern.
- **Finalizer** `flotilla.work/terminal-teardown` cleanly terminates the session and releases the pool entry before deletion.

### `TaskWorkspace`

The per-task bundle. Created by the provisioning controller when a Convoy task transitions to `Ready`. Owned by the parent Convoy.

```yaml
apiVersion: flotilla.work/v1
kind: TaskWorkspace
metadata:
  name: convoy-fix-bug-123-implement
  ownerReferences:
    - apiVersion: flotilla.work/v1
      kind: Convoy
      name: fix-bug-123
      controller: true
  labels:
    flotilla.work/convoy: fix-bug-123
    flotilla.work/task: implement
spec:
  convoy_ref: fix-bug-123
  task: implement                                   # task name in the parent's workflow_snapshot
  placement_policy_ref: docker-on-01HXYZ
status:
  phase: Provisioning                               # Pending | Provisioning | Ready | TearingDown | Failed
  message: null
  observed_policy_ref: docker-on-01HXYZ
  observed_policy_version: "12"                     # PlacementPolicy resourceVersion at resolution
  environment_ref: alice-docker-dev-task-123
  checkout_ref: flotilla-fix-bug-123
  terminal_session_refs:
    - convoy-fix-bug-123-implement-coder-0
    - convoy-fix-bug-123-implement-build-0
  started_at: "..."
  ready_at: "..."
```

- Process definitions are read from the parent Convoy's `status.workflow_snapshot` — not duplicated here.
- **Lifecycle**: persists for the convoy's life. Owner-ref cascade GCs everything (TaskWorkspace + owned Environment/Checkout/TerminalSessions) when the Convoy is deleted. No auto-delete on terminal task transition; the bundle stays inspectable until the Convoy is gone.
- **Running TerminalSessions on terminal tasks** stay alive until the TaskWorkspace cascades. Future "kill on terminal" policy field or Bosun-style cleanup is deferred.
- **No finalizer** on TaskWorkspace itself — its children carry their own finalizers, owner-ref cascade waits for them to clear before TaskWorkspace deletes.
- **Policy snapshot**: `observed_policy_ref + observed_policy_version` records which PlacementPolicy was used (and at what version), matching Stage 3's pattern of recording observed_workflow_ref/version.

### `PlacementPolicy`

Orthogonal "how" resource. Referenced by `TaskWorkspace.spec.placement_policy_ref`. Encodes a stitching pattern.

```yaml
apiVersion: flotilla.work/v1
kind: PlacementPolicy
metadata:
  name: docker-on-01HXYZ
spec:
  pool: cleat                          # preferred terminal pool

  # Exactly one variant populated.
  host_direct:
    host_ref: 01HXYZ...
  # docker_per_task:
  #   host_ref: 01HXYZ...
  #   image: ghcr.io/flotilla/dev:latest
  #   checkout_mount_path: /workspace
  #   default_cwd: /workspace
  #   env: { FOO: bar }
```

- Two variants in Stage 4a: `host_direct` and `docker_per_task`. Future variants (`docker_shared`, `k8s_pod`, `runpod`, `meta_policy` delegating to a Quartermaster) are deferred.
- **`host_ref` lives per-variant**, not at top level — some future variants (RunPod, meta-policy) don't bind to a specific host.
- **`pool` is at top level** because it applies uniformly to anything that creates terminal sessions.
- **No status, no controller for PlacementPolicy itself** — pure data, like WorkflowTemplate. The provisioning controller consults it during reconciliation.
- **Daemon-created defaults** at startup (see Daemon Startup section): one `host-direct-<host>` policy and (if docker is available) one `docker-on-<host>` policy. User can edit, replace, or add custom ones.

### Resource interactions and ownership summary

| Resource | Owned by | Finalizer | Created by |
|----------|----------|-----------|------------|
| Host | nobody | none | daemon (self) |
| Environment (host_direct) | nobody | none | daemon (auto-created at startup) |
| Environment (docker_per_task) | TaskWorkspace | docker-teardown | provisioning controller |
| Checkout | TaskWorkspace | checkout-cleanup | provisioning controller |
| TerminalSession | TaskWorkspace | terminal-teardown | provisioning controller |
| TaskWorkspace | Convoy | none (children carry finalizers) | provisioning controller |
| PlacementPolicy | nobody | none | daemon (defaults) or user (custom) |

## Controller framework (Stage 1 layer addition)

Lives in `crates/flotilla-resources/src/controller/`. Used by Stage 4a's reconcilers and the existing Stage 3 convoy controller (refactored).

```rust
pub trait Reconciler: Send + Sync + 'static {
    type Resource: Resource;
    type Dependencies;

    async fn fetch_dependencies(
        &self,
        obj: &ResourceObject<Self::Resource>,
    ) -> Result<Self::Dependencies, ResourceError>;

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource>;

    async fn run_finalizer(
        &self,
        obj: &ResourceObject<Self::Resource>,
    ) -> Result<(), ResourceError>;

    fn finalizer_name(&self) -> Option<&'static str>;
}

pub struct ReconcileOutcome<T: Resource> {
    pub patch: Option<T::StatusPatch>,
    pub actuations: Vec<Actuation>,
    pub events: Vec<Event>,
    pub requeue_after: Option<Duration>,
}

pub enum Actuation {
    CreateEnvironment    { spec: EnvironmentSpec, owner_ref: ResourceRef, name: String },
    CreateCheckout       { spec: CheckoutSpec,    owner_ref: ResourceRef, name: String },
    CreateTerminalSession { spec: TerminalSessionSpec, owner_ref: ResourceRef, name: String },
    CreateTaskWorkspace  { spec: TaskWorkspaceSpec, owner_ref: ResourceRef, name: String },
    DeleteResource       { kind: ResourceKind, name: String },
    PatchConvoyTask      { convoy_name: String, patch: ConvoyStatusPatch },
}

/// A secondary watch is spawned alongside the primary watch and feeds primary
/// keys into the shared reconcile channel. Each impl handles one Watched
/// resource type and a label-based mapping back to primary names. The trait
/// is object-safe (no Watched associated type leaks through `dyn`); concrete
/// impls keep their Watched type internal.
pub trait SecondaryWatch: Send + Sync {
    type Primary: Resource;

    async fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Result<(), ResourceError>;
}

// A typed helper for the common case (concrete impls use this internally,
// not the dyn-erased trait above):
pub struct LabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,    // e.g. "flotilla.work/task_workspace"
    pub _marker: PhantomData<(W, P)>,
}

pub struct ControllerLoop<R: Reconciler> {
    primary: TypedResolver<R::Resource>,
    secondaries: Vec<Box<dyn SecondaryWatch<Primary = R::Resource>>>,
    reconciler: R,
    resync_interval: Duration,
    backend: ResourceBackend,
}

impl<R: Reconciler> ControllerLoop<R> {
    pub async fn run(self) -> Result<(), ResourceError> { /* … */ }
}
```

### Loop mechanics

- **Primary watch**: `list()` → reconcile each → `watch(WatchStart::FromVersion(rv))`. Standard Stage 3 list-then-watch.
- **Secondary watches**: one task per secondary spawned alongside the primary watch. Each watched event maps via `SecondaryWatch::map_to_primary_keys` (typically reading a label) to a list of primary keys to enqueue.
- **Shared reconcile channel**: all watches push primary keys into one mpsc channel. A worker dequeues, dedupes consecutive entries for the same key, fetches the primary by name, calls `reconcile`, applies the typed patch via `apply_status_patch`, then enacts each `Actuation`.
- **Resync ticker** (~60s): periodically pushes every known primary key. Standard k8s safety net.
- **Finalizer handling**: on a primary with `metadata.deletionTimestamp` set and the configured finalizer present, call `reconciler.run_finalizer(...)`, then patch the resource to remove the finalizer entry, then let GC complete.

### Conflict and retry

`apply_status_patch` already handles status-write conflicts via read-modify-write retry. Actuations (creates) are idempotent by name — the loop checks before creating. Deletes are tolerant of NotFound.

### Refactor of Stage 3's convoy controller

Mechanical: implement `Reconciler` for `ConvoyReconciler`, instantiate `ControllerLoop` in the daemon. No behavior change. Same tests pass.

## Provisioning controllers (Stage 4a, in `flotilla-controllers`)

One reconciler per resource type. All run in the daemon. Each uses `ControllerLoop` from the framework.

### `HostReconciler`

Watches Host resources (primary). No secondaries. Mostly self-modifies — refreshes `heartbeat_at`, recomputes `ready`, updates capabilities snapshot. No finalizer.

### `EnvironmentReconciler`

Watches Environment resources (primary). No secondaries. Branches on `spec.<kind>`:

- `host_direct`: no actuation, immediately `Ready`.
- `docker`: calls flotilla-core's Docker provider (`ensure_image` → `create`). Updates `status.docker_container_id`, transitions to `Ready` when the container is up, `Failed` on error.

Finalizer: `flotilla.work/environment-teardown`. Branches on kind for cleanup.

### `CheckoutReconciler`

Watches Checkout resources (primary). No secondaries. Calls flotilla-core's `CheckoutManager` based on `spec.method`. Updates `status.path`, `status.commit`, transitions phases.

Finalizer: `flotilla.work/checkout-cleanup`. Worktree remove or `rm -rf` per method.

### `TerminalSessionReconciler`

Watches TerminalSession resources (primary). No secondaries. Looks up the referenced Environment (must be `Ready`), calls flotilla-core's `TerminalPool` (cleat / shpool / passthrough) to start a wrapped session. Updates `status.session_id`, `status.phase`. Tracks the inner command's status as informational fields.

The pool implementation handles the shell-wrapping behavior — TerminalSession spec carries the literal command, the pool wraps it.

Finalizer: `flotilla.work/terminal-teardown`. Stops the session and releases the pool entry.

### `TaskWorkspaceReconciler`

Watches TaskWorkspace (primary) plus Environment, Checkout, TerminalSession as secondaries (each mapping back to its `flotilla.work/task_workspace` label).

Reconcile flow:

1. **Resolve PlacementPolicy** via `placement_policy_ref`. Missing → `Failed` + propagate to Convoy via `MarkTaskFailed`.
2. **Read parent Convoy's `status.workflow_snapshot`** for the task's process definitions.
3. **Ensure Checkout.** If `status.checkout_ref` unset, emit a `CreateCheckout` actuation (owned by this TaskWorkspace, host_ref + repo + ref + method from the policy). Wait (next reconcile pass) until the Checkout reaches `Ready`.
4. **Ensure Environment.** Branch on policy variant:
   - `host_direct`: look up the shared host-direct Environment for the host. Set `status.environment_ref`. No creation.
   - `docker_per_task`: if `status.environment_ref` unset, emit `CreateEnvironment` with mounts derived from `Checkout.status.path`. Wait until `Ready`.
5. **Ensure TerminalSessions**, one per process. If a session for a given role is missing, emit `CreateTerminalSession` with `cwd` derived from policy + Environment (e.g. `default_cwd: /workspace` for docker_per_task), `pool` from policy.
6. **All Ready** → patch `status.phase = Ready` and emit `PatchConvoyTask` with `MarkTaskRunning` for the Convoy.

Failure at any step: `status.phase = Failed` + `MarkTaskFailed` propagation. No automatic retry.

No finalizer on TaskWorkspace itself; child finalizers handle external state.

## Daemon startup

The daemon at startup, after connecting to the resource backend:

1. **Self-register as Host**: create-or-update a Host resource for itself (using the existing persistent host id as the resource name). Spawn a periodic heartbeat task that updates `Host.status.heartbeat_at` every ~30s.
2. **Create the host-direct Environment** for itself if not present. Idempotent.
3. **Create default PlacementPolicies**:
   - Always: `host-direct-<host-id>` (variant: `host_direct`).
   - If `Host.status.capabilities.docker == true`: `docker-on-<host-id>` (variant: `docker_per_task`, with a sensible default image).
4. **Spawn all controller loops**: HostReconciler, EnvironmentReconciler, CheckoutReconciler, TerminalSessionReconciler, TaskWorkspaceReconciler, ConvoyReconciler (refactored from Stage 3).

This is the "discovered resources" pattern in its simplest form: the daemon creates the resources that describe its own capabilities, and they lifecycle out of band from user interaction. User can edit or replace any of them; the daemon doesn't keep regenerating.

## Failure handling

- **Reconcile failure** within a reconciler → `phase = Failed` + message + propagation to the next layer up. TaskWorkspace failures propagate to Convoy via `MarkTaskFailed`.
- **No automatic retry** within Stage 4a. The user retries by deleting the failed resource (TaskWorkspace, etc.); the controllers will create fresh ones from scratch on the next reconcile pass if the upstream resource is still in a state that wants provisioning.
- **Heartbeat staleness on Host**: the provisioning controller refuses to place new TaskWorkspaces on a Host whose `ready: false` or `heartbeat_at` is older than ~60s. TaskWorkspaces already provisioned on a now-stale host are eventually marked `Failed` after extended staleness; full "host comes back, what do we do" is a future Bosun-style concern.
- **Cancellation cascades** from Stage 3's convoy controller: when a task is patched to `Cancelled` (fail-fast), TerminalSessions for that task stay alive until TaskWorkspace cascades. Auto-cleanup-on-cancellation is a future policy.

## Tests

### Pure reconcile tests

One file per reconciler, table-driven. For each reconciler:

- Fresh resource, dependencies present → expected actuations + status patch.
- Various status combinations → correct phase transitions.
- Failure modes (missing dependency, stale Host, etc.) → expected Failed patches and event emissions.

### `StatusPatch::apply` unit tests

Per variant on each new resource's StatusPatch enum. Same pattern as Stage 3.

### Framework tests

`ControllerLoop` with a fake `Reconciler`:
- Verify primary watch events trigger reconcile.
- Verify secondary watches map correctly and enqueue the right primary keys.
- Verify dedup of consecutive enqueues for the same key.
- Verify finalizer dispatch on `deletionTimestamp`.
- Verify conflict retry path.

### Actuator tests

Each actuator (Docker, worktree, terminal pool) tested against an injected fake provider. Verify spec → provider call translation; verify error paths produce Failed status.

### In-memory backend end-to-end

- Instantiate all controllers against the in-memory backend.
- Create WorkflowTemplate + PlacementPolicy + Convoy.
- Drive task-completion via simulated `MarkTaskCompleted` patches.
- Assert: TaskWorkspace created → Children created → Convoy reaches `Completed` → cascade GCs all children on Convoy delete.

### HTTP backend integration (minikube, gated)

- Apply all CRDs.
- Run `flotillad` with the controller loops.
- Create resources, drive a task through completion, assert end-to-end flow including CRD-level CEL validations where applicable.

### Docker actuator integration (gated on docker available)

- Real `docker run` for the `docker_per_task` variant.
- Confirms image pull, mount, container lifecycle, finalizer cleanup.

### Finalizer behavior tests

For each resource that carries a finalizer: verify cleanup runs, finalizer entry is cleared, deletion completes.

## Deliverables

### Stage 1 layer (in `flotilla-resources`)

1. `controller` module: `Reconciler` trait, `SecondaryWatch` trait, `ControllerLoop`, `Actuation` enum, `ReconcileOutcome`.
2. Refactor Stage 3 convoy controller to implement `Reconciler` (mechanical, no behavior change).
3. Framework tests.

### Stage 4a proper

4. New crate `flotilla-controllers`.
5. Six new CRDs: `Host`, `Environment`, `Checkout`, `TerminalSession`, `TaskWorkspace`, `PlacementPolicy`. CEL immutability where applicable.
6. Rust types for each + `StatusPatch` enums + per-resource reconcilers.
7. Three actuators wrapping existing flotilla-core providers: Docker (Environment), CheckoutManager (Checkout), TerminalPool (TerminalSession).
8. Daemon startup logic: self-register as Host, create host-direct Environment, create default PlacementPolicies, spawn all controller loops.
9. Heartbeat task: periodic Host status updates.
10. New `flotillad` binary target in `flotilla-daemon`.
11. `flotilla` TUI binary's embedded-daemon mode demoted to `--embedded` flag with deprecation note.
12. Tests at every layer (pure reconcile, StatusPatch::apply, framework, actuator, in-memory end-to-end, minikube integration, docker actuator integration, finalizer behavior).
13. CRD bootstrap via `ensure_crd` for example/integration paths.

## Design Decisions

### Path C: flotilla-daemon placement now, k8s placement deferred

A "productive" k8s Pod backend needs a runnable image, a checkout mechanism for the cluster, per-tool config preparation, and selector resolution — each a real design problem. Stage 4a uses the existing `WorkspaceOrchestrator` and providers in `flotilla-core` to ship a productive prototype on day one, without solving any of those four. K8s placement (Stage 4k) gets its own brainstorm where image / checkout / config can be designed honestly. The 2x2 of state × placement (flotilla-cp vs k8s × flotilla-daemon vs k8s-cluster) makes both columns valid; we're shipping the left column first.

### Per-layer resources, not a single bundled resource

Five resources (Host, Environment, Checkout, TerminalSession, TaskWorkspace) instead of one bundled `Workspace` resource. Each existing flotilla provider concept gets its own resource shape with its own lifecycle, finalizer, and visibility. Costs more upfront than a single resource but pays off for: independent inspection and labelling (`kubectl get terminalsessions -l role=coder`), clear ownership boundaries, future per-resource controllers, and the agent-era model where a Yeoman or Bosun watches per-resource events. Underspecifying the cuts now would force a much larger disaggregation transition later.

### One CRD per concept, kind-discriminator inside

Environment is one CRD with `host_direct` / `docker` / future variants distinguished by field presence (untagged enum on the Rust side, `oneOf` on the CRD). Same shape we proved with `ProcessSource` in WorkflowTemplate. Polymorphic resource references (`{kind: DockerEnvironment, name: foo}`) are ugly in YAML and require every consumer to case-switch — a single resource with an internal discriminator keeps references clean (`environment_ref: foo`).

### Mounts on Environment.spec, written at creation

Mounts are static fields on Environment, populated by the provisioning controller when it creates a per-task Environment. The Environment controller never touches them; it reads its own spec and actuates. Path-coordination across resources happens in one place (the TaskWorkspace controller) at one time (creation), not via cross-controller patching.

### PlacementPolicy is referenced data, not just a name

PlacementPolicy is a real resource with a CRD, status-less, like WorkflowTemplate. Daemon auto-creates defaults at startup; users can author custom policies via YAML. Not just a string config — a resource so it can be inspected, labelled, eventually selected by labels, and one day pointed at a Quartermaster agent.

### TerminalSession models the outer shell wrapper

The pool wraps the configured command in a shell so process exits don't leave hung terminals — current flotilla pain. TerminalSession's lifecycle is the wrapper's lifecycle; inner-command exit is observed but informational. Maps cleanly onto a future Bosun agent that handles restart/repair behavior.

### Per-resource controllers (option B), with framework extraction

Four narrow reconcilers (one per resource type) on top of a small `ControllerLoop` framework. The framework extraction (Stage 1 layer) is small (~200-300 lines) and benefits every controller from now on. Without the framework, "boilerplate" was the argument for option A (one controller); with the framework, B is unambiguously cleaner. Each reconciler's tests are scoped, future variants of each resource type are local additions, and the foundation is in place for cluster-native deployment splitting controllers across processes.

### Dedicated `flotillad` binary

The `flotilla` TUI binary's embedded-daemon mode never quite earned its keep — multiple TUI windows want to share state (which forces daemon-as-process), CLI dies with TUI, and providing controllers to an embedded daemon means a TUI-binary dep on `flotilla-controllers` (very wide). A separate `flotillad` is the production path; `flotilla` becomes pure client/TUI. Tests retain `--embedded` for single-process testing where useful.

### Owner refs + finalizers for proper cleanup

AttachableSet today references Environment / Checkout without owning them, and there's no proper teardown — moving to resources makes cleanup a first-class concern. Owner-ref cascade GCs children when a TaskWorkspace deletes; finalizers on resources with external state (Environment, Checkout, TerminalSession) ensure docker containers, worktrees, and processes are cleaned up before the resource vanishes. Standard k8s pattern; explicit and reliable.

### Self-registration for Host, daemon-created defaults for PlacementPolicy

The daemon creates resources describing its own capabilities at startup, with predictable names. User can edit or replace; daemon doesn't fight user edits. This is the simplest form of the "discovered resources" pattern (a controller creates resources that lifecycle out of band from user interaction) — full discovered-resource design comes later if we want to scan for and represent ambient state more broadly.

## Deferred Items

To capture in the brainstorm-prompts master deferred list under "From Stage 4a":

- **Stage 4k**: k8s cluster-native placement backend (Pods). Requires image-as-resource, cross-cluster checkout, selector resolution, per-tool config preparation.
- **Image as a cluster resource** — declarative spec with availability guarantees ("make this image accessible from this provider"), on-demand vs pre-fetched, registry policy.
- **Selector resolution** (capability → concrete agent command). Carried over from Stage 2; agent processes still cannot run end-to-end until this lands. Tool processes work fine.
- **Auto-discovery of additional policies** beyond daemon-startup defaults — broader "discovered resources" pattern.
- **Agent-side completion CLI** — agents marking their own task complete via a CLI command.
- **Per-tool config preparation** in environments (`~/.claude` shuttling, auth tokens, etc.). Carried forward as a known gap for the Docker variant.
- **Step-plan retirement** — `StepPlan` → convoy-driven coordination. Bigger refactor; not Stage 4a.
- **Multi-host placement** — SSH-reachable Hosts, mesh-aware Host resources, label-selector host targeting.
- **Bosun-style automatic restart / repair / cleanup** — restart policies, terminal-session restarts on inner-command crash, cleanup on terminal task transitions.
- **Convoy launched against an existing Checkout** — workflow flexibility for "use this existing tree as the work area."
- **Repository as a resource** — currently URL on Checkout.spec; a Repository resource would let URL → name indirection and per-repo configuration.
- **Detached-head / sha / tag refs on Checkout** — useful for agent-driven bisect workflows and pinned-version provisioning.
- **Shared Docker environments** as a placement variant — needs the shared-env-plus-per-task-checkout composability question solved.
- **Meta-policy variant** for PlacementPolicy — delegate to a Quartermaster agent that picks among other policies.
- **TUI/CLI binary split** — separate the TUI from the CLI in `flotilla` as the next structural cleanup.
- **Lease-based leader election** for controllers — carried over from Stage 3 deferred list.
- **Per-task restart policies / explicit retry UX** — a way to say "retry this failed task" without manually deleting resources.
- **Auto-cleanup of stopped sessions on terminal task transitions** — opt-in policy field.
- **Vessel / Crew / Shipment naming pass** — convoy-themed renames once the abstractions settle (TaskWorkspace → Vessel, processes → Crew, artifacts → Shipment).
- **VCS abstraction in resource shape** — Checkout is git-shaped in v1; future `vcs:` discriminator for hg / fossil / etc.
