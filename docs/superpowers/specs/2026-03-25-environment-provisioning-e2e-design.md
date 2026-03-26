# Environment Provisioning End-to-End

**Issue:** #474 (Phase D of #442)
**Date:** 2026-03-25
**Depends on:** #471 (hop chain, complete), #472 (provider audit, complete), #473 (EnvironmentProvider + Docker, complete), #486 (workspace daemon phases, complete)

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

`host` + `environment` together are the proto-`ProvisioningTarget`. `host` alone means bare host (today's behavior). `host` + `environment` means provision a container on that host. `#[serde(default, skip_serializing_if = "Option::is_none")]` for consistency with existing optional fields on `Command`.

## Data Model Corrections (from Phase C)

Phase C added `environment_id: Option<EnvironmentId>` to both `Checkout` and `CloudAgentSession`, and `environment_binding: Option<EnvironmentBinding>` to `RepoSnapshot`. Phase D corrects these:

- **Remove `environment_id` from `CloudAgentSession`** — cloud agent sessions (Claude, Codex, Cursor) run in their own sandboxes, not flotilla-managed environments. The field is meaningless on them.
- **Remove `environment_binding` from `RepoSnapshot`** — the repo-to-environment relationship is many-to-many (multiple environments per repo, multiple repos per environment). The correct model is `environment_id` on individual items (checkouts, terminals) plus host-level `EnvironmentInfo` in `HostSummary`. The `EnvironmentBinding` type can be removed from `flotilla-protocol`.
- **Keep `environment_id` on `Checkout`** — each checkout knows which environment it lives in.

## Step System

### StepExecutionContext (replaces StepHost)

`StepHost` is renamed to `StepExecutionContext`. The old name conflated transport routing (which daemon?) with provider context (which providers?). The new name separates these: the `HostName` determines transport routing, the `EnvironmentId` determines provider context.

```rust
enum StepExecutionContext {
    Host(HostName),
    Environment(HostName, EnvironmentId),
}
```

`Local` is removed — every step explicitly names its daemon. The plan builder stamps `Host(local_host)` on steps that run here, `Host(feta)` on steps that run there. No ambiguity about "local relative to whom." Plans are self-contained and portable.

**Transport routing:** The step runner extracts the `HostName` from either variant. Steps targeting the same daemon are batched together regardless of whether they're `Host` or `Environment`. The `RemoteStepExecutor` sends the batch to that daemon.

**Resolver dispatch:** On the receiving daemon, the resolver checks the variant. `Host` → use the daemon's host providers. `Environment(_, env_id)` → look up the environment's `ProviderRegistry` and resolve against those providers. The resolver also builds a different `RepoExecutionContext` for environment steps: the `repo_root` is the interior path (e.g., `/workspace/branch`), and checkout data comes from the environment's provider tree, not the host's `providers_data`.

### New StepAction Variants

```rust
EnsureEnvironmentImage { spec: EnvironmentSpec },
CreateEnvironment { env_id: EnvironmentId, image: ImageId },
DiscoverEnvironmentProviders { env_id: EnvironmentId },
DestroyEnvironment { env_id: EnvironmentId },
```

Actions carry no environment context — `StepExecutionContext` handles that. The `EnvironmentId` is pre-allocated by the plan builder so the resolver can set up sockets before calling the provider.

Note: `CreateOpts` is constructed by the resolver (not the plan builder), since the resolver has access to `EnvironmentSocketRegistry` and can resolve the reference repo path. The plan builder doesn't have these.

### Step Resolver

The `ExecutorStepResolver` gains:
- `environment_handles: HashMap<EnvironmentId, EnvironmentHandle>` — populated by `CreateEnvironment`, consumed by subsequent steps.
- `environment_registries: HashMap<EnvironmentId, Arc<ProviderRegistry>>` — populated by `DiscoverEnvironmentProviders`.
- `environment_sockets: Arc<Mutex<EnvironmentSocketRegistry>>` — passed in from the daemon server.

**Resolution of new actions:**

`EnsureEnvironmentImage { spec }` — looks up `EnvironmentProvider` from the host's registry, calls `ensure_image(spec)`. Returns `Produced(ImageId)`.

`CreateEnvironment { env_id, image }` — the resolver:
1. Creates sandbox socket via `EnvironmentSocketRegistry::add(env_id, ...)` → gets socket path.
2. Resolves reference repo path on the host (`git rev-parse --git-common-dir`).
3. Builds `CreateOpts` with socket path, reference repo mount, and tokens.
4. Calls `provider.create(env_id, image, opts)`.
5. If `create()` fails, cleans up the socket via `EnvironmentSocketRegistry::remove(env_id)`.
6. Stores the `EnvironmentHandle`. Returns `Produced(EnvironmentId)`.

`DiscoverEnvironmentProviders { env_id }` — retrieves handle, calls `handle.env_vars()` to get raw `HashMap<String, String>`. Runs host-level and repo-level detectors through the environment runner to build an `EnvironmentBag` (same detection pipeline as host discovery, routed through the runner). Runs `FactoryRegistry::probe()` with the environment's `EnvironmentBag` and runner. Stores the resulting per-environment `ProviderRegistry`. Returns `Completed`.

`DestroyEnvironment { env_id }` — calls `handle.destroy()`, removes sandbox socket via `EnvironmentSocketRegistry::remove()`. Returns `Completed`.

**Environment execution context:** When the resolver receives a step with `StepExecutionContext::Environment(_, env_id)`, it builds a `RepoExecutionContext` using the environment's providers, runner, and interior paths. Specifically:
- `repo_root` → the checkout path inside the container (from the `CreateCheckout` step outcome)
- `providers` → from `environment_registries[env_id]`
- `runner` → from `environment_handles[env_id].runner(host_runner)`
- Checkout validation uses the environment's Vcs provider, not the host's

This means existing step actions (checkout, terminal prep) don't need modification — they operate through the `RepoExecutionContext` interface, which is now polymorphic on host vs environment.

## Plan Builder

`build_plan()` in `executor.rs` checks `cmd.environment`. When present and the command involves checkout/workspace creation, it prepends environment lifecycle steps:

```
1. EnsureEnvironmentImage { spec }             on Host(target_host)
2. CreateEnvironment { env_id, image }         on Host(target_host)
3. DiscoverEnvironmentProviders { env_id }     on Host(target_host)
4. CreateCheckout { branch, ... }              on Environment(target_host, env_id)
5. PrepareWorkspace { ... }                    on Environment(target_host, env_id)
6. AttachWorkspace                             on Host(local_host)
```

Steps 1-3 use `Host(target_host)` — they manage the environment on the daemon that owns Docker. Steps 4-5 use `Environment(target_host, env_id)` — same daemon, but the resolver uses environment providers. Step 5 produces a `PreparedWorkspace` payload (containing label, target host, checkout path, attachable set ID, template YAML, and prepared commands). Step 6 uses `Host(local_host)` — it consumes the `PreparedWorkspace` from prior step outcomes, wraps commands through the hop chain (inserting `EnterEnvironment`), and calls the local workspace manager.

The step runner batches steps 1-5 together (same `HostName`) and sends them to `target_host` via `RemoteStepExecutor`. Step 6 executes locally. The receiving daemon distinguishes `Host` from `Environment` steps at resolution time.

The reference repo is mounted directly as a single `-v` bind mount at container creation time. This is a Docker-specific optimisation for fast `git clone --reference`. For VMs or cloud instances (future), the checkout strategy would clone from the remote without a reference. Multi-repo environments would require multiple mounts at creation time or a different strategy; deferred as an open question.

## CloneCheckoutManager

New `CheckoutManager` implementation for environments. Discovered inside the container by its factory when the `EnvironmentBag` indicates a container context (presence of `FLOTILLA_ENVIRONMENT_ID` env var and `/ref/repo` reference mount).

```rust
struct CloneCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    reference_dir: ExecutionEnvironmentPath,  // /ref/repo
}
```

`create_checkout(branch)` → `git clone --reference /ref/repo <remote_url> /workspace/<branch>`. The remote URL is read from the reference: `git --git-dir /ref/repo remote get-url origin`. For fresh branches, clones with `--no-checkout` then `git checkout -b <branch>` from the default branch.

Uses the same `CheckoutManager` trait as the worktree implementation. The plan builder and step resolver don't know about the difference — they call `create_checkout()` and the discovered provider handles the rest.

**Failure/rollback:** If a mid-plan step fails after the environment is created (e.g., checkout fails), the container is left running. Phase D does not add automatic rollback — `run_step_plan` stops on first error. Cleanup is manual (`docker rm -f`) or via a future `DestroyEnvironment` command. Automatic compensating actions are deferred.

### Factory

`CloneCheckoutManagerFactory` probes for:
- `FLOTILLA_ENVIRONMENT_ID` in `EnvironmentBag` (we're inside a container)
- `/ref/repo` exists and is a valid git directory (reference mount is available)

If both conditions are met, it returns a `CloneCheckoutManager` pointed at `/ref/repo`. Priority should be higher than the worktree factory inside environments (worktree creation doesn't make sense inside a disposable container).

## Attachable Environment Awareness

`AttachableSet` gains `environment_id: Option<EnvironmentId>`. When terminals are allocated inside an environment, the set is tagged with the environment ID. This field is the data path that the hop chain builder reads to know when to insert `EnterEnvironment` hops.

`PreparedWorkspace` gains `environment_id: Option<EnvironmentId>`. The `PrepareWorkspace` resolver sets this when running inside an environment. The `AttachWorkspace` resolver reads it from the prior step's `PreparedWorkspace` outcome and passes it to `resolve_prepared_commands_via_hop_chain()` for hop chain construction.

## Hop Chain Wiring

### HopPlanBuilder

`build_for_attachable()` and `build_for_prepared_command()` gain environment awareness. When the target attachable set carries `environment_id` (read from `AttachableStore`), or when the caller passes an explicit `environment_id`, the builder inserts `Hop::EnterEnvironment` between `RemoteToHost` and the terminal/command hop:

```
RemoteToHost(feta) → EnterEnvironment(env_id, "docker") → AttachTerminal(sess)
```

### AttachWorkspace Resolver (Workspace Orchestrator)

`resolve_prepared_commands_via_hop_chain()` in `executor/workspace.rs` currently uses `NoopEnvironmentHopResolver`. When creating a workspace inside an environment, it constructs `DockerEnvironmentHopResolver` with the container name mapping and passes it to the `HopResolver`. The mapping comes from the `EnvironmentHandle` in resolver state — `DockerProvisionedEnvironment` knows its container name internally, exposed via a method that the resolver calls to build the `EnvironmentId → container_name` map.

**Data path for environment_id to the attach step:** The `AttachWorkspace` step (step 6) runs on the presentation host. It needs the `environment_id` to build hop plans with `EnterEnvironment`. This data flows through the `PreparedWorkspace` payload:
- `PrepareWorkspace` (step 5, on the remote daemon) produces `CommandValue::PreparedWorkspace(PreparedWorkspace { environment_id: Some(env_id), ... })`.
- `AttachWorkspace` (step 6, on the presentation host) reads `environment_id` from the `PreparedWorkspace` in prior step outcomes.
- The workspace orchestrator passes it to `resolve_prepared_commands_via_hop_chain()`.

## Refresh and Host Summary

Environment listing is a host-level concern, not per-repo. `refresh_providers()` (which runs per-repo) does **not** call `EnvironmentProvider::list()` — that would duplicate work for every tracked repo.

Instead, `build_local_host_summary()` in `host_summary.rs` queries the `EnvironmentProvider` from the host-level provider registry and calls `list()` to populate `HostSummary.environments` with `EnvironmentInfo` entries. This runs once per host summary build (periodic, not per-repo). Remote daemons see environment availability via the host summary exchange.

## Sandbox Socket Lifecycle

**Ownership:** The step resolver (not the plan builder) manages sockets. The plan builder has no access to `EnvironmentSocketRegistry` — it only builds the step plan. The resolver has the registry, passed from the daemon server.

**`CreateEnvironment` step resolver** (env_id is pre-allocated by plan builder):
1. Calls `EnvironmentSocketRegistry::add(env_id, state_dir, spawn_fn)` → gets socket path
2. Resolves reference repo path, builds `CreateOpts` with socket path, reference mount, tokens
3. Calls `EnvironmentProvider::create(env_id, image, opts)`
4. **On failure:** calls `EnvironmentSocketRegistry::remove(env_id)` to clean up the orphaned socket before propagating the error

**`DestroyEnvironment` step resolver:**
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

Construct `Command { host: Some(feta), environment: Some(spec), action: Checkout { ... } }`, execute through `InProcessDaemon`. Verify full step sequence: ensure image → create environment → discover → checkout → prepare workspace → attach workspace. Verify `PreparedWorkspace` carries `environment_id` and attach step inserts `EnterEnvironment` hop. All mock-backed via replay fixtures.

### Real Docker (optional, not CI)

Same flow with `REPLAY=passthrough` against real Docker using the `flotilla-dev-env` image. Validates the entire chain against a real container.

## Dependencies

- **#464 phase 1 (step-level remote routing)** — merged (#513). `build_plan()` now extracts `Command.host` and stamps steps with `StepHost::Remote(host)`. `run_step_plan_with_remote_executor()` dispatches remote steps via the `RemoteStepExecutor` trait. Phase D uses this infrastructure: environment lifecycle steps target the host that owns Docker, while workspace creation stays local on the presentation host.
- **#486 (workspace daemon phases)** — merged (#515). Replaced the old `TerminalPrepared` → TUI follow-up `CreateWorkspaceFromPreparedTerminal` hack with a unified two-step plan: `PrepareWorkspace` (runs on checkout host, produces `PreparedWorkspace` payload) → `AttachWorkspace` (runs locally, consumes payload). The `WorkspaceManager` trait now takes `WorkspaceAttachRequest` instead of `WorkspaceConfig`. Phase D builds on this: environment workspace creation uses the same `PrepareWorkspace` → `AttachWorkspace` flow, with `PreparedWorkspace` carrying `environment_id` so the attach step can insert `EnterEnvironment` hops.

## Open Questions

- **Multi-repo environments:** A single bind mount per container limits each environment to one reference repo. Multiple repos would need multiple `-v` mounts at creation time (requires advance knowledge) or a different strategy (full clone from remote). Deferred.
- **Environment reuse:** Phase D creates a new environment per checkout command. Sharing an existing environment (e.g., "use the container that's already running for this branch") requires environment lookup and lifecycle management beyond create/destroy.
- **EnvironmentProvider API change:** `create()` currently generates the `EnvironmentId` internally. Phase D needs pre-allocated IDs (for socket setup before container creation). The `DockerEnvironment::create()` signature must change to accept an `EnvironmentId` parameter.

## Not in Scope (Phase E)

- CLI noun/verb changes (environment noun, provisioning target routing)
- TUI provisioning target UI (replacing target host)
- `.flotilla/environment.yaml` parsing
- Token config resolution (tokens passed programmatically)
- `ProvisioningTarget` enum (proto-form is `host` + `environment` on `Command`)
- Multi-repo environment support
