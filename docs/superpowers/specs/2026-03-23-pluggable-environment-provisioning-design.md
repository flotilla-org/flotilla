# Pluggable Environment Provisioning

**Issue:** #442 (also addresses #368, defers #443)
**Date:** 2026-03-23

## Problem

Flotilla assumes host = daemon = provider runtime. Every worktree, terminal session, and agent runs directly on the physical host where the daemon lives. To support agent workloads in Docker containers, VMs, or cloud instances, flotilla needs to separate three concepts that today collapse into one:

- **Physical host** — the machine running a kernel
- **Environment** — an isolated runtime where code executes (container, VM, bare host)
- **Daemon node** — a flotilla daemon managing work

Phase 1 targets Docker containers as a managed resource. The host daemon orchestrates the container from outside; no daemon runs inside. The flotilla CLI works inside the container via a mounted daemon socket.

## Target End-to-End

"Launch workspace in Docker container on feta (Linux), from kiwi (Mac)."

1. Feta's daemon discovers Docker is available (factory probe).
2. Read `.flotilla/environment.yaml` → `EnvironmentSpec`. Ensure image is built on feta.
3. Launch container: mount daemon socket, mount host repo read-only, inject tokens.
4. Run provider discovery inside container via environment runner — build interior provider tree.
5. Clone repo inside container using discovered Vcs provider (`git clone --reference /ref/repo`).
6. Allocate attachable set and terminal sessions inside container.
7. Resolve attach command as a hop chain: `[SshToHost(feta), EnterEnvironment(abc), AttachTerminal(sess)]`.
8. Inject `CLAUDE_CODE_OAUTH_TOKEN` and scoped `GH_TOKEN` as env vars.

## Design

### Three Independent Axes

Environment configuration, workspace layout, and provisioning target are orthogonal concerns that compose at execution time:

**Environment** (`.flotilla/environment.yaml`) — project-level declaration of what the development environment must provide. Phase 1: a Dockerfile or image reference. Future: affordance profiles (ML, iOS dev), agent-composed from project context.

**Workspace** (`.flotilla/workspace.yaml`) — personal/task-level declaration of what to run. Pane layout, roles, commands. Varies by person, task type, and workflow stage. Unchanged by this work.

**Provisioning target** — where the workload runs. Replaces the current "target host" concept, which conflates the orchestrating daemon with the execution environment.

### Provisioning Target

```rust
enum ProvisioningTarget {
    DirectHost(HostName),
    Provision {
        source: EnvironmentSource,
        spec: EnvironmentSpec,
    },
}

enum EnvironmentSource {
    Mesh { host: HostName, provider: String },
    Cloud { provider: String },
}
```

`DirectHost` preserves today's behavior. `Provision` adds environment creation. The UI renames "target host" to reflect the broader concept.

### EnvironmentProvider Trait

A new provider category in the `FactoryRegistry` (new field: `pub environment_providers: Vec<Box<EnvironmentProviderFactory>>`). Discovery uses the existing `Factory::probe()` mechanism — the factory checks whether the runtime (Docker, Firecracker, etc.) is available on the host.

```rust
trait EnvironmentProvider {
    async fn ensure_image(&self, spec: &EnvironmentSpec) -> Result<ImageId, String>;
    async fn create(&self, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String>;
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
}
```

`EnvironmentSpec` is the template parsed from `.flotilla/environment.yaml`:

```rust
struct EnvironmentSpec {
    image: ImageSource,                 // Dockerfile path or registry image
    token_requirements: Vec<String>,    // e.g. ["github", "claude"]
}

enum ImageSource {
    Dockerfile(PathBuf),
    Registry(String),
}
```

`EnvironmentHandle` is a live reference to a running environment, not a frozen snapshot:

```rust
type EnvironmentHandle = Arc<dyn ProvisionedEnvironment>;

trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;

    // Live queries — implementation decides caching strategy
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;

    // Execution context — composes with host runner (decorator pattern)
    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner>;

    // Lifecycle
    async fn destroy(&self) -> Result<(), String>;
}
```

The handle is opaque — callers interact through the trait. The Docker implementation holds the container ID and queries status, env vars, etc. on demand. `env_vars()` captures the raw shell environment via login shell invocation (`docker exec <container> sh -lc env`), returning a `HashMap<String, String>` of what processes inside the container actually see — `PATH`, `HOME`, `SHELL`, injected tokens, etc. The implementation may cache and invalidate on restart; that's an internal detail.

**Important distinction:** raw env vars (`HashMap<String, String>`) are not the same as `EnvironmentBag` (the assertion-based discovered facts used by `Factory::probe()`). The raw env vars feed *into* the discovery pipeline which builds the `EnvironmentBag` — same as on a host today. The handle provides the raw vars; the discovery pipeline derives the assertions.

`EnvironmentId` is a filesystem-safe newtype (UUID or slug) since it appears in socket paths (`env-{id}.sock`), container names, and replicated data.

The runner returned by the handle wraps commands via `docker exec` (or equivalent). The same runner feeds back into standard provider discovery — the `FactoryRegistry` probes inside the environment using the environment's runner and the raw env vars (to build the `EnvironmentBag`), producing a per-environment provider tree identical in shape to a host-level one.

**Phase 1 implementation:** `DockerEnvironment`.

### Interior Discovery

Provider discovery inside an environment reuses the existing discovery pipeline. The environment handle provides a `CommandRunner`; the standard factories probe through it. The result is a per-environment provider tree (Vcs, TerminalPool, etc.) — the same data structure as host-level discovery.

This design means the provider tree is identical whether discovery ran from a daemon inside the environment or a daemon poking in from outside. The execution context differs; the result does not.

**Research required:** audit every existing factory for assumptions that bypass the injected `CommandRunner` or `EnvironmentBag` — direct `std::env::var()` reads, hardcoded paths, `Command::new()` without the runner. These are seams that must be cleaned before environment discovery works reliably. `ConfigStore` and `repo_root` also need review: config projection into an environment may be a subset, and `repo_root` inside a container is wherever the clone landed, not a pre-known host path.

### Hop Chain Abstraction

Replaces the current string-based attach command with a structured, late-binding plan. Addresses #368.

```rust
enum Hop {
    SshToHost { host: HostName },
    EnterEnvironment { env_id: EnvironmentId, provider: String },
    AttachTerminal { session_name: String, pool: String },
    RunCommand { command: String },
}

enum HopExecution {
    WrapCommand { argv: Vec<String> },
    SendKeys { text: String },
    Collapse,
}

trait HopResolver {
    fn resolve(&self, plan: &[Hop], context: &HopContext) -> Result<Vec<HopExecution>, String>;
}
```

`HopContext` carries the resolver's current position (which host, whether inside an environment). The resolver walks the plan and decides per-hop whether to wrap the next command as an argument, inject keystrokes into a shell, or collapse (already at that point — skip).

**Late binding:** the plan is declarative. The same plan resolves differently depending on where the user is:
- From kiwi: `ssh feta` → `docker exec -it abc` → `cleat attach sess`
- From feta: collapse SSH → `docker exec -it abc` → `cleat attach sess`
- From inside the container: collapse both → `cleat attach sess`

**Migration:** existing SSH wrapping for remote terminal attach migrates onto this abstraction. The hop chain delivers standalone value for #368 before any Docker code exists.

### Codebase Access

Phase 1: clone inside the environment.

```rust
enum CodebaseAccessStrategy {
    CloneInside {
        reference_mount: Option<PathBuf>,
        shallow: bool,
    },
}
```

The environment creation mounts the host's git object store (`.git` directory) read-only at a reference path inside the container. The checkout step uses the environment's discovered Vcs provider to clone:

```
git clone --reference /ref/repo <remote-url> /workspace/<branch>
```

This avoids git worktree symlink problems entirely. The clone is fast (shared objects via `--reference`), has clean ownership (container user owns it), and independent git state (pushes directly with injected token). Shallow clone (`--depth 1`) is available for large repos.

The Vcs provider handles the clone — no git-specific logic in the environment provider. `CodebaseAccessStrategy` sits at the orchestration level (in `CreateOpts` or the step plan), not in `EnvironmentSpec`. The environment spec describes what the environment *provides*; how codebase access works is a deployment-time decision made by the step plan builder.

Future strategies (mount worktree, bidirectional sync) are additional enum variants.

### Terminal Pool Interaction

The terminal pool stays environment-unaware. Cleat (or shpool) runs inside the container and manages sessions natively. The host daemon manages that pool through the environment's `CommandRunner` — `ensure_session()` becomes `docker exec <container> cleat ensure <session>` under the hood.

The hop chain handles the indirection for attach commands. The pool doesn't know about Docker; it just does terminal management in whatever execution context it was discovered in.

### Step System

New `StepAction` variants:

```rust
EnsureEnvironmentImage { spec: EnvironmentSpec }
CreateEnvironment { image: ImageId, opts: CreateOpts }
DestroyEnvironment { env_id: EnvironmentId }
DiscoverEnvironmentProviders { env_id: EnvironmentId }
```

`StepHost` gains an environment dimension:

```rust
enum StepHost {
    Local,
    Remote(HostName),
    Environment(EnvironmentId),
}
```

`StepHost::Environment` is agnostic about whether a daemon runs inside — the owner daemon resolves execution internally.

A workspace-in-environment plan composes these with existing steps:

1. `EnsureEnvironmentImage` → `Produced(ImageId)`
2. `CreateEnvironment` → `Produced(EnvironmentId)`
3. `DiscoverEnvironmentProviders` → interior provider tree available
4. `CreateCheckout` (via env Vcs) → repo cloned inside
5. `PrepareTerminal` (via env TerminalPool) → sessions ready
6. `CreateWorkspace` → layout applied
7. `ResolveAttachCommand` → `HopPlan` with `EnterEnvironment` hop

### Sandbox-Scoped Sockets

The daemon creates a Unix socket per environment at `$FLOTILLA_RUN_DIR/env-{id}.sock`. This socket is mounted into the container at `/run/flotilla.sock`. The container receives env vars:

- `FLOTILLA_DAEMON_SOCKET=/run/flotilla.sock`
- `FLOTILLA_ENVIRONMENT_ID={id}`

The protocol handshake gains an optional `environment_id` field. Even with socket-per-environment (where the daemon already knows the mapping), the client sends the ID for verification. This forward-proofs for HTTP/TCP transports where socket identity is unavailable.

Commands from an environment socket are tagged with that environment context. The daemon uses this for attach command resolution (includes `EnterEnvironment` hop), provider routing (uses the environment's provider tree), and step plan building.

### Environment as Replicated Data

`EnvironmentId` must be visible across the daemon mesh. Environments become first-class data in the snapshot/delta system:

- `id: EnvironmentId`
- `owner: HostName` (managing daemon)
- `source: EnvironmentSource`
- `status: EnvironmentStatus`
- `spec: EnvironmentSpec`

Environments participate in correlation via a new `CorrelationKey::EnvironmentRef(EnvironmentId)` — an environment contains checkouts, terminal sessions, and agent instances that should group into the same work item.

Items inside an environment carry `environment_id` as a **first-class field**, not just a correlation key. This is important: today's correlation is hardcoded with checkout as the primary grouping axis, which is already straining. Environments make it undeniable that different views (by work stream, by environment, by agent, by branch) need different primary axes. Storing `environment_id` richly on every item means configurable correlation views can be built later without restructuring the data model. For phase 1, `EnvironmentRef` participates in the existing union-find alongside branch-based keys — same branch + same environment = same work item.

### Token Injection

Phase 1: env vars at container launch. `CreateOpts` carries token key-value pairs:

- `CLAUDE_CODE_OAUTH_TOKEN` — for the agent inside
- `GH_TOKEN` — scoped to the repo

Long-lived tokens, manually configured. The environment spec declares what it *needs* ("requires: github, claude"); the provisioning system resolves requirements to actual tokens from config. Secrets stay out of the project-level spec file.

**Known limitation:** env vars injected at container launch are visible to all processes inside the container, including agent sub-processes. This is acceptable for phase 1 where the container runs a single trusted agent, but is not suitable for multi-tenant or untrusted workloads. Full credential management (#443) is deferred: rotation, revocation, API proxying, vault integration, audit trails.

## Implementation Phases

### Phase A: Hop Chain + Migrate SSH Wrapping

Introduce `Hop`, `HopExecution`, `HopResolver`. Migrate existing remote terminal SSH wrapping onto the hop chain abstraction. `attach_command()` returns a `HopPlan` instead of a `String` — this changes the `TerminalPool` trait signature, rippling through all implementations (cleat, shpool, passthrough) and callers. Alternatively, the hop chain may compose *around* the pool's string output rather than replacing the trait method; exact migration strategy to be determined during implementation. Delivers #368 value immediately.

### Phase B: Provider Audit + Execution Context Cleanup

Audit every provider factory for direct host assumptions. Ensure all discovery and runtime operations go through injected `CommandRunner` and `EnvironmentBag`. Review `ConfigStore` projection and `repo_root` handling. This phase is research-heavy — its output is a detailed map of what needs changing and the changes themselves.

**Factories in scope:** git, cleat, shpool, passthrough, cmux, tmux, zellij, claude, codex, cursor, github. **Done means:** no `std::env::var()` calls outside test-gated paths, no `Command::new()` bypassing the injected runner, no hardcoded host paths. Each factory's `probe()` and the resulting provider's runtime methods must work when the injected `CommandRunner` and env vars describe a container interior rather than the host.

### Phase C: EnvironmentProvider Trait + Docker Implementation

`EnvironmentProvider` trait, `EnvironmentHandle`, `DockerEnvironment` implementation. Factory registration, discovery probe. `CodebaseAccessStrategy` with clone-inside. Sandbox-scoped sockets. Environment data in snapshot/delta.

### Phase D: Step System + Provisioning Target + End-to-End

`ProvisioningTarget` enum replacing target host. New step actions for environment lifecycle. Interior provider discovery via environment runner. Wire everything together: launch workspace in Docker container on remote host, attach from anywhere via hop chain.

## Open Questions for Nested Brainstorms

- **ConfigStore projection:** what config subset does an environment need? How does `repo_root` work before checkout exists?
- **Environment lifecycle management:** start/stop (pause without destroying interior state), garbage collection, idle timeout, resource limits
- **Image caching:** per-host image cache, cross-host image distribution
- **DirectHost-as-Environment unification:** should bare-host execution eventually become a degenerate environment for a uniform model?
- **Agent awareness:** should agents inside environments know they're sandboxed? How does this affect their workflow?
- **Configurable correlation views:** the current single hardcoded grouping axis (checkout-centric) won't scale. Environments, agents, work streams, and branches are all valid primary axes. How should users switch between views? How does the correlation engine support multiple simultaneous grouping strategies?
- **Proxmox/LXC path:** how does the model extend to managing VMs/containers on a hypervisor from a flotilla daemon in one LXC?
