# EnvironmentProvider Trait and Docker Implementation

**Issue:** #473 (Phase C of #442)
**Date:** 2026-03-25
**Depends on:** #471 (hop chain, complete), #472 (provider audit, complete)

## Summary

Introduce the `EnvironmentProvider` trait, `ProvisionedEnvironment` handle trait, and a Docker implementation. This phase delivers the provider infrastructure, runner decorator, hop chain integration, sandbox-scoped sockets, and environment data in the snapshot/delta system. It does not include step system integration, provisioning target UI, or workspace orchestration (Phase D).

## Core Types

All types use `DaemonHostPath` and `ExecutionEnvironmentPath` newtypes where appropriate.

**Prerequisite:** move `DaemonHostPath` and `ExecutionEnvironmentPath` from `flotilla-core::path_context` to `flotilla-protocol`. They are pure `PathBuf` wrappers with transparent serde and carry no core logic. This is a small mechanical refactor (move definitions, update imports) that should be a separate prep PR. Until the migration, types like `CreateOpts` that use these newtypes live in `flotilla-core` — the protocol-layer types (`HostEnvironmentInfo`, `EnvironmentBinding`) use plain `PathBuf` with conversion in `convert.rs`.

```rust
struct EnvironmentId(String);  // filesystem-safe: UUID or slug
                                // appears in socket paths, container names, replicated data

struct EnvironmentSpec {
    image: ImageSource,
    token_requirements: Vec<String>,  // e.g. ["github", "claude"]
}

enum ImageSource {
    Dockerfile(PathBuf),
    Registry(String),
}

struct ImageId(String);  // docker image ID or tag

enum EnvironmentStatus {
    Building,
    Starting,
    Running,
    Stopped,
    Failed(String),
}

struct CreateOpts {
    tokens: Vec<(String, String)>,                          // env var key-value pairs
    reference_repo: Option<DaemonHostPath>,                 // host .git to mount read-only
    daemon_socket_path: DaemonHostPath,                     // host-side socket to mount
    working_directory: Option<ExecutionEnvironmentPath>,     // interior working dir
}
```

## EnvironmentProvider Trait

New provider category in `FactoryRegistry`:

```rust
pub type EnvironmentProviderFactory = dyn Factory<Output = dyn EnvironmentProvider>;

// In FactoryRegistry:
pub environment_providers: Vec<Box<EnvironmentProviderFactory>>,
```

The factory `probe()` checks `docker --version` via the injected runner. If Docker is available, returns a `DockerEnvironment` provider.

```rust
#[async_trait]
trait EnvironmentProvider: Send + Sync {
    async fn ensure_image(&self, spec: &EnvironmentSpec) -> Result<ImageId, String>;
    async fn create(&self, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String>;
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
}
```

## ProvisionedEnvironment Handle

Live reference to a running environment. Callers interact through the trait; the implementation owns caching strategy and lifecycle.

```rust
type EnvironmentHandle = Arc<dyn ProvisionedEnvironment>;

#[async_trait]
trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;

    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;

    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner>;

    async fn destroy(&self) -> Result<(), String>;
}
```

**Important distinction:** `env_vars()` returns raw shell environment variables (`HashMap<String, String>`) — what processes inside the container actually see. This is not the same as `EnvironmentBag` (assertion-based discovered facts used by `Factory::probe()`). Raw env vars feed into the discovery pipeline which builds `EnvironmentBag`, same as on a host.

## Docker Implementation

### DockerEnvironment Provider

```rust
struct DockerEnvironment {
    runner: Arc<dyn CommandRunner>,
}
```

All operations shell out to the `docker` CLI via the injected `CommandRunner`. This is consistent with every other provider (git, gh, cleat, shpool) and leverages the existing replay test infrastructure.

**`ensure_image(spec)`:**
- `Dockerfile(path)` → `docker build -t flotilla-env-{hash} -f {path} {context_dir}`
- `Registry(image)` → `docker pull {image}`
- Returns `ImageId`

**`create(image, opts)`:**
```
docker run -d \
  --name flotilla-env-{uuid} \
  --label flotilla.environment={id} \
  -v {opts.daemon_socket_path}:/run/flotilla.sock \
  -v {opts.reference_repo}:/ref/repo:ro \
  -e FLOTILLA_DAEMON_SOCKET=/run/flotilla.sock \
  -e FLOTILLA_ENVIRONMENT_ID={id} \
  -e {token_key}={token_value} \
  {image} \
  sleep infinity
```

The container runs `sleep infinity` — it is a managed resource, not a service. Processes launch inside via `docker exec`. Containers are labelled with `flotilla.environment={id}` so `list()` can filter for flotilla-managed containers.

**`list()`:** `docker ps --filter label=flotilla.environment --format json` → parse, return handles.

### DockerProvisionedEnvironment Handle

```rust
struct DockerProvisionedEnvironment {
    id: EnvironmentId,
    container_name: String,
    image: ImageId,
    runner: Arc<dyn CommandRunner>,  // host runner for docker CLI
}
```

**`status()`:** `docker inspect --format '{{.State.Status}}' {container}` → map to `EnvironmentStatus`.

**`env_vars()`:** `docker exec {container} sh -lc env` → parse key=value lines into `HashMap<String, String>`.

**`runner(host_runner)`:** Returns an `EnvironmentRunner` decorator.

**`destroy()`:** `docker rm -f {container}`.

## EnvironmentRunner Decorator

Wraps a host `CommandRunner` so that all commands execute inside the container via `docker exec`:

```rust
struct EnvironmentRunner {
    container_name: String,
    inner: Arc<dyn CommandRunner>,
}
```

All three `CommandRunner` methods are wrapped:

**`run(cmd, args, cwd, label)`** → `inner.run("docker", &["exec", "-w", cwd_str, &container_name, cmd, ...args], "/", label)`.

**`run_output(cmd, args, cwd, label)`** → same transformation, delegates to `inner.run_output()`.

**`exists(cmd, args)`** → cannot delegate to `inner.exists()` (different signature — no `cwd` or `label`). Instead calls `inner.run("docker", &["exec", &container_name, "which", cmd], "/", label)` and converts the result to `bool`.

The caller's `cwd` (an `ExecutionEnvironmentPath` inside the container) becomes a `-w` flag on `docker exec`. The host-side `cwd` is irrelevant (uses `/`).

This runner feeds into `FactoryRegistry::probe()` for interior provider discovery — the same factories discover cleat, git, etc. inside the container using the decorated runner.

## EnterEnvironment Hop

Extends the hop chain (Phase A) with environment traversal.

### New hop variant

```rust
// Added to Hop enum:
EnterEnvironment { env_id: EnvironmentId, provider: String }
```

### EnvironmentHopResolver

New per-hop resolver trait, following the `resolve_wrap`/`resolve_enter` pattern of `RemoteHopResolver`:

```rust
trait EnvironmentHopResolver: Send + Sync {
    fn resolve_wrap(&self, env_id: &EnvironmentId, ctx: &mut ResolutionContext) -> Result<(), String>;
    fn resolve_enter(&self, env_id: &EnvironmentId, ctx: &mut ResolutionContext) -> Result<(), String>;
}
```

**Docker implementation:**

`resolve_wrap()` — pops the inner command from context, wraps in `docker exec -it {container} ...` using the `Arg` tree with depth-aware quoting. Same nesting pattern as SSH wrapping.

`resolve_enter()` — creates a `docker exec -it {container} /bin/sh` execution boundary, then remaining hops become `SendKeys`.

**Collapse logic:** if `ResolutionContext.current_environment == env_id`, skip the hop.

**Dispatch:** `HopResolver::resolve()` gains an `EnterEnvironment` arm in its `match` on `Hop`. The existing `CombineStrategy::should_wrap()` already receives `&Hop` — it pattern-matches the new variant to decide between `resolve_wrap` and `resolve_enter`. The `ResolutionContext::current_environment` field changes from `Option<String>` (placeholder) to `Option<EnvironmentId>`.

## Sandbox-Scoped Sockets

### Per-environment listeners

The daemon creates an additional `tokio::net::UnixListener` per environment. Each listener is its own tokio task, accepting connections that spawn `handle_client` with the same shared state (`Arc<InProcessDaemon>`, `Arc<PeerManager>`, etc.). No multiplexing needed — this is just more tasks hitting the same shared state.

```rust
// Managed by daemon server:
struct EnvironmentSocketRegistry {
    sockets: HashMap<EnvironmentId, (JoinHandle<()>, DaemonHostPath)>,
}

impl EnvironmentSocketRegistry {
    async fn add(&mut self, id: EnvironmentId, state_dir: &DaemonHostPath) -> Result<DaemonHostPath, String>;
    async fn remove(&mut self, id: &EnvironmentId) -> Result<(), String>;
}
```

`add()` — binds `{state_dir}/env-{id}.sock`, spawns accept loop task, returns socket path (for `CreateOpts`).

`remove()` — aborts the task, removes the socket file.

### Connection tagging

`handle_client` gains `environment_context: Option<EnvironmentId>`. Connections from environment sockets are tagged; connections from the main socket are not.

### Protocol handshake

The `Hello` message gains `environment_id: Option<EnvironmentId>` (with `#[serde(default)]` for wire compatibility). No protocol version bump needed — the project is in a no-backwards-compatibility phase. The server verifies:
- Environment socket connection: claimed `environment_id` must match the socket's environment — reject mismatches.
- Main socket connection: `environment_id` accepted as-is (forward-proofs for HTTP/TCP transport).

## Environment in Snapshot/Delta

### Host level

Environment info is reported in the host summary exchanged between peers. This tells the mesh which environments exist and where. The successful factory probe also indicates the host can provision Docker environments — remote daemons use this when routing provisioning requests. `HostSummary` in `flotilla-protocol` gains a new field: `environments: Vec<HostEnvironmentInfo>`.

```rust
struct HostEnvironmentInfo {
    id: EnvironmentId,
    image: ImageId,
    status: EnvironmentStatus,
}
```

The semantics of "host level" are "owning daemon node," not physical containment. For Docker phase 1 these are the same (container runs on the same machine as the managing daemon). For future cloud providers, the environment is managed by a daemon but runs elsewhere.

### Repo level

Provider data items (checkouts, terminals, agents) gain `environment_id: Option<EnvironmentId>` as a first-class field. `RepoModel` gains an optional environment binding that connects the repo's work to its environment:

```rust
// On RepoModel (core) and in repo snapshot data (protocol):
environment_binding: Option<EnvironmentBinding>,

struct EnvironmentBinding {
    environment_id: EnvironmentId,
    host: HostName,
}
```

The binding is set when a step plan wires an environment to a checkout workflow (Phase D). For Phase C, the types exist and are serializable; the binding is populated in tests but not yet by production step execution.

### Correlation

New correlation key: `CorrelationKey::EnvironmentRef(EnvironmentId)`. Items inside the same environment group together. This merges with existing branch-based correlation — same branch + same environment = same work item.

The `environment_id` field on items is stored independently of correlation, enabling future configurable views (by environment, by agent, by work stream) without restructuring the data model.

### Delta handling

Environment changes flow through the existing delta pipeline. New environments, status changes, and removals are delta events. Host summary exchange (already periodic) carries environment info.

## Testing

All Docker operations go through `CommandRunner`, so the existing replay infrastructure handles them as command-channel fixtures.

### Unit tests (replay fixtures)

- `DockerEnvironmentFactory::probe()` — fixture: `docker --version` success/failure
- `ensure_image()` — fixtures: `docker build` and `docker pull` variants
- `create()` — fixture: `docker run` returning container ID
- `status()` — fixture: `docker inspect` returning state
- `env_vars()` — fixture: `docker exec sh -lc env` returning key=value pairs
- `destroy()` — fixture: `docker rm -f`
- `EnvironmentRunner` — verify command transformation (docker exec wrapping)
- `list()` — fixture: `docker ps --filter` returning JSON

### Hop chain integration

- `EnterEnvironment` hop with wrap strategy
- `EnterEnvironment` hop with sendkeys strategy
- Collapse when already inside target environment
- Composition: `RemoteToHost` → `EnterEnvironment` → `AttachTerminal`

### Socket tests

- Environment socket creation and cleanup
- Connection tagging with environment context
- Handshake verification (match/mismatch)

### Integration (in-process daemon)

- Create environment → run discovery inside → verify provider tree contains expected providers
- Full hop chain resolution with environment hop

### Passthrough validation

`REPLAY=passthrough` runs against real Docker when available. Not required for CI; useful for manual validation on equipped hosts.

## Not in Scope (Phase D)

- `StepAction` variants for environment lifecycle
- `StepHost::Environment` step routing
- `ProvisioningTarget` enum and UI changes
- Workspace orchestration with environment context
- `CodebaseAccessStrategy` / clone-inside execution
- Token config resolution from flotilla config
- `.flotilla/environment.yaml` parsing (the `EnvironmentSpec` type exists but is constructed programmatically in tests; file parsing is Phase D)
