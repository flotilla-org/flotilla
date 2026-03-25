# EnvironmentProvider Trait and Docker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce the `EnvironmentProvider` trait with a Docker implementation, enabling flotilla to provision and manage container environments for agent workloads.

**Architecture:** New provider category (`EnvironmentProvider`) follows the existing factory/probe pattern. `DockerEnvironment` shells out via `CommandRunner`. An `EnvironmentRunner` decorator wraps commands through `docker exec`. The hop chain gains `EnterEnvironment`. Per-environment sockets let CLI tools inside containers talk to the host daemon.

**Tech Stack:** Rust, async-trait, tokio (Unix listeners), Docker CLI, existing replay test infrastructure.

**Spec:** `docs/superpowers/specs/2026-03-25-environment-provider-docker-design.md`

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `crates/flotilla-protocol/src/path_context.rs` | `DaemonHostPath`, `ExecutionEnvironmentPath` newtypes (moved from core) |
| `crates/flotilla-protocol/src/environment.rs` | `EnvironmentId`, `EnvironmentSpec`, `ImageSource`, `ImageId`, `EnvironmentStatus`, `EnvironmentInfo`, `EnvironmentBinding` (serde types only) |
| `crates/flotilla-core/src/providers/environment/mod.rs` | `EnvironmentProvider` trait, `ProvisionedEnvironment` trait, `EnvironmentHandle` type alias, `CreateOpts` |
| `crates/flotilla-core/src/providers/environment/runner.rs` | `EnvironmentRunner` decorator |
| `crates/flotilla-core/src/providers/environment/docker.rs` | `DockerEnvironment` provider, `DockerProvisionedEnvironment` handle |
| `crates/flotilla-core/src/providers/environment/tests.rs` | Unit tests for runner decorator and Docker implementation |
| `crates/flotilla-core/src/providers/discovery/factories/docker.rs` | `DockerEnvironmentFactory` |
| `crates/flotilla-core/src/hop_chain/environment.rs` | `EnvironmentHopResolver` trait, `DockerEnvironmentHopResolver` |
| `crates/flotilla-daemon/src/server/environment_sockets.rs` | `EnvironmentSocketRegistry` |

### Modified files

| File | Change |
|------|--------|
| `crates/flotilla-core/src/path_context.rs` | Remove type definitions, re-export from protocol |
| `crates/flotilla-protocol/src/lib.rs` | Add `pub mod path_context`, `pub mod environment`, re-exports, `Hello` message `environment_id` field |
| `crates/flotilla-protocol/src/host_summary.rs` | Add `environments: Vec<EnvironmentInfo>` to `HostSummary` |
| `crates/flotilla-protocol/src/provider_data.rs` | Add `CorrelationKey::EnvironmentRef(EnvironmentId)` |
| `crates/flotilla-core/src/providers/mod.rs` | Add `pub mod environment` |
| `crates/flotilla-core/src/providers/discovery/mod.rs` | Add `EnvironmentProviderFactory` type alias, `ProviderCategory::EnvironmentProvider` variant, `FactoryRegistry` field, `discover_providers` probe_all call |
| `crates/flotilla-core/src/providers/registry.rs` | Add `environment_providers: ProviderSet<dyn EnvironmentProvider>` to `ProviderRegistry` |
| `crates/flotilla-core/src/providers/discovery/factories/mod.rs` | Register docker factory |
| `crates/flotilla-core/src/hop_chain/mod.rs` | Add `pub mod environment`, `Hop::EnterEnvironment` variant, update `ResolutionContext::current_environment` from `Option<String>` to `Option<EnvironmentId>` |
| `crates/flotilla-core/src/hop_chain/resolver.rs` | Add `EnterEnvironment` match arm, `environment: Box<dyn EnvironmentHopResolver>` field on `HopResolver` |
| `crates/flotilla-core/src/hop_chain/tests.rs` | Tests for EnterEnvironment hop |
| `crates/flotilla-daemon/src/server.rs` | Add `environment_context` to `handle_client`, wire `EnvironmentSocketRegistry` |

---

## Task 1: Move Path Newtypes to flotilla-protocol

**Files:**
- Create: `crates/flotilla-protocol/src/path_context.rs`
- Modify: `crates/flotilla-core/src/path_context.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1:** Copy `DaemonHostPath` and `ExecutionEnvironmentPath` from `crates/flotilla-core/src/path_context.rs` (lines 8–72) to new file `crates/flotilla-protocol/src/path_context.rs`. Include all impl blocks.

- [ ] **Step 2:** In `crates/flotilla-protocol/src/lib.rs`, add `pub mod path_context;` and re-export both types.

- [ ] **Step 3:** Replace definitions in `crates/flotilla-core/src/path_context.rs` with re-exports: `pub use flotilla_protocol::path_context::{DaemonHostPath, ExecutionEnvironmentPath};`

- [ ] **Step 4:** Run `cargo build --workspace --locked` — fix import paths.

- [ ] **Step 5:** Run `cargo test --workspace --locked` — all pass.

- [ ] **Step 6:** Commit: `git commit -m "refactor: move path newtypes to flotilla-protocol"`

---

## Task 2: Core Environment Types in flotilla-protocol

**Files:**
- Create: `crates/flotilla-protocol/src/environment.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1:** Create `crates/flotilla-protocol/src/environment.rs` with serde-only types: `EnvironmentId` (newtype around `String`, filesystem-safe), `EnvironmentSpec`, `ImageSource` (enum: `Dockerfile(PathBuf)`, `Registry(String)`), `ImageId`, `EnvironmentStatus` (enum: `Building`, `Starting`, `Running`, `Stopped`, `Failed(String)`), `EnvironmentInfo` (id, image, status), `EnvironmentBinding` (environment_id, host). All derive `Serialize, Deserialize`. `EnvironmentId` and `ImageId` get `Display`, `new()`, `as_str()`.

- [ ] **Step 2:** In `crates/flotilla-protocol/src/lib.rs`, add `pub mod environment;` and re-export all types.

- [ ] **Step 3:** Run `cargo build --workspace --locked`.

- [ ] **Step 4:** Commit: `git commit -m "feat: add core environment types to flotilla-protocol"`

---

## Task 3: EnvironmentProvider Traits and Discovery Wiring

**Files:**
- Create: `crates/flotilla-core/src/providers/environment/mod.rs`
- Modify: `crates/flotilla-core/src/providers/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` (lines 381–550)
- Modify: `crates/flotilla-core/src/providers/registry.rs` (lines 117–139)

- [ ] **Step 1:** Create `crates/flotilla-core/src/providers/environment/mod.rs` with:
  - `pub mod runner;` (will be created in Task 4)
  - `CreateOpts` struct (runtime-only, not serializable): `tokens: Vec<(String, String)>`, `reference_repo: Option<DaemonHostPath>`, `daemon_socket_path: DaemonHostPath`, `working_directory: Option<ExecutionEnvironmentPath>`
  - `EnvironmentHandle` type alias: `Arc<dyn ProvisionedEnvironment>`
  - `EnvironmentProvider` trait: `ensure_image`, `create`, `list`
  - `ProvisionedEnvironment` trait: `id`, `image`, `status`, `env_vars`, `runner`, `destroy`

- [ ] **Step 2:** In `crates/flotilla-core/src/providers/mod.rs`, add `pub mod environment;`

- [ ] **Step 3:** In `crates/flotilla-core/src/providers/discovery/mod.rs`:
  - Add `EnvironmentProvider` variant to `ProviderCategory` enum. Update `slug()` → `"environment_provider"`, `display_name()` → `"Environment Provider"`.
  - Add type alias after line 403: `pub type EnvironmentProviderFactory = dyn Factory<Output = dyn crate::providers::environment::EnvironmentProvider>;`
  - Add field to `FactoryRegistry` (line 418): `pub environment_providers: Vec<Box<EnvironmentProviderFactory>>,`
  - Update `FactoryRegistry` constructors (`default_all()`, `for_follower()`) to include `environment_providers: vec![]`.
  - Add `probe_all` call in `discover_providers()` after the `terminal_pools` call (around line 546):
    ```rust
    probe_all(&factories.environment_providers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
        registry.environment_providers.insert(desc.implementation.clone(), desc, provider);
    }).await;
    ```

- [ ] **Step 4:** In `crates/flotilla-core/src/providers/registry.rs` (line 117), add to `ProviderRegistry`:
  ```rust
  pub environment_providers: ProviderSet<dyn crate::providers::environment::EnvironmentProvider>,
  ```
  Update `ProviderRegistry::new()` (line 129) to include `environment_providers: ProviderSet::new()`.

- [ ] **Step 5:** Run `cargo build --workspace --locked`. Fix exhaustive match warnings on `ProviderCategory` and any test updates needed (e.g. `default_all_has_all_categories`).

- [ ] **Step 6:** Run `cargo test --workspace --locked` — all pass.

- [ ] **Step 7:** Commit: `git commit -m "feat: add EnvironmentProvider and ProvisionedEnvironment traits with discovery wiring"`

---

## Task 4: EnvironmentRunner Decorator

**Files:**
- Create: `crates/flotilla-core/src/providers/environment/runner.rs`
- Create: `crates/flotilla-core/src/providers/environment/tests.rs`
- Modify: `crates/flotilla-core/src/providers/environment/mod.rs`

- [ ] **Step 1:** Create test file `crates/flotilla-core/src/providers/environment/tests.rs` with tests:
  - `run_wraps_with_docker_exec` — verifies `run("git", &["status"], Path::new("/workspace"), &label)` becomes `docker exec -w /workspace {container} git status` on the inner runner.
  - `run_output_wraps_with_docker_exec` — same for `run_output`.
  - `exists_uses_run_with_which` — verifies `exists("cleat", &[])` calls inner's `run` with `docker exec {container} which cleat` and returns `true`/`false` based on success.
  Use a `RecordingRunner` that stores `(cmd, args, cwd)` tuples for verification. Use `ChannelLabel::Noop` for labels.

- [ ] **Step 2:** Add `#[cfg(test)] mod tests;` to `crates/flotilla-core/src/providers/environment/mod.rs`.

- [ ] **Step 3:** Run `cargo test -p flotilla-core --locked environment::tests` — verify compilation fails.

- [ ] **Step 4:** Create `crates/flotilla-core/src/providers/environment/runner.rs`:
  - `EnvironmentRunner` struct: `container_name: String`, `inner: Arc<dyn CommandRunner>`.
  - `new(container_name, inner)` constructor.
  - `impl CommandRunner`:
    - `run()` → wraps args as `["exec", "-w", cwd, container, cmd, ...args]`, delegates to `inner.run("docker", ..., Path::new("/"), label)`.
    - `run_output()` → same transformation, delegates to `inner.run_output()`.
    - `exists()` → calls `inner.run("docker", &["exec", container, "which", cmd], Path::new("/"), &ChannelLabel::Noop)` and returns `.is_ok()`. Cannot use `inner.exists()` because it has no `cwd`/`label` parameters.

- [ ] **Step 5:** Run `cargo test -p flotilla-core --locked environment::tests` — all pass.

- [ ] **Step 6:** Commit: `git commit -m "feat: add EnvironmentRunner decorator for docker exec wrapping"`

---

## Task 5: DockerEnvironment Factory, Provider, and Handle

**Files:**
- Create: `crates/flotilla-core/src/providers/environment/docker.rs`
- Create: `crates/flotilla-core/src/providers/discovery/factories/docker.rs`
- Modify: `crates/flotilla-core/src/providers/environment/mod.rs`
- Modify: `crates/flotilla-core/src/providers/environment/tests.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/mod.rs`

- [ ] **Step 1:** Add tests to `tests.rs` for Docker operations: `ensure_image_builds_dockerfile`, `ensure_image_pulls_registry`, `create_returns_handle`, `status_returns_running`, `env_vars_parses_output`, `destroy_calls_docker_rm`. Use the existing `MockRunner` from the codebase (see `crates/flotilla-core/src/providers/mod.rs` — it uses a `VecDeque<Result<String, String>>` to return responses in order).

- [ ] **Step 2:** Run tests — verify they fail (docker module doesn't exist yet).

- [ ] **Step 3:** Create `crates/flotilla-core/src/providers/environment/docker.rs`:
  - `DockerEnvironment` struct: `runner: Arc<dyn CommandRunner>`.
  - `impl EnvironmentProvider`:
    - `ensure_image(spec)`: `Dockerfile` → `docker build -t flotilla-env-{uuid} -f {path} {context}`, `Registry` → `docker pull {image}`.
    - `create(image, opts)`: `docker run -d --name flotilla-env-{uuid} --label flotilla.environment={id} -v {socket}:/run/flotilla.sock ...` with tokens as `-e`, reference repo as `-v :ro`. Returns `DockerProvisionedEnvironment` handle.
    - `list()`: `docker ps --filter label=flotilla.environment --format ...` → parse, return handles.
  - `DockerProvisionedEnvironment` struct: `id`, `container_name`, `image`, `runner`.
  - `impl ProvisionedEnvironment`:
    - `status()`: `docker inspect --format '{{.State.Status}}'` → map to `EnvironmentStatus`.
    - `env_vars()`: `docker exec {container} sh -lc env` → parse `key=value` lines into `HashMap`. Note: fragile for multi-line values; acceptable for Phase 1.
    - `runner(host_runner)`: returns `EnvironmentRunner::new(container_name, host_runner)`.
    - `destroy()`: `docker rm -f {container}`.
  All docker CLI calls use `ChannelLabel::Noop` for now.

- [ ] **Step 4:** Add `pub mod docker;` to `crates/flotilla-core/src/providers/environment/mod.rs`.

- [ ] **Step 5:** Create `crates/flotilla-core/src/providers/discovery/factories/docker.rs`:
  - `DockerEnvironmentFactory` struct.
  - `impl Factory`: descriptor `ProviderCategory::EnvironmentProvider, "docker"`.
  - `probe()`: check `env.find_binary("docker")` from the `EnvironmentBag`. If found, return `DockerEnvironment::new(runner)`. If not, fall back to `runner.run("docker", &["--version"], ...)` directly and return the provider on success. Return `Err(vec![UnmetRequirement::MissingBinary("docker")])` on failure.

- [ ] **Step 6:** In `crates/flotilla-core/src/providers/discovery/factories/mod.rs`, add `pub mod docker;` and wire `DockerEnvironmentFactory` into `FactoryRegistry::default_all()` environment_providers field.

- [ ] **Step 7:** Run `cargo test -p flotilla-core --locked environment` — all pass.

- [ ] **Step 8:** Commit: `git commit -m "feat: add DockerEnvironment provider and factory"`

---

## Task 6: EnterEnvironment Hop and Resolver

**Files:**
- Create: `crates/flotilla-core/src/hop_chain/environment.rs`
- Modify: `crates/flotilla-core/src/hop_chain/mod.rs`
- Modify: `crates/flotilla-core/src/hop_chain/resolver.rs`
- Modify: `crates/flotilla-core/src/hop_chain/tests.rs`

- [ ] **Step 1:** In `crates/flotilla-core/src/hop_chain/mod.rs`:
  - Add `pub mod environment;`
  - Add variant to `Hop` enum: `EnterEnvironment { env_id: EnvironmentId, provider: String }`
  - Change `ResolutionContext::current_environment` from `Option<String>` to `Option<EnvironmentId>`.

- [ ] **Step 2:** Create `crates/flotilla-core/src/hop_chain/environment.rs`:
  - `EnvironmentHopResolver` trait with `resolve_wrap` and `resolve_enter` (same shape as `RemoteHopResolver`).
  - `DockerEnvironmentHopResolver`: maps `EnvironmentId → container_name`.
    - `resolve_wrap()`: pop inner `Command` action, wrap in `Arg::Literal("docker"), Arg::Literal("exec"), Arg::Literal("-it"), Arg::Literal(container), ...inner_args`. Note: must include "docker" as the command binary in the arg list.
    - `resolve_enter()`: push `Command([Arg::Literal("docker"), Arg::Literal("exec"), Arg::Literal("-it"), Arg::Literal(container), Arg::Literal("/bin/sh")])`.
  - `NoopEnvironmentHopResolver`: rejects all environment hops with an error.

- [ ] **Step 3:** In `crates/flotilla-core/src/hop_chain/resolver.rs`:
  - Add `pub environment: Box<dyn EnvironmentHopResolver>` to `HopResolver`.
  - Add match arm for `Hop::EnterEnvironment { env_id, .. }`:
    ```rust
    Hop::EnterEnvironment { env_id, .. } => {
        if ctx.current_environment.as_ref() == Some(env_id) {
            continue; // collapse
        }
        if self.strategy.should_wrap(hop, ctx) {
            self.environment.resolve_wrap(env_id, ctx)?;
        } else {
            self.environment.resolve_enter(env_id, ctx)?;
        }
        ctx.nesting_depth += 1;
        ctx.current_environment = Some(env_id.clone());
    }
    ```
  - Update existing `CombineStrategy` implementations (`AlwaysWrap`, `AlwaysSendKeys`) to handle the new `Hop::EnterEnvironment` pattern in their match (they receive `&Hop`).

- [ ] **Step 4:** Fix existing code that constructs `HopResolver` to supply an `environment` field (use `NoopEnvironmentHopResolver` as default). Fix existing code that constructs `ResolutionContext` with `current_environment: Option<String>` to use `Option<EnvironmentId>`.

- [ ] **Step 5:** Add tests to `crates/flotilla-core/src/hop_chain/tests.rs`:
  - `enter_environment_wrap_produces_docker_exec`: plan `[EnterEnvironment, AttachTerminal]` with `AlwaysWrap` → output contains `docker exec -it {container}` wrapping the terminal attach.
  - `enter_environment_collapses_when_already_inside`: set `context.current_environment = Some(env_id)`, plan includes `EnterEnvironment` for same `env_id` → hop skipped.
  - `remote_then_environment_then_terminal`: plan `[RemoteToHost(feta), EnterEnvironment(abc), AttachTerminal(sess)]` with `AlwaysWrap` → output nests SSH → docker exec → terminal attach.

- [ ] **Step 6:** Run `cargo test -p flotilla-core --locked hop_chain` — all pass.

- [ ] **Step 7:** Commit: `git commit -m "feat: add EnterEnvironment hop and EnvironmentHopResolver"`

---

## Task 7: Protocol Changes (Hello, CorrelationKey, HostSummary)

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/provider_data.rs`
- Modify: `crates/flotilla-protocol/src/host_summary.rs`

- [ ] **Step 1:** In `crates/flotilla-protocol/src/lib.rs`, add `#[serde(default)] environment_id: Option<EnvironmentId>` to the `Hello` variant of the `Message` enum (around line 150). No protocol version bump needed — project is in no-backwards-compatibility phase.

- [ ] **Step 2:** In `crates/flotilla-protocol/src/provider_data.rs` (line 10), add `EnvironmentRef(EnvironmentId)` to `CorrelationKey` enum.

- [ ] **Step 3:** In `crates/flotilla-protocol/src/host_summary.rs`, add `#[serde(default)] pub environments: Vec<EnvironmentInfo>` to `HostSummary`.

- [ ] **Step 4:** Run `cargo build --workspace --locked`. Fix exhaustive matches on `CorrelationKey` — add `EnvironmentRef` arms (even if just placeholder handling for now).

- [ ] **Step 5:** Run `cargo test --workspace --locked` — all pass.

- [ ] **Step 6:** Commit: `git commit -m "feat: add environment_id to protocol (Hello, CorrelationKey, HostSummary)"`

---

## Task 8: Environment Binding on Snapshot Data

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs` or `provider_data.rs`
- Modify: `crates/flotilla-core/src/convert.rs`

- [ ] **Step 1:** Add `#[serde(default)] pub environment_id: Option<EnvironmentId>` to provider data types that represent items inside an environment: `Checkout`, `CloudAgentSession`, and any other relevant types in `flotilla-protocol/src/provider_data.rs`.

- [ ] **Step 2:** Add `#[serde(default)] pub environment_binding: Option<EnvironmentBinding>` to the repo snapshot type in `flotilla-protocol/src/snapshot.rs`.

- [ ] **Step 3:** Update `crates/flotilla-core/src/convert.rs` to handle the new fields in core↔protocol conversion. The fields are `Option` with `serde(default)`, so existing data remains compatible.

- [ ] **Step 4:** Run `cargo build --workspace --locked && cargo test --workspace --locked`.

- [ ] **Step 5:** Commit: `git commit -m "feat: add environment binding and environment_id to snapshot data model"`

---

## Task 9: Sandbox-Scoped Sockets

**Files:**
- Create: `crates/flotilla-daemon/src/server/environment_sockets.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1:** Create `crates/flotilla-daemon/src/server/environment_sockets.rs`:
  - `EnvironmentSocketRegistry` struct with `sockets: HashMap<EnvironmentId, (JoinHandle<()>, DaemonHostPath)>`.
  - `add(id, state_dir, spawn_fn)` → binds `{state_dir}/env-{id}.sock`, removes stale socket, spawns accept loop via provided closure, returns socket path.
  - `remove(id)` → aborts task, removes socket file.
  - `remove_all()` → cleans up all sockets.

- [ ] **Step 2:** In `crates/flotilla-daemon/src/server.rs`:
  - Add `environment_context: Option<EnvironmentId>` parameter to `handle_client()` (line 371). Main socket accept loop passes `None`.
  - In `handle_client`, after receiving the `Hello` message, verify: if `environment_context` is `Some(expected)` and `hello.environment_id` is `Some(claimed)`, assert `expected == claimed` — reject on mismatch.
  - Add `environment_sockets: Arc<Mutex<EnvironmentSocketRegistry>>` to the server struct for use during environment lifecycle.

- [ ] **Step 3:** Run `cargo build --workspace --locked && cargo test --workspace --locked`.

- [ ] **Step 4:** Commit: `git commit -m "feat: add per-environment sandbox-scoped sockets"`

---

## Task 10: Integration Tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/environment/tests.rs`

- [ ] **Step 1:** Write test `environment_runner_supports_factory_probe`:
  - Create a mock runner responding to `docker exec -w / container cleat --version` with a version string.
  - Create `EnvironmentRunner` wrapping it.
  - Run `CleatTerminalPoolFactory::probe()` with the environment runner.
  - Assert success — proves interior discovery works through the decorator.

- [ ] **Step 2:** Write test `hop_chain_resolves_remote_plus_environment_plus_terminal`:
  - Build plan: `[RemoteToHost(feta), EnterEnvironment(env1, "docker"), AttachTerminal(sess)]`.
  - Create `HopResolver` with `SshRemoteHopResolver` (or mock), `DockerEnvironmentHopResolver`, `PoolTerminalHopResolver` (or mock).
  - Resolve with `AlwaysWrap`.
  - Assert output contains SSH wrapping → docker exec → terminal attach in correct nesting order.

- [ ] **Step 3:** Run `cargo test --workspace --locked` — all pass.

- [ ] **Step 4:** Commit: `git commit -m "test: add environment runner and hop chain integration tests"`

---

## Task 11: Clippy, Format, and Final Verification

- [ ] **Step 1:** Run `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 2:** Run `cargo clippy --workspace --all-targets --locked -- -D warnings` — fix any warnings.

- [ ] **Step 3:** Run `cargo test --workspace --locked` — all pass.

- [ ] **Step 4:** Commit any fixes: `git commit -m "chore: clippy and format cleanup"`
