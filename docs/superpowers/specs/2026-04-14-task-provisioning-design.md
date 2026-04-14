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

- Six new resources (`Host`, `Environment`, `Clone`, `Checkout`, `TerminalSession`, `TaskWorkspace`) plus an orthogonal `PlacementPolicy` resource.
- A small controller framework added to `flotilla-resources` (the same crate Stage 3 lives in).
- A new `flotilla-controllers` crate containing five reconcilers (TaskWorkspace, Environment, Clone, Checkout, TerminalSession) and three actuators wrapping existing flotilla-core providers (Docker, CheckoutManager, TerminalPool).
- Daemon startup: self-registration as a `Host`, creation of a host-direct `Environment`, discovery of existing `Clone` resources from flotilla-core's repo registry, creation of default `PlacementPolicy` resources.
- A new `flotillad` binary in the `flotilla-daemon` crate. The existing `flotilla` TUI binary's embedded-daemon mode is removed entirely — Stage 4a is the cut. Tests that need everything-in-one-process compose `InProcessDaemon` + controllers via a small test-support helper.
- Two `PlacementPolicy` variants: `host_direct` and `docker_per_task`.
- **Env-relative model**: every persistent on-disk thing (Clone, Checkout, TerminalSession) lives in an `Environment`. The host's bare filesystem is the `host_direct` Environment; docker is another Environment kind; future k8s_pod likewise. Clone and Checkout reference `env_ref`, never `host_ref`.
- Tests at every layer: pure reconcile, status patch, framework, actuator, in-memory end-to-end, minikube integration.

### Out of scope (Stage 4a)

- K8s cluster-native placement backend (Stage 4k).
- Selector resolution: agent processes still cannot run end-to-end.
- Per-tool config preparation in environments.
- AttachableSet migration / deletion of legacy state stores.
- Presentation manager integration (Stage 5).
- Lease-based leader election.

### Completion in Stage 4a

Tool processes provision and run; some are designed never to complete (test runners, dev servers — *watchers*) and that's fine. Task completion is always **external** in Stage 4a, consistent with Stage 3: a human (or future agent) issues `MarkTaskCompleted` via a CLI subcommand. Auto-completion based on inner-process exit is deferred — it requires extending the `TerminalPool` API to surface inner-command events, plus an opt-in policy field on tasks that have a designated "progress-bearing" process. Stage 4a ships the small CLI affordance (`flotilla convoy <name> task <task> complete`) so end-users can drive completion without writing test scripts.

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
flotilla        binary (deps: client)                ← TUI/CLI only; embedded-daemon mode removed
```

No backwards dependencies. Each crate has one job. `flotilla-controllers` is the natural home for code that bridges resources and the existing provider system.

The `flotillad` binary becomes the only production path for running controllers. The `flotilla` TUI binary's embedded-daemon mode is removed entirely as part of Stage 4a — it can't run controllers without depending on `flotilla-controllers` (which would widen the TUI binary unacceptably), and a half-functional embedded mode is worse than no embedded mode. Tests that want everything-in-one-process compose `InProcessDaemon` plus controllers via a small test-support helper.

## Blue-sky model (orientation)

For navigators landing in this spec without the brainstorm context:

- **`WorkflowTemplate`** — what to run, in what order. Already exists (Stage 2).
- **`Convoy`** — instance of a workflow with concrete inputs. Already exists (Stage 3).
- **`PlacementPolicy`** — *how* and *where* tasks run. Named, possibly auto-discovered. Eventually delegates to a `PersistentAgent` (the Quartermaster). New in Stage 4a; scoped to two variants for now.
- **`PersistentAgent`** — single resource type with k8s-style labels/selectors; conventionally-labeled instances are Quartermaster, Yeoman, custom SDLC agents. Future.
- **`Host`** — daemon identity, heartbeat, capabilities. New in Stage 4a; self-registered.
- **`Environment`** — a runtime + filesystem. The host's bare filesystem is the `host_direct` Environment; docker is another kind; future k8s-pod likewise. New in Stage 4a.
- **`Clone`** — a specific on-disk git clone in some Environment's filesystem. Identity is (URL, env). New in Stage 4a. (A future logical `Repository` resource — representing the URL-level identity across clones — is deferred; see deferred list.)
- **`Checkout`** — a working tree in some Environment's filesystem. New in Stage 4a.
- **`TerminalSession`** — an individual process session in an Environment. New in Stage 4a.
- **`TaskWorkspace`** — the per-task bundle tying a Convoy task to its concrete Environment + Checkout + TerminalSessions. New in Stage 4a.
- **`PresentationManager`** — surface for user interaction with workspaces. Stage 5.

**Env-relative principle**: Clone, Checkout, and TerminalSession all carry `env_ref` rather than `host_ref`. The host's filesystem is just one kind of Environment (the `host_direct` one). When env-internal cases arrive (k8s_pod with its own writable layer), nothing in the schema needs to change — they just point `env_ref` at a non-host_direct env.

**Clone vs. logical "Repository"**: what Stage 4a calls `Clone` is intentionally the physical-clone concept — one per (URL, env) pair. The convoy names the logical repo (by URL) and the provisioning layer materializes Clones in whichever envs tasks need them. If we later want a queryable "logical Repository" resource (canonical URL, aliases, default-branch declared vs observed, GitHub/GitLab slug for ChangeRequestTracker/IssueProvider config, the anchor for cross-env "show all clones" queries), that becomes a separate `Repository` resource in a future stage. The `Clone` name is precise about what we have now and leaves the word "Repository" available for the logical layer later.

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

The host's bare filesystem is itself an Environment (`host_direct` kind). This is the env-relative model: every persistent on-disk thing (Clone, Checkout) lives in an Environment by `env_ref`; the host filesystem is just a special-named Environment.

```yaml
apiVersion: flotilla.work/v1
kind: Environment
metadata:
  name: host-direct-01HXYZ
  labels:
    flotilla.work/host: 01HXYZ...
spec:
  # Exactly one variant populated.
  host_direct:
    host_ref: 01HXYZ...
    repo_default_dir: /Users/alice/dev/flotilla-repos    # where new Clones land
  # docker:
  #   host_ref: 01HXYZ...
  #   image: ghcr.io/flotilla/dev:latest
  #   mounts:
  #     - source_path: /Users/alice/dev/flotilla.feat-foo  # path in the env's host's filesystem
  #       target_path: /workspace
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

- **Mounts live on `Environment.spec`** as static fields, written at creation by the `TaskWorkspaceReconciler` (which is the only thing that creates per-task Environments). The Environment controller never touches them; it reads its own spec and actuates.
- **Mount `source_path` is implicit "from the env's host's filesystem"** in Stage 4a — the docker env's `host_ref` tells you which host. A future cross-env mount story would add an explicit `from_env` field; the name `source_path` (vs `host_path`) keeps it forward-compatible. Joined-up summary views (TUI, CLI) show the resolved mount picture; the spec stays minimal.
- **`host_direct` Environments are auto-created by the daemon** (one per host, at startup). Not owned by any TaskWorkspace; persist for the daemon's life. The `repo_default_dir` here governs where new Clones land on this host.
- **`docker_per_task` Environments** are owned by their TaskWorkspace via `ownerReferences` and GC-cascade on TaskWorkspace deletion.
- **Finalizer** `flotilla.work/environment-teardown` on `docker_per_task` Environments runs `docker rm -f` before deletion completes. `host_direct` Environments carry no finalizer — there is no per-env state to tear down (the host filesystem outlives them).

### `Clone`

A specific on-disk git clone in some Environment's filesystem. The home of the `.git` directory; the parent for worktree-strategy Checkouts. Identity is (URL, env): at most one Clone per `(spec.url, spec.env_ref)` pair (enforced by deterministic naming + controller reuse logic).

```yaml
apiVersion: flotilla.work/v1
kind: Clone
metadata:
  name: flotilla-flotilla-org-host-direct-01HXYZ    # derived from url-slug + env_ref
  labels:
    flotilla.work/discovered: "true"                # set when auto-created via discovery
    flotilla.work/env: host-direct-01HXYZ
    flotilla.work/url-slug: flotilla-flotilla-org
spec:
  url: https://github.com/flotilla-org/flotilla     # canonical git URL
  env_ref: host-direct-01HXYZ                        # env that owns the filesystem
  path: /Users/alice/dev/flotilla                    # path within env_ref's filesystem
status:
  phase: Ready                                       # Pending | Cloning | Ready | Failed
  default_branch: main
  message: null
```

- **Git-shaped in v1.** The "VCS abstraction" is notional in flotilla today (provider trait exists, no real consumer); we don't expose it as a CRD field. Future: a `vcs:` discriminator (`git | hg | fossil | …`) if we add other backends.
- **Three creation paths in Stage 4a:**
  1. **Auto-discovery on daemon startup** — daemon scans flotilla-core's repo registry (`~/.config/flotilla/repos/*.toml`) and creates Clone resources for what it finds (env_ref = host-direct env of this daemon). The canonical URL is derived per-path by running `git remote get-url origin`; entries without an origin remote are skipped with a warning (see Daemon startup). Marked with `flotilla.work/discovered: "true"`. Lifecycle is out-of-band — these persist regardless of any TaskWorkspace.
  2. **User-authored** — `kubectl apply` of a Clone spec. The `CloneReconciler` actuates the clone if not already present; the resource exists in `phase: Pending` until the clone completes.
  3. **On-demand by `TaskWorkspaceReconciler`** — when a task needs a Clone (for worktree strategies) in some env and no matching Clone exists for `(url, env)`, the reconciler emits a `CreateClone` actuation. URL comes from `convoy.spec.repository.url` (see ConvoySpec extensions); env comes from the resolved PlacementPolicy. Deterministic naming means concurrent tasks converge on the same Clone.
- **Not owned** by any TaskWorkspace — Clones outlive the tasks that use them. Multiple tasks (possibly across multiple convoys) share a Clone by URL+env.
- **Deterministic naming**: `name = <url-slug>-<env_ref>`. Stable across restarts and across concurrent reconciles; `AlreadyExists` on `CreateClone` is success.
- **Finalizer** `flotilla.work/clone-cleanup` only runs on explicit delete. Stage 4a default: don't auto-delete the on-disk clone (deleting the resource removes the resource only). Explicit-delete-with-cleanup is a future opt-in.

### `Checkout`

A working tree in some Environment's filesystem. Owned per-task. Variant-discriminated by strategy.

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
    flotilla.work/env: host-direct-01HXYZ
spec:
  env_ref: host-direct-01HXYZ              # env that owns the filesystem
  ref: feat/convoy-resource                # branch (v1); sha/tag deferred
  target_path: /Users/alice/dev/flotilla.fix-bug-123  # path within env_ref's filesystem

  # Exactly one strategy variant populated:
  worktree:
    clone_ref: flotilla-flotilla-org-host-direct-01HXYZ
  # fresh_clone:
  #   url: https://github.com/...

status:
  phase: Ready                             # Pending | Preparing | Ready | Terminating | Failed
  path: /Users/alice/dev/flotilla.fix-bug-123    # echoes spec.target_path once Ready
  commit: 44982740...
  message: null
```

- **Strategy variants:**
  - `worktree { clone_ref }` — `git worktree add` from a Clone in the same Environment. Lightweight; default for typical use.
  - `fresh_clone { url }` — `git clone <url> <target_path>` directly. No Clone resource needed. Used for env-internal cases (k8s_pod future, or docker `fresh_clone_in_container`) and standalone clones.
  - `local_clone` deferred — copy from a local Clone (separate clone on disk, not a worktree). Rare; can be added without disturbing existing variants.
- **Validation rules:**
  - Worktree's `clone_ref` must reference a Clone with the same `env_ref`. Cross-env worktrees are not possible (`git worktree add` needs the parent on the same filesystem).
  - Branch refs only in v1; sha/tag/detached-head deferred.
- Default ownership: per-task (owned by TaskWorkspace, GC-cascades). Shared persistent checkouts are deferred.
- **Finalizer** `flotilla.work/checkout-cleanup` runs `git worktree remove` (worktree variant) or `rm -rf target_path` (fresh_clone) before deletion.

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
  env_ref: alice-docker-dev-task-123                # env where the process runs
  role: coder                                       # informational
  command: "claude --prompt '…'"                    # literal command to wrap
  cwd: /workspace                                   # path within env_ref's filesystem
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

The per-task bundle. **Created by the Convoy reconciler** as part of its declarative reconciliation. The convoy reconciler examines each task's state on every pass and ensures the right TaskWorkspace exists by deterministic name (idempotent — `AlreadyExists` on actuation is success). Owned by the parent Convoy. The `TaskWorkspaceReconciler` then takes over to provision children. See "Convoy reconciler extension" below for the full per-task logic.

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
    flotilla.work/url-slug: flotilla-flotilla-org     # slug(convoy.spec.repository.url); lets Clone→TaskWorkspace join
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
    checkout:
      strategy: worktree               # only variant supported on host_direct in v1
  # docker_per_task:
  #   host_ref: 01HXYZ...
  #   image: ghcr.io/flotilla/dev:latest
  #   default_cwd: /workspace
  #   env: { FOO: bar }
  #   checkout:
  #     # Exactly one sub-variant populated.
  #     worktree_on_host_and_mount:
  #       mount_path: /workspace       # container path the host worktree is mounted at
  #     # fresh_clone_in_container:
  #     #   clone_path: /workspace     # container path to clone into
```

- Two variants in Stage 4a: `host_direct` and `docker_per_task`. Future variants (`docker_shared`, `k8s_pod`, `runpod`, `meta_policy` delegating to a Quartermaster) are deferred.
- **`host_ref` lives per-variant**, not at top level — some future variants (RunPod, meta-policy) don't bind to a specific host.
- **`pool` is at top level** because it applies uniformly to anything that creates terminal sessions.
- **`checkout` lives per-variant** because the strategies that make sense depend on the placement shape. `host_direct` only permits `worktree` in v1 (fresh clone onto the host is possible but pointless when the host already has a Clone). `docker_per_task` offers two sub-variants: `worktree_on_host_and_mount` (cheap, shares fetch state; requires a Clone on the host) and `fresh_clone_in_container` (isolated, no host dependency; slower). `local_clone` is deferred.
- **No status, no controller for PlacementPolicy itself** — pure data, like WorkflowTemplate. The `TaskWorkspaceReconciler` consults it during reconciliation.
- **Daemon-created defaults** at startup (see Daemon Startup section): one `host-direct-<host>` policy and (if docker is available) one `docker-on-<host>` policy (defaulting to `worktree_on_host_and_mount`). User can edit, replace, or add custom ones.

### `ConvoySpec` extensions (Stage 4a)

Stage 4a extends Stage 3's `ConvoySpec` with two new fields — a logical repo identity and a branch. The convoy names the *logical* repo (by URL); the provisioning layer materializes per-env Clones as tasks need them. The convoy never references a specific Clone resource, because that would bake a placement decision into the caller.

```yaml
spec:
  workflow_ref: fix-bug-123            # existing (Stage 3)
  inputs: { … }                        # existing (Stage 3)
  placement_policy: …                  # existing (Stage 3)

  repository:                          # NEW: logical repo identity (URL-only in Stage 4a)
    url: https://github.com/flotilla-org/flotilla
  ref: feat/convoy-resource            # NEW: branch (v1); sha/tag deferred
```

- **`repository`** is inline, not a resource reference. In Stage 4a it only carries `url`. This is the invariant identity of the repo across any clones that materialize in any envs. A future logical `Repository` resource would move this into a reference (`repository_ref: foo-bar`), with `url` living on the Repository. The nested-object shape (`repository: { … }`) is forward-compatible: adding a `ref` (resource-name) field or moving fields out later doesn't reshape callers.
- **`ref`** is the branch used by the convoy's Checkouts. Convoy-level rather than per-task because every task in a Stage 4a convoy works the same branch. Per-task overrides, sha/tag refs, detached-head, and shared-branch coordination are deferred.
- Both fields are immutable after create (CEL validation), matching Stage 3's immutability story for `workflow_ref`.
- The TaskWorkspaceReconciler consumes both: `convoy.spec.ref` flows into `CheckoutSpec.ref`; `convoy.spec.repository.url` flows into per-env Clone materialization (see reconcile flow).

### Resource interactions and ownership summary

| Resource | Owned by | Finalizer | Created by |
|----------|----------|-----------|------------|
| Host | nobody | none | daemon (self via heartbeat task) |
| Environment (host_direct) | nobody | none | daemon (auto-created at startup) |
| Environment (docker_per_task) | TaskWorkspace | docker-teardown | TaskWorkspaceReconciler |
| Clone | nobody | clone-cleanup (rare; explicit delete only) | daemon (discovery), user, or TaskWorkspaceReconciler (on-demand per-env) |
| Checkout | TaskWorkspace | checkout-cleanup | TaskWorkspaceReconciler |
| TerminalSession | TaskWorkspace | terminal-teardown | TaskWorkspaceReconciler |
| TaskWorkspace | Convoy | none (children carry finalizers) | Convoy reconciler (extended in Stage 4a) |
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

pub struct ObjectMeta {
    pub name: String,
    pub owner_ref: Option<ResourceRef>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

pub enum Actuation {
    CreateEnvironment     { meta: ObjectMeta, spec: EnvironmentSpec },
    CreateClone           { meta: ObjectMeta, spec: CloneSpec },
    CreateCheckout        { meta: ObjectMeta, spec: CheckoutSpec },
    CreateTerminalSession { meta: ObjectMeta, spec: TerminalSessionSpec },
    CreateTaskWorkspace   { meta: ObjectMeta, spec: TaskWorkspaceSpec },
    DeleteResource        { kind: ResourceKind, name: String },
}
// CreateClone is safe because URL comes from convoy.spec.repository.url (one
// authoritative source) and naming is deterministic `<url-slug>-<env>`, so
// concurrent tasks converge on the same Clone via AlreadyExists-is-success.

/// A secondary watch is spawned alongside the primary watch and feeds primary
/// keys into the shared reconcile channel. Each impl handles one Watched
/// resource type and a mapping back to primary names. The trait is
/// object-safe (no Watched associated type leaks through `dyn`); concrete
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

// Typed helpers for the two common shapes (concrete impls use these internally,
// not the dyn-erased trait above):

/// Direct: watched object carries a label whose value IS the primary's name.
/// Fires one primary-name enqueue per watched event. Used for owned-child
/// relationships: e.g. TerminalSession labelled with
/// `flotilla.work/task_workspace: <name>` wakes exactly that TaskWorkspace.
pub struct LabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,    // e.g. "flotilla.work/task_workspace"
    pub _marker: PhantomData<(W, P)>,
}

/// Join: watched object carries a label whose value matches the same label
/// on zero or more primaries. On a watched event, the framework reads the
/// label value, lists primaries where the same label key has that value, and
/// enqueues every match. Used for shared-referent fan-out: e.g. a Clone
/// labelled `flotilla.work/url-slug: foo-bar` wakes every TaskWorkspace
/// carrying the same `flotilla.work/url-slug: foo-bar` label.
///
/// Implementation uses the backend's label-selector list (already required by
/// the primary watch's initial list path, so no new backend capability).
pub struct LabelJoinWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,    // e.g. "flotilla.work/url-slug"
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
- **Secondary watches**: one task per secondary spawned alongside the primary watch. Two typed helpers cover the mapping patterns we need: `LabelMappedWatch` (watched's label value *is* a primary name — 1:1 owned-child case) and `LabelJoinWatch` (watched's label value matches the same label on zero-or-more primaries — shared-referent fan-out). Custom `SecondaryWatch` impls can do anything the trait allows; the helpers just cover the common shapes cleanly.
- **`ObjectMeta` on `Create*` actuations**: every create carries a full `ObjectMeta { name, owner_ref, labels, annotations }`, not just a name. Labels drive secondary-watch routing (e.g. `flotilla.work/convoy`, `flotilla.work/task_workspace`, `flotilla.work/role`), so they must flow through actuation rather than being bolted on after creation. Keeping the metadata bundle in one struct means future additions (e.g. annotations for observability) don't require widening every variant.
- **Shared reconcile channel**: all watches push primary keys into one mpsc channel. A worker dequeues, dedupes consecutive entries for the same key, fetches the primary by name, calls `reconcile`, then enacts each `Actuation` first and applies the typed patch via `apply_status_patch` afterwards.
- **Actuate-then-patch ordering, with idempotent actuations.** The framework enacts actuations *before* applying the status patch. If an actuation fails, no patch lands and the task stays in its previous phase, so the next reconcile pass retries cleanly. All actuations are idempotent: `Create*` is name-keyed (`AlreadyExists` is success); `DeleteResource` treats `NotFound` as success. Reconcilers should not rely on single-pass atomicity; they must be written to converge over multiple passes.
- **No cross-resource status patches as actuations.** Every reconciler patches only its own resource's status. State propagation between resources happens via observation: a reconciler watches sibling/parent resources (as secondaries) and reacts to their status changes during its own reconcile pass. This eliminates a whole class of cross-resource ordering hazards (the parent advancing ahead of the child) and keeps single-writer-per-status discipline.
- **Resync ticker** (~60s): periodically pushes every known primary key. Standard k8s safety net.
- **Finalizer handling**: on a primary with `metadata.deletionTimestamp` set and the configured finalizer present, call `reconciler.run_finalizer(...)`, then patch the resource to remove the finalizer entry, then let GC complete.

### Conflict and retry

`apply_status_patch` already handles status-write conflicts via read-modify-write retry. Actuations (creates) are idempotent by name — the loop checks before creating. Deletes are tolerant of NotFound.

### Refactor of Stage 3's convoy controller

Mechanical: implement `Reconciler` for `ConvoyReconciler`, instantiate `ControllerLoop` in the daemon. No behavior change. Same tests pass.

## Provisioning controllers (Stage 4a, in `flotilla-controllers`)

One reconciler per resource type. All run in the daemon. Each uses `ControllerLoop` from the framework.

### `HostHeartbeatTask` (not a `Reconciler`)

Host doesn't really need a reconciler — there's no useful "reconcile other daemons' Hosts" work. A `Reconciler` watching Host resources would race: every daemon would try to reconcile every Host, including ones owned by other daemons, corrupting heartbeat / capabilities.

Instead, each daemon spawns a simple `HostHeartbeatTask`: a periodic background task (every ~30s) that updates *only the daemon's own* `Host` record (`name = local_host_id`). It refreshes `heartbeat_at`, recomputes `ready`, and writes the current capability snapshot. Other daemons' Host records are never written by this task.

Other daemons' Host records get *read* by consumers (notably `TaskWorkspaceReconciler` checking host staleness before placing work), but never written. Single-writer per Host record by construction.

No finalizer. When a daemon stops, the heartbeat just stops updating; the staleness check at consumers handles the consequence.

### `EnvironmentReconciler`

Watches Environment resources (primary). No secondaries. Branches on `spec.<kind>`:

- `host_direct`: no actuation, immediately `Ready`.
- `docker`: calls flotilla-core's Docker provider (`ensure_image` → `create`). Updates `status.docker_container_id`, transitions to `Ready` when the container is up, `Failed` on error.

Finalizer: `flotilla.work/environment-teardown` on `docker_per_task` only (runs `docker rm -f`). `host_direct` Environments carry no finalizer.

### `CloneReconciler`

Watches Clone resources (primary). No secondaries. On a Clone whose `status.phase` is `Pending`: calls flotilla-core's git layer to clone `spec.url` into `spec.env_ref`'s filesystem at `spec.path`. Updates `status.default_branch` once the clone completes; transitions to `Ready`. On a discovery-marked Clone whose path already exists, just verifies and transitions to `Ready` without re-cloning.

Finalizer: `flotilla.work/clone-cleanup`. Stage 4a default for the cleanup itself: do nothing (Clones are persistent; deleting the resource doesn't remove the on-disk clone). Explicit-delete-with-cleanup is a future opt-in.

### `CheckoutReconciler`

Watches Checkout resources (primary). No secondaries. Branches on strategy variant:

- `worktree`: looks up the referenced Clone (must be `Ready`, must have matching `env_ref`), runs `git worktree add` to `spec.target_path`.
- `fresh_clone`: runs `git clone <spec.fresh_clone.url> <spec.target_path>` directly.

Updates `status.path`, `status.commit`, transitions phases.

Finalizer: `flotilla.work/checkout-cleanup`. `git worktree remove` for worktree variant; `rm -rf target_path` for fresh_clone.

### `TerminalSessionReconciler`

Watches TerminalSession resources (primary). No secondaries. Looks up the referenced Environment (must be `Ready`), calls flotilla-core's `TerminalPool` (cleat / shpool / passthrough) to start a wrapped session. Updates `status.session_id`, `status.phase`. Tracks the inner command's status as informational fields.

The pool implementation handles the shell-wrapping behavior — TerminalSession spec carries the literal command, the pool wraps it.

Finalizer: `flotilla.work/terminal-teardown`. Stops the session and releases the pool entry.

### `TaskWorkspaceReconciler`

Watches TaskWorkspace (primary) plus Environment, Checkout, TerminalSession as owned-child secondaries via `LabelMappedWatch` on `flotilla.work/task_workspace` (1:1 mapping — the watched object's label value *is* the TaskWorkspace name). Clone is a shared-referent secondary via `LabelJoinWatch` on `flotilla.work/url-slug`: Clone carries the url-slug label, TaskWorkspace carries it too (written at creation from the parent Convoy's `spec.repository.url`), and a Clone status change enqueues every TaskWorkspace with the same slug. This wakes the right reconcilers without a 60s resync wait and without inventing a framework mechanism beyond the two typed helpers.

**Reconcile flow — ordering depends on the policy's checkout strategy.** The three supported strategies produce three dependency chains:

- **`host_direct` + `worktree`**: Clone (host-direct env) → Checkout (host-direct env) → Environment (no-op: reuse host-direct env) → TerminalSessions.
- **`docker_per_task` + `worktree_on_host_and_mount`**: Clone (host-direct env of the policy's host) → Checkout (host-direct env) → Environment (docker env, mounts the Checkout path) → TerminalSessions.
- **`docker_per_task` + `fresh_clone_in_container`**: Environment (docker env) → Checkout (in the docker env, `fresh_clone` strategy, URL from `convoy.spec.repository.url`) → TerminalSessions. No Clone resource is created — the clone happens *inside the container* via the Checkout's `fresh_clone` strategy.

The reconciler computes the dependency order from the strategy and drives the per-pass ensure logic accordingly. Written declaratively, every pass:

1. **Resolve PlacementPolicy** via `placement_policy_ref`. Missing → `Failed`; the Convoy reconciler observes and propagates.
2. **Read parent Convoy's `spec` and `status.workflow_snapshot`** for `repository.url`, `ref`, process definitions, and inputs.
3. **Ensure Clone** (when strategy needs one — skipped for `fresh_clone_in_container`). Compute the clone env (the host-direct env of the policy's host) and the deterministic Clone name: `<url-slug(convoy.spec.repository.url)>-<clone_env_ref>`. Look it up:
   - Not found → emit `CreateClone` actuation with `spec.url = convoy.spec.repository.url`, `spec.env_ref = clone_env_ref`, `spec.path = <clone_env.spec.host_direct.repo_default_dir>/<url-slug>`. Labels include `flotilla.work/url-slug` so the secondary watch maps back here. On `AlreadyExists` (a peer task got there first), treat as success and proceed.
   - Found → reuse. Clone is shared by URL+env across all tasks and all convoys.
   Then wait for `phase: Ready`.
4. **Ensure Environment** (only before Checkout for `fresh_clone_in_container`; otherwise after Checkout). Branch on policy variant:
   - `host_direct` → reuse the shared host-direct Environment for the host; write `status.environment_ref`. No creation.
   - `docker_per_task` + `fresh_clone_in_container` → if `status.environment_ref` unset, emit `CreateEnvironment` with no checkout mount (the clone happens inside the container). Wait until `Ready`.
   - `docker_per_task` + `worktree_on_host_and_mount` → deferred until after the Checkout is Ready (see step 6).
5. **Ensure Checkout.** If `status.checkout_ref` unset, emit `CreateCheckout` (owned by this TaskWorkspace) with:
   - `env_ref`: the clone env for both worktree strategies; the just-created docker env for `fresh_clone_in_container`.
   - `ref`: `convoy.spec.ref`.
   - Strategy variant: `worktree { clone_ref: <Clone name from step 3> }` for both worktree strategies; `fresh_clone { url: convoy.spec.repository.url }` for `fresh_clone_in_container`.
   Wait until the Checkout reaches `Ready`.
6. **Ensure Environment (worktree-and-mount only).** For `docker_per_task` + `worktree_on_host_and_mount`: if `status.environment_ref` unset, emit `CreateEnvironment` with mounts derived from `Checkout.status.path` (mount `source_path = Checkout.status.path`; `target_path = policy.docker_per_task.checkout.worktree_on_host_and_mount.mount_path`). Wait until `Ready`. Skipped for the other two strategies (Environment already resolved).
7. **Ensure TerminalSessions**, one per process in the task's snapshot. If a session for a given role is missing, emit `CreateTerminalSession` with `env_ref` = the chosen Environment, `cwd` derived from policy + Environment (for `host_direct`: `Checkout.status.path`; for `worktree_on_host_and_mount`: `policy.docker_per_task.checkout.worktree_on_host_and_mount.mount_path`; for `fresh_clone_in_container`: `policy.docker_per_task.checkout.fresh_clone_in_container.clone_path`; further overridden by `policy.docker_per_task.default_cwd` if set), `pool` from policy.
8. **All Ready** → patch own `status.phase = Ready`. The Convoy reconciler observes this change via its TaskWorkspace secondary watch and patches the convoy task to `Running` as part of its own next reconcile pass.

**Multi-host / cross-env placement.** Because Clone materialization is keyed on (URL, env), a future multi-host PlacementPolicy (e.g. Task 1 on host A, Task 2 on host B) "just works": each task's `TaskWorkspaceReconciler` pass computes its own clone env, looks up or creates the Clone there, and proceeds. Host A and Host B each end up with their own Clone of the same logical repo; they converge independently. The convoy's `repository.url` is the single point of truth; no placement information leaks into the convoy itself.

Failure at any step: own `status.phase = Failed` with a clear message. The Convoy reconciler observes Failed TaskWorkspace status and patches the convoy task to `Failed` as part of its own reconcile. No automatic retry.

`TaskWorkspaceReconciler` never patches another resource's status. Cross-resource state propagation is observation-driven, not actuation-driven.

No finalizer on TaskWorkspace itself; child finalizers handle external state.

### Convoy reconciler extension (Stage 4a)

The convoy reconciler from Stage 3 grows TaskWorkspace responsibilities. The new logic is **fully declarative** — every reconcile pass examines each task's current state plus the corresponding TaskWorkspace's status, and produces whichever patches/actuations are needed to make them consistent. Combined with the framework's actuate-then-patch ordering and the no-cross-resource-patch rule, this is robust to transient failures of either the actuation or the patch.

The convoy reconciler watches Convoys (primary) and TaskWorkspaces (secondary, mapped via `flotilla.work/convoy: <convoy-name>` label).

**Bootstrap-time agent-process rejection (Stage 4a).** When the Stage 3 bootstrap path builds the `workflow_snapshot`, it now also checks every `ProcessDefinition` in the resolved WorkflowTemplate. If any process is a `ProcessSource::Agent`, bootstrap fails with `ConvoyStatusPatch::FailInit` carrying the message `"Stage 4a supports tool processes only; agent processes require selector resolution (Stage 4b)."` The convoy transitions straight to `Failed` — no tasks are ever marked `Ready`, no TaskWorkspaces are ever created. This is fail-fast: users get the error at apply time, not after provisioning partial state. Selector resolution in Stage 4b will remove this check.

Per-task logic on every reconcile pass — first looks up the TaskWorkspace by deterministic name (`<convoy>-<task>`):

- **`Pending`**, deps satisfied → emit `MarkTaskReady` patch (Stage 3 unchanged).
- **`Ready`**:
  - If TaskWorkspace doesn't exist → emit `CreateTaskWorkspace` actuation. The actuation's `ObjectMeta.labels` include `flotilla.work/convoy: <convoy>`, `flotilla.work/task: <task>`, and `flotilla.work/url-slug: slug(convoy.spec.repository.url)` — the last of which is the key that `LabelJoinWatch` uses to wake this TaskWorkspace when its Clone transitions. Idempotent; `AlreadyExists` is success.
  - If TaskWorkspace exists with `status.phase == Failed` → emit `MarkTaskFailed` patch with the workspace's failure message. A task is not considered launched just because the TaskWorkspace object exists.
  - If TaskWorkspace exists with `status.phase == Ready` → emit `MarkTaskLaunching` patch. This is the point where `started_at` and placement metadata become true for the convoy task.
  - Otherwise → no work; wait for next reconcile.
- **`Launching`**:
  - If TaskWorkspace doesn't exist → emit `CreateTaskWorkspace` (recovers from prior actuation failure or lost watch event).
  - If TaskWorkspace exists with `status.phase == Ready` → emit `MarkTaskRunning` patch (the observation-driven propagation that replaces the old `PatchConvoyTask` actuation).
  - If TaskWorkspace exists with `status.phase == Failed` → emit `MarkTaskFailed` patch with the workspace's failure message.
  - Otherwise → no work; wait for next reconcile.
- **`Running`**:
  - If TaskWorkspace's `status.phase == Failed` → emit `MarkTaskFailed` (a workspace failing while the task is Running, e.g. terminal session crash, propagates).
  - Otherwise → no convoy-side work; explicit completion comes from an external actor (CLI, future agent CLI).
- **`Completed | Failed | Cancelled`** → terminal; phase rollup applies.

Single-writer discipline preserved: the convoy reconciler is the only thing that writes to convoy task phase. TaskWorkspaceReconciler only writes to its own TaskWorkspace status. State propagates between them via observation.

## Daemon startup

The daemon at startup, after connecting to the resource backend:

1. **Self-register as Host**: create-or-update a Host resource for itself (using the existing persistent host id as the resource name). Spawn a periodic heartbeat task that updates `Host.status.heartbeat_at` every ~30s.
2. **Create the host-direct Environment** for itself if not present. Includes `repo_default_dir` from existing flotilla-core config or a sensible default. Idempotent.
3. **Discover Clones**: scan flotilla-core's repo registry (`~/.config/flotilla/repos/*.toml`). The registry is path-only; the canonical URL lives in the on-disk clone's git config, not in flotilla's config. For each registered path:
   1. Run `git remote get-url origin` against the path. If that fails (no origin remote, bare local repo, renamed remote), log a warning and skip this entry — a Clone without a URL can't be matched against any convoy's `repository.url`.
   2. Compute the deterministic name (`<url-slug>-<host-direct-env-ref>`).
   3. Create-or-update the Clone resource with `spec.url` = the derived URL, `spec.env_ref` = the host-direct env, `spec.path` from the registry, and labels `flotilla.work/discovered: "true"` + `flotilla.work/url-slug: <slug>` + `flotilla.work/env: <env-ref>`.
   Idempotent across daemon restarts; if the user manually renamed a remote or moved a path, the next discovery pass picks up the current URL.
4. **Create default PlacementPolicies**:
   - Always: `host-direct-<host-id>` (variant: `host_direct`).
   - If `Host.status.capabilities.docker == true`: `docker-on-<host-id>` (variant: `docker_per_task`, with a sensible default image).
5. **Spawn the `HostHeartbeatTask`** (per-daemon background task; not a controller).
6. **Spawn all controller loops**: EnvironmentReconciler, CloneReconciler, CheckoutReconciler, TerminalSessionReconciler, TaskWorkspaceReconciler, ConvoyReconciler (refactored from Stage 3, extended to ensure TaskWorkspaces and observe their status as described above; declarative per-pass).

This is the "discovered resources" pattern in its simplest form: the daemon creates the resources that describe its own capabilities, and they lifecycle out of band from user interaction. User can edit or replace any of them; the daemon doesn't keep regenerating.

## Failure handling

- **Reconcile failure** within a reconciler → `phase = Failed` + message + propagation to the next layer up. TaskWorkspace failures propagate to Convoy via `MarkTaskFailed`.
- **Stage 3's fail-fast applies.** Once any task is `Failed`, the parent Convoy goes `Failed` (Stage 3 reconciler), siblings get cancelled, and the convoy stops reconciling — terminal phase. There is **no per-task retry** in Stage 4a. Recovery means creating a new Convoy. Per-task restart policies / explicit retry UX are deferred (already in the deferred list).
- **No automatic in-reconciler retry** either. A Failed Environment / Checkout / TerminalSession stays Failed; the TaskWorkspace fails; the Convoy fails. User intervention is creating a fresh Convoy from the same WorkflowTemplate.
- **Heartbeat staleness on Host**: the `TaskWorkspaceReconciler` refuses to place new children on a Host whose `ready: false` or `heartbeat_at` is older than ~60s. TaskWorkspaces already provisioned on a now-stale host are eventually marked `Failed` after extended staleness; full "host comes back, what do we do" is a future Bosun-style concern.
- **Cancellation cascades** from Stage 3's convoy controller: when a task is patched to `Cancelled` (fail-fast), TerminalSessions for that task stay alive until TaskWorkspace cascades on Convoy deletion. Auto-cleanup-on-cancellation is a future policy.

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

1. `controller` module: `Reconciler` trait, `SecondaryWatch` trait plus `LabelMappedWatch` (1:1) and `LabelJoinWatch` (shared-referent fan-out) typed helpers, `ControllerLoop`, `Actuation` enum (with `ObjectMeta` struct carrying name + owner_ref + labels + annotations on every `Create*` variant), `ReconcileOutcome`.
2. Refactor Stage 3 convoy controller to implement `Reconciler` (mechanical, no behavior change).
3. Framework tests.

### Stage 4a proper

4. New crate `flotilla-controllers`.
5. Seven new CRDs: `Host`, `Environment`, `Clone`, `Checkout`, `TerminalSession`, `TaskWorkspace`, `PlacementPolicy`. CEL immutability where applicable.
6. Rust types for each + `StatusPatch` enums + per-resource reconcilers (five reconcilers: Environment, Clone, Checkout, TerminalSession, TaskWorkspace) plus a `HostHeartbeatTask` (not a reconciler).
7. Three actuators wrapping existing flotilla-core providers: Docker (Environment), CheckoutManager (Clone + Checkout), TerminalPool (TerminalSession).
8. **`ConvoySpec` extensions**: add `repository: { url: String }` (inline logical identity) and `ref: String` fields, immutable after create (CEL). Update serde types, Stage 3 bootstrap, and tests.
9. **Convoy reconciler extension** (Plan A2 or A3): per-task declarative logic that ensures a TaskWorkspace exists and propagates TaskWorkspace status into convoy task phase via observation (no cross-resource patches). Adds TaskWorkspace as a secondary watch with the `flotilla.work/convoy` label mapping. Grow the `Actuation` enum with `CreateTaskWorkspace` + `CreateClone`. Bootstrap rejects workflows containing `ProcessSource::Agent` with `FailInit` until Stage 4b ships selector resolution.
10. Daemon startup logic: self-register as Host, create host-direct Environment, discover existing Clones from flotilla-core registry, create default PlacementPolicies, spawn the heartbeat task and all controller loops.
11. **CLI completion path** (touches several crates, extending the existing `Command`/`CommandAction` vocabulary):
    - `flotilla-protocol` — new `CommandAction::MarkConvoyTaskComplete { namespace, convoy, task }` variant on the existing `CommandAction` enum (sent through the existing `Request::Execute { command }` flow). Matching success/error shape on the existing result type.
    - `flotilla-core` — extend the daemon-level command handling path so `CommandAction::MarkConvoyTaskComplete` is recognized as a daemon-scoped action rather than a per-repo executor plan step. `InProcessDaemon` / the core executor boundary needs to route it to the resource patching path cleanly.
    - `flotilla-daemon` — extend the existing command dispatcher / daemon-scoped command handling with a handler for the new `CommandAction` that validates the request and calls `apply_status_patch::<Convoy>(...)` with `ConvoyStatusPatch::MarkTaskCompleted`.
    - `flotilla-client` — no new protocol shape is required beyond the existing `execute()` path. The CLI can build a `Command` with the new `CommandAction` and send it via `Request::Execute`; a small convenience helper is optional but not required at the `DaemonHandle` trait boundary.
    - `flotilla` binary — CLI subcommand `flotilla convoy <name> task <task> complete [--namespace <ns>]` building the new `CommandAction` and invoking `execute()`.
    Future short-form (`flotilla complete` driven by env-var context) is deferred.
12. New `flotillad` binary target in `flotilla-daemon`.
13. `flotilla` TUI binary's embedded-daemon mode removed entirely (Stage 4a cuts the cord).
14. Tests at every layer (pure reconcile, StatusPatch::apply, framework, actuator, in-memory end-to-end, minikube integration, docker actuator integration, finalizer behavior).
15. CRD bootstrap via `ensure_crd` for example/integration paths.

## Design Decisions

### Path C: flotilla-daemon placement now, k8s placement deferred

A "productive" k8s Pod backend needs a runnable image, a checkout mechanism for the cluster, per-tool config preparation, and selector resolution — each a real design problem. Stage 4a uses the existing `WorkspaceOrchestrator` and providers in `flotilla-core` to ship a productive prototype on day one, without solving any of those four. K8s placement (Stage 4k) gets its own brainstorm where image / checkout / config can be designed honestly. The 2x2 of state × placement (flotilla-cp vs k8s × flotilla-daemon vs k8s-cluster) makes both columns valid; we're shipping the left column first.

### Per-layer resources, not a single bundled resource

Six resources (Host, Environment, Clone, Checkout, TerminalSession, TaskWorkspace) instead of one bundled `Workspace` resource. Each existing flotilla provider concept gets its own resource shape with its own lifecycle, finalizer, and visibility. Costs more upfront than a single resource but pays off for: independent inspection and labelling (`kubectl get terminalsessions -l role=coder`), clear ownership boundaries, future per-resource controllers, and the agent-era model where a Yeoman or Bosun watches per-resource events. Underspecifying the cuts now would force a much larger disaggregation transition later.

### Env-relative model (everything has `env_ref`, not `host_ref`)

Clone, Checkout, and TerminalSession all carry `env_ref` rather than `host_ref`. The host's bare filesystem is just one kind of Environment (`host_direct`). Cloning into a k8s pod is the same act as cloning anywhere else — the pod's filesystem is just another Environment kind.

Treating the host as a special case of Environment, rather than a peer concept, removes a discriminator that would otherwise ossify in every related resource. When env-internal cases arrive (k8s_pod, future runpod), the schema doesn't need to change; new resources just point `env_ref` at the new Environment kind. Mounts are minimal (`source_path + target_path + mode`) — the source is implicitly the env's host's filesystem, and a future cross-env mount story would add `from_env` then. Joined-up summary views show the resolved picture; the spec stays minimal.

### Convoy names the logical repo by URL; per-env Clones materialize under placement

The convoy carries `repository: { url }` inline, not a reference to a specific Clone resource. This matters because placement decides which env(s) a convoy's tasks run in — and a convoy tied to "the Clone on host A" would break the moment placement picked host B, or split tasks across both. The `TaskWorkspaceReconciler` materializes (or reuses) a Clone per `(url, env)` pair as each task is placed. The URL lives in exactly one place (on the convoy); every Clone's `spec.url` matches that URL. Deterministic Clone naming (`<url-slug>-<env>`) makes the lookup a keyed read, and `AlreadyExists` on `CreateClone` is success — so concurrent tasks targeting the same env-clone converge cleanly. A future logical `Repository` resource (carrying URL, aliases, default-branch declared, ChangeRequestTracker/IssueProvider config anchor, queryable "all clones of this repo") would turn the inline field into a `repository_ref` without rewriting any of this flow.

### One CRD per concept, kind-discriminator inside

Environment is one CRD with `host_direct` / `docker` / future variants distinguished by field presence (untagged enum on the Rust side, `oneOf` on the CRD). Same shape we proved with `ProcessSource` in WorkflowTemplate. Polymorphic resource references (`{kind: DockerEnvironment, name: foo}`) are ugly in YAML and require every consumer to case-switch — a single resource with an internal discriminator keeps references clean (`environment_ref: foo`).

### Mounts on Environment.spec, written at creation

Mounts are static fields on Environment, populated by the `TaskWorkspaceReconciler` when it creates a per-task Environment. The Environment controller never touches them; it reads its own spec and actuates. Path-coordination across resources happens in one place (the TaskWorkspaceReconciler) at one time (creation), not via cross-controller patching.

### PlacementPolicy is referenced data, not just a name

PlacementPolicy is a real resource with a CRD, status-less, like WorkflowTemplate. Daemon auto-creates defaults at startup; users can author custom policies via YAML. Not just a string config — a resource so it can be inspected, labelled, eventually selected by labels, and one day pointed at a Quartermaster agent.

### TerminalSession models the outer shell wrapper

The pool wraps the configured command in a shell so process exits don't leave hung terminals — current flotilla pain. TerminalSession's lifecycle is the wrapper's lifecycle; inner-command exit is observed but informational. Maps cleanly onto a future Bosun agent that handles restart/repair behavior.

### Per-resource controllers (option B), with framework extraction

Five narrow reconcilers (one per resource type with non-trivial reconciliation) on top of a small `ControllerLoop` framework. The framework extraction (Stage 1 layer) is small (~200-300 lines) and benefits every controller from now on. Without the framework, "boilerplate" was the argument for option A (one controller); with the framework, B is unambiguously cleaner. Each reconciler's tests are scoped, future variants of each resource type are local additions, and the foundation is in place for cluster-native deployment splitting controllers across processes.

### Dedicated `flotillad` binary; embedded-daemon mode removed

The `flotilla` TUI binary's embedded-daemon mode never quite earned its keep — multiple TUI windows want to share state (which forces daemon-as-process), CLI dies with TUI, and providing controllers to an embedded daemon means a TUI-binary dep on `flotilla-controllers` (very wide). Stage 4a cuts the cord entirely: a separate `flotillad` is the only production path for running controllers; `flotilla` becomes pure client/TUI. Tests that want everything-in-one-process compose `InProcessDaemon` plus controllers via a small test-support helper crate.

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
- **Logical `Repository` resource** — Stage 4a only has `Clone` (one per URL+env). A future `Repository` would be the URL-level identity: canonical URL + aliases/mirrors, declared default branch (distinct from observed), GitHub/GitLab owner+slug for the existing ChangeRequestTracker/IssueProvider config to hang off, the anchor for cross-env "show me all clones of this repo" queries. When it lands, `ConvoySpec.repository: { url }` becomes `ConvoySpec.repository_ref: <name>` and `Clone` gains a `repository_ref` back-pointer.
- **Clone extensions** — Stage 4a's Clone carries URL + env_ref + path + default_branch. Future additions: credentials, per-clone workspace.yaml location, badge metadata, default-checkout tracking (the working tree you get from a non-bare clone vs. explicit Checkout resources). Each is an additive field.
- **Detached-head / sha / tag refs on Checkout** — useful for agent-driven bisect workflows and pinned-version provisioning.
- **Shared Docker environments** as a placement variant — needs the shared-env-plus-per-task-checkout composability question solved.
- **Meta-policy variant** for PlacementPolicy — delegate to a Quartermaster agent that picks among other policies.
- **TUI/CLI binary split** — separate the TUI from the CLI in `flotilla` as the next structural cleanup.
- **Lease-based leader election** for controllers — carried over from Stage 3 deferred list.
- **Per-task restart policies / explicit retry UX** — a way to say "retry this failed task" without manually deleting resources.
- **Auto-cleanup of stopped sessions on terminal task transitions** — opt-in policy field.
- **Vessel / Crew / Shipment naming pass** — convoy-themed renames once the abstractions settle (TaskWorkspace → Vessel, processes → Crew, artifacts → Shipment).
- **VCS abstraction in resource shape** — Clone and Checkout are git-shaped in v1; future `vcs:` discriminator for hg / fossil / etc.
- **Exit-code-as-completion opt-in** — for tasks whose configured "progress-bearing" process should drive task completion (e.g. a one-shot `cargo test`). Requires extending `TerminalPool` to surface inner-command exit events plus a per-task / per-process opt-in flag. Watcher-kind processes (test runners, dev servers, log tails) explicitly opt out — they're informational and never complete by design.
- **CLI shortcutting for task completion** — env-var-derived context (`FLOTILLA_CONVOY`, `FLOTILLA_TASK`, `FLOTILLA_ROLE`) propagated into terminal sessions so a process can call `flotilla complete` without the long form. Part of a wider story about how `flotilla` CLI infers context from the calling environment.
- **Per-task retry / convoy-task reset** — once richer workflows are in flight, we'll want to mark a failed task for retry without recreating the whole convoy. Today's Stage 3 fail-fast + Stage 4a's "no in-place retry" forces convoy recreation. Likely needs a new convoy-controller patch variant (`ResetTaskToPending` or similar) plus a corresponding CLI affordance.
- **Richer convoy CLI surface** — Stage 4a ships exactly one CLI verb (`task complete`). Future verbs (create, list, inspect, cancel, mark-failed, kill-session, reset-task, …) each currently require their own `CommandAction` variant + client method + daemon handler. If this surface grows large enough that per-verb protocol bloat becomes annoying, revisit a generic "apply this typed status patch" shape with serialised payloads — but that has its own ergonomic costs (protocol-resource type coupling).
- **Client/daemon protocol convergence with resource-management protocol** — the long-term direction: the client/daemon protocol probably becomes essentially HTTP-over-UDS, mirroring the resource-management protocol that controllers already speak. This eliminates per-verb protocol mapping and naturally supports remote-HTTP daemons. Stage 4a is the hybrid middle, where the existing `Command`/`CommandAction` surface coexists with the new resource-shaped flows. Once the one-task-convoy story works end-to-end, we can start safe cruft-cutting on the protocol layer.

## Plan structure

Stage 4a is large enough that a single implementation plan is unwieldy. The recommended split is three plans along the natural seams of dependency:

### Plan A1 — Controller framework + Stage 3 refactor

Add `Reconciler`, `ControllerLoop`, `SecondaryWatch`, `Actuation`, `ReconcileOutcome` to `flotilla-resources`. Refactor Stage 3's convoy controller to implement `Reconciler` (mechanical, no behavior change). Framework tests.

Small, self-contained, mergeable independently. Stage 3's existing tests pass after this lands.

### Plan A2 — Resources and reconcilers

Seven new CRDs. Rust types + StatusPatch enums + per-resource reconcilers + actuators (Docker, CheckoutManager, TerminalPool). New `flotilla-controllers` crate. Each reconciler tested in isolation (pure reconcile + StatusPatch::apply tests + actuator-with-fake-provider tests).

After this lands: the resources exist, individual reconcilers work against the in-memory backend, but nothing creates TaskWorkspaces yet.

### Plan A3 — Daemon wiring + binary split

Daemon startup (Host self-register, host-direct Environment auto-create, Clone discovery, default PlacementPolicies, controller-loop spawning), heartbeat task, new `flotillad` binary in `flotilla-daemon`, removal of `flotilla` TUI binary's embedded-daemon mode, test-support helper for InProcessDaemon-everything setups, in-memory backend end-to-end test, HTTP backend integration test against minikube.

After this lands: end-to-end flow lights up.

### Sequencing

Strict dependency chain: A1 → A2 → A3. Each is reviewable as a separate PR; nothing in A2 can land before A1, nothing in A3 before A2.
