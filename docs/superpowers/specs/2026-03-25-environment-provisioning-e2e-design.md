# Environment Provisioning End-to-End

**Issue:** #474 (Phase D of #442)
**Date:** 2026-03-25
**Depends on:** #471 (hop chain, complete), #472 (provider audit, complete), #473 (EnvironmentProvider + Docker, complete)

## Summary

Wire the environment provisioning infrastructure from Phase C into the step system, plan builder, checkout flow, hop chain, refresh pipeline, and sandbox socket lifecycle. Driven programmatically (tests construct Commands directly) — no CLI or TUI changes.

## Command Extension

`Command` gains `environment: Option<EnvironmentSpec>` next to `host`:

```rust
pub struct Command {
    pub host: Option<HostName>,
    pub environment: Option<EnvironmentSpec>,
    pub context_repo: Option<RepoSelector>,
    pub action: CommandAction,
}
```

`host` + `environment` together are the proto-`ProvisioningTarget`. `host` alone means bare host (today's behavior). `host` + `environment` means provision a container on that host. `#[serde(default)]` for wire compatibility.

## Step System

### New StepAction Variants

```rust
EnsureEnvironmentImage { spec: EnvironmentSpec },
CreateEnvironment { image: ImageId, opts: CreateOpts },
DiscoverEnvironmentProviders { env_id: EnvironmentId },
DestroyEnvironment { env_id: EnvironmentId },
```

### StepHost Extension

```rust
pub enum StepHost {
    Local,
    Remote(HostName),
    Environment(EnvironmentId),
}
```

### Step Resolver

The `ExecutorStepResolver` gains:
- `environment_handles: HashMap<EnvironmentId, EnvironmentHandle>` — populated by `CreateEnvironment`, consumed by subsequent steps.
- `environment_registries: HashMap<EnvironmentId, Arc<ProviderRegistry>>` — populated by `DiscoverEnvironmentProviders`, used when `StepHost::Environment` routes actions through environment providers.
- `environment_sockets: Arc<Mutex<EnvironmentSocketRegistry>>` — passed in from the daemon server.

**Resolution of new actions:**

`EnsureEnvironmentImage { spec }` — looks up `EnvironmentProvider` from the host's registry, calls `ensure_image(spec)`. Returns `Produced(ImageId)`.

`CreateEnvironment { image, opts }` — creates sandbox socket via `EnvironmentSocketRegistry::add()`, passes socket path in `CreateOpts`. Calls `provider.create(image, opts)`. Stores the `EnvironmentHandle`. Returns `Produced(EnvironmentId)`.

`DiscoverEnvironmentProviders { env_id }` — retrieves handle, calls `handle.env_vars()`, runs `FactoryRegistry::probe()` through the handle's runner with the captured env vars. Stores the resulting per-environment `ProviderRegistry`. Returns `Completed`.

`DestroyEnvironment { env_id }` — calls `handle.destroy()`, removes sandbox socket via `EnvironmentSocketRegistry::remove()`. Returns `Completed`.

**Routing for `StepHost::Environment(env_id)`:** The resolver looks up the environment's `ProviderRegistry` and routes the step's action through those providers instead of the host's. Existing step actions (checkout, terminal prep, workspace creation) work unchanged — they just run against different providers.

## Plan Builder

`build_plan()` in `executor.rs` checks `cmd.environment`. When present and the command involves checkout/workspace creation, it prepends environment lifecycle steps:

```
1. EnsureEnvironmentImage { spec }             on Remote(host)
2. CreateEnvironment { image, opts }           on Remote(host)
3. DiscoverEnvironmentProviders { env_id }     on Remote(host)
4. CreateCheckout { branch, ... }              on Environment(env_id)
5. PrepareTerminalForCheckout { ... }          on Environment(env_id)
6. CreateWorkspaceFromPreparedTerminal { ... } on Environment(env_id)
7. ResolveAttachCommand { ... }                → HopPlan with EnterEnvironment
```

Steps 1-3 run on the host (the host daemon orchestrates the environment). Steps 4-6 run inside the environment (routed through the environment's providers). Step 7 produces a hop plan that includes the `EnterEnvironment` hop for correct attach resolution.

`CreateOpts` is populated by the plan builder:
- `daemon_socket_path` — from the sandbox socket registry (step 2 creates it)
- `reference_repo` — resolved from the host repo's git common dir (`git rev-parse --git-common-dir`)
- `tokens` — passed through from `Command` context (Phase D: programmatic, Phase E: from config)

## CloneCheckoutManager

New `CheckoutManager` implementation for environments. Discovered inside the container by its factory when the `EnvironmentBag` indicates a container context (presence of `FLOTILLA_ENVIRONMENT_ID` env var and `/ref/repo` reference mount).

```rust
struct CloneCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    reference_repo: ExecutionEnvironmentPath,  // /ref/repo
}
```

`create_checkout(branch)` → `git clone --reference /ref/repo <remote_url> /workspace/<branch>`. The remote URL is read from the reference repo: `git --git-dir /ref/repo remote get-url origin`. For fresh branches, clones default branch then `git checkout -b <branch>`.

Uses the same `CheckoutManager` trait as the worktree implementation. The plan builder and step resolver don't know about the difference — they call `create_checkout()` and the discovered provider handles the rest.

### Factory

`CloneCheckoutManagerFactory` probes for:
- `FLOTILLA_ENVIRONMENT_ID` in `EnvironmentBag` (we're inside a container)
- `/ref/repo` exists and is a valid git directory (reference mount is available)

If both conditions are met, it returns a `CloneCheckoutManager`. Priority should be higher than the worktree factory inside environments (worktree creation doesn't make sense inside a disposable container).

## Hop Chain Wiring

### HopPlanBuilder

`build_for_attachable()` and `build_for_prepared_command()` gain environment awareness. When the target attachable or command is inside an environment (determined from `AttachableStore` metadata — attachables carry `environment_id`), the builder inserts `Hop::EnterEnvironment` between `RemoteToHost` and the terminal/command hop:

```
RemoteToHost(feta) → EnterEnvironment(env_id, "docker") → AttachTerminal(sess)
```

### Workspace Orchestrator

`resolve_prepared_commands_via_hop_chain()` in `executor/workspace.rs` currently uses `NoopEnvironmentHopResolver`. When creating a workspace inside an environment, it constructs `DockerEnvironmentHopResolver` with the container name mapping (from the `EnvironmentHandle` in resolver state) and passes it to the `HopResolver`.

## Refresh and Host Summary

`refresh_providers()` in `refresh.rs` gains a call to `EnvironmentProvider::list()` alongside existing provider refreshes. Results populate `ProviderData` with environment info.

`build_local_host_summary()` reads environment provider results and populates `HostSummary.environments` with `EnvironmentInfo` entries. Remote daemons see environment availability via the host summary exchange.

## Sandbox Socket Lifecycle

`CreateEnvironment` step resolver:
1. Calls `EnvironmentSocketRegistry::add(env_id, state_dir, spawn_fn)` → gets socket path
2. Passes socket path in `CreateOpts::daemon_socket_path`
3. Calls `EnvironmentProvider::create(image, opts)` — container starts with socket mounted

`DestroyEnvironment` step resolver:
1. Calls `handle.destroy()` — container removed
2. Calls `EnvironmentSocketRegistry::remove(env_id)` — socket cleaned up

The `spawn_fn` closure creates an accept loop calling `handle_client` with `environment_context: Some(env_id)`.

`ExecutorStepResolver` gains `environment_sockets: Arc<Mutex<EnvironmentSocketRegistry>>`, passed from the daemon server.

## Testing

### Unit tests

- Plan builder produces correct step sequence when `command.environment` is `Some`
- Step resolver handles each new `StepAction` variant with mock providers
- `CloneCheckoutManager` calls correct git commands through mock runner
- `CloneCheckoutManagerFactory` probes correctly for container indicators

### In-process daemon test

Construct `Command { host: Some(feta), environment: Some(spec), action: Checkout { ... } }`, execute through `InProcessDaemon`. Verify full step sequence: ensure image → create environment → discover → checkout → terminals → workspace. Verify attach command resolves with `EnterEnvironment` hop. All mock-backed via replay fixtures.

### Real Docker (optional, not CI)

Same flow with `REPLAY=passthrough` against real Docker using the `flotilla-dev-env` image. Validates the entire chain against a real container.

## Not in Scope (Phase E)

- CLI noun/verb changes (environment noun, provisioning target routing)
- TUI provisioning target UI (replacing target host)
- `.flotilla/environment.yaml` parsing
- Token config resolution (tokens passed programmatically)
- `ProvisioningTarget` enum (proto-form is `host` + `environment` on `Command`)
