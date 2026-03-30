# Environment Manager Phase 2b-3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the remaining semantic debt from Phase 2, generalize `EnvironmentManager` so it can own arbitrary direct environments instead of only the daemon's ambient one, and then add static SSH-backed direct environments using that same model.

**Architecture:** Treat the current `EnvironmentManager` work as a structural refactor that now needs to become a real direct-environment runtime. First close the known gaps: restore a green executor test target, remove the hard-coded local direct `EnvironmentId`, and make the manager able to register multiple direct environments with independent runners and discovery bags. Then add configuration-backed SSH direct environments, discovered via injected remote runners, and keep them explicitly separate from peer-daemon mesh transports. Local, SSH, and provisioned execution contexts should all be owned by one manager by the end of this tranche, but provider attribution, `QualifiedPath`, and `NodeId` remain out of scope.

**Tech Stack:** Rust, async-trait, tokio, flotilla-core, flotilla-daemon, flotilla-protocol

**Spec:** `docs/superpowers/specs/2026-03-30-environment-model-sequencing-design.md`

---

## File Structure

Primary files for this tranche:

- Modify: `crates/flotilla-core/src/environment_manager.rs`
  Generalize direct-environment registration and lookup; add SSH direct environments.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Stop assuming a single hard-coded local environment id; initialize local and static SSH direct environments through the manager.
- Modify: `crates/flotilla-core/src/executor.rs`
  Fix current planning regressions and keep executor behavior aligned with manager-backed direct environments.
- Modify: `crates/flotilla-core/src/config.rs`
  Add daemon-side configuration for static SSH environments, or adapt existing config if reusing it is the better fit.
- Modify: `crates/flotilla-core/src/host_identity.rs`
  Add local direct `EnvironmentId` persistence if not already present.
- Create: `crates/flotilla-core/src/providers/ssh_runner.rs` or a similarly focused module
  Provide an injected command runner abstraction for SSH-backed direct environments.
- Modify: `crates/flotilla-core/src/host_summary.rs`
  Ensure summaries can include manager-backed local and SSH direct environments without changing protocol semantics.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Wire daemon startup to construct static SSH direct environments as part of manager initialization, while keeping peer mesh bootstrap separate.
- Modify: `crates/flotilla-core/src/executor/tests.rs`
  Fix the two failing tests and add regression coverage for the intended remote workspace-label behavior.
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  Add tests for local direct id persistence and SSH direct-environment discovery.
- Add or modify: discovery and manager test support files as needed for SSH runner mocking.

Explicitly out of scope for this plan:

- environment-scoped provider attribution in snapshots and merge logic
- `QualifiedPath` migration / real `HostId` rollout
- peer mesh `NodeId` rekeying
- UI work to expose SSH environments as first-class objects

## Task 1: Repair the branch baseline before extending scope

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/tests.rs`

- [ ] **Step 1: Reproduce the current executor test failures**

Run:

```bash
cargo test -p flotilla-core executor -- --nocapture
```

Expected: reproduce the two current failures around remote workspace-label expectations.

- [ ] **Step 2: Decide and document the intended remote workspace-label behavior**

Inspect the failing tests in:

- `crates/flotilla-core/src/executor/tests.rs`

and the relevant implementation in:

- `crates/flotilla-core/src/executor.rs`

Make a deliberate decision about whether:

- remote checkout plans should still suffix workspace labels with `@host`
- `CreateWorkspaceForCheckout` targeting a remote host should prepare remotely and attach locally with the host suffix

Do not just update snapshots or assertions blindly. The code and tests need to agree on one behavior.

- [ ] **Step 3: Fix the implementation or the tests so the behavior is coherent**

Update `build_plan(...)`, `workspace_label_for_host(...)`, or the relevant tests so the remote-workspace behavior is intentional and consistent.

- [ ] **Step 4: Add one regression test that captures the chosen rule**

Add a focused test that makes the selected remote workspace-labeling rule unambiguous for future refactors.

- [ ] **Step 5: Re-run the executor tests**

Run:

```bash
cargo test -p flotilla-core executor -- --nocapture
```

Expected: the executor test target is green.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/executor.rs crates/flotilla-core/src/executor/tests.rs
git commit -m "fix: restore coherent remote workspace planning semantics"
```

## Task 2: Replace the hard-coded local direct environment id with a real owned identity

**Files:**
- Modify: `crates/flotilla-core/src/host_identity.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Modify: tests in `crates/flotilla-core/src/host_identity.rs` and `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add persisted local direct-environment id resolution**

In `crates/flotilla-core/src/host_identity.rs`, add a helper mirroring the existing host-id persistence pattern:

```rust
pub fn resolve_or_create_environment_id(state_dir: &Path) -> Result<flotilla_protocol::EnvironmentId, String>
```

Use the same create-or-read strategy already used for `host-id`, but persist to `environment-id`.

- [ ] **Step 2: Add tests for stable local environment id persistence**

Add tests proving:

- the environment id is generated once and then re-read
- an existing `environment-id` file is respected

- [ ] **Step 3: Wire `InProcessDaemon` startup to resolve the local direct environment id**

In `crates/flotilla-core/src/in_process.rs`, replace:

```rust
let local_environment_id = EnvironmentId::new("local-environment");
```

with a value resolved from machine-scoped local state.

Use the existing config/state directory infrastructure and keep this scoped to the local direct environment only. Do not expand into the full `HostId`/`QualifiedPath` migration here.

- [ ] **Step 4: Make tests use deterministic injected local direct ids**

Where tests currently depend on the `"local-environment"` literal, refactor them to inject a chosen id via manager construction or dedicated test helpers so the runtime no longer relies on that magic constant.

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core host_identity -- --nocapture
cargo test -p flotilla-core environment_manager -- --nocapture
```

Expected: both targets pass with the new local direct id lifecycle.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/host_identity.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/environment_manager.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "feat: persist local direct environment identity"
```

## Task 3: Generalize `EnvironmentManager` to support multiple direct environments

**Files:**
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Modify: `crates/flotilla-core/src/host_summary.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Remove the “single local direct environment” assumption from manager internals**

Refactor `EnvironmentManager` so it still knows which environment id is the daemon’s local direct environment, but no longer assumes that is the only direct environment it can own.

Keep the local id as a distinguished field if needed for current host-summary and repo-discovery callers, but make direct-environment registration general.

- [ ] **Step 2: Add a public direct-environment registration API**

Add an API such as:

```rust
pub fn register_direct_environment(
    &self,
    env_id: EnvironmentId,
    runner: Arc<dyn CommandRunner>,
    env_bag: EnvironmentBag,
) -> Result<(), String>;
```

Requirements:

- reject collisions with existing provisioned environments
- reject accidental replacement of the current local direct environment unless an explicit replace API is intended
- preserve the existing local direct-environment helpers for call sites that truly need “local”

- [ ] **Step 3: Add a targeted replace/update API for direct-environment discovery state if needed**

If SSH direct environments will be re-probed over time, add a narrow update method for their `EnvironmentBag` rather than exposing mutable internals.

- [ ] **Step 4: Generalize summary-facing manager queries**

Update `host_summary_environments()` and any related helper methods so they can enumerate:

- the local direct environment
- non-local direct environments
- provisioned environments

Do not change protocol shapes yet if that would force broader downstream changes. It is acceptable in this tranche to expose only what the existing summary types can represent, as long as the manager model itself is general.

- [ ] **Step 5: Add manager tests for multiple direct environments**

Add tests proving:

- multiple direct environments can coexist
- direct/provisioned collisions are rejected
- direct-environment lookups use the runner and bag associated with the selected environment id

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test -p flotilla-core environment_manager -- --nocapture
```

Expected: the manager test target passes with multiple direct environments.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/environment_manager.rs crates/flotilla-core/src/host_summary.rs
git commit -m "refactor: generalize environment manager for multiple direct environments"
```

## Task 4: Add an SSH-backed `CommandRunner` for direct environments

**Files:**
- Create: `crates/flotilla-core/src/providers/ssh_runner.rs` or equivalent
- Modify: `crates/flotilla-core/src/providers/mod.rs`
- Test: new runner tests plus any discovery-oriented tests

- [ ] **Step 1: Introduce a focused SSH direct-environment runner**

Create a `CommandRunner` implementation that can execute commands through SSH for discovery and direct-environment operations.

Keep it distinct from the peer mesh transport in `flotilla-daemon`. This runner is for execution environments, not daemon-to-daemon messaging.

The runner should accept explicit connection config, for example:

```rust
pub struct SshCommandRunner {
    destination: String,
    multiplex: bool,
}
```

Keep the first version narrow: enough to run host detectors and repo detectors remotely.

- [ ] **Step 2: Make the runner testable with command construction assertions**

Add tests that validate:

- the SSH invocation shape
- working-directory behavior
- stderr/stdout error handling behavior

Prefer command-shape tests and mocked subprocess execution to live SSH integration tests.

- [ ] **Step 3: Export the runner through the provider module tree**

Add the new module in `crates/flotilla-core/src/providers/mod.rs` and keep its API narrow.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test -p flotilla-core ssh_runner -- --nocapture
```

Expected: runner tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/mod.rs crates/flotilla-core/src/providers/ssh_runner.rs
git commit -m "feat: add ssh command runner for direct environments"
```

## Task 5: Add config for static SSH direct environments

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`
- Modify: related config tests in the same file

- [ ] **Step 1: Decide configuration source of truth**

Make an explicit decision between:

- adding static execution environments to `daemon.toml` as the sequencing spec proposes, or
- temporarily reusing/adapting existing remote-host config in `hosts.toml`

Recommendation: use `daemon.toml` for execution environments and leave `hosts.toml` for peer-daemon mesh config, even if that means some duplication for now. The concepts are different and should not be conflated further.

- [ ] **Step 2: Add `daemon.toml` config types for static SSH environments**

Extend `DaemonConfig` with a structure like:

```rust
#[derive(Debug, Default, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub follower: bool,
    pub host_name: Option<String>,
    #[serde(default)]
    pub environments: std::collections::BTreeMap<String, StaticEnvironmentConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StaticEnvironmentConfig {
    pub hostname: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub flotilla_command: Option<String>,
}
```

Keep this scoped to execution environments only.

- [ ] **Step 3: Add parsing and defaulting tests**

Add tests proving:

- empty daemon config still works
- one or more static environments deserialize correctly
- malformed environment config fails clearly

- [ ] **Step 4: Run targeted config tests**

Run:

```bash
cargo test -p flotilla-core config -- --nocapture
```

Expected: config tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/config.rs
git commit -m "feat: add static ssh environment config"
```

## Task 6: Register static SSH direct environments in `InProcessDaemon`

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/environment_manager.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Add startup wiring to build static SSH direct environments**

In daemon startup:

- load `DaemonConfig`
- iterate configured static SSH environments
- construct an SSH-backed runner for each
- run host detectors through that runner
- register each as a direct environment with the manager

Keep failures isolated per configured environment where possible: one broken SSH environment should not prevent the daemon from managing local execution.

- [ ] **Step 2: Decide how SSH direct environments get their `EnvironmentId` in this tranche**

For this plan, use a staged approach:

- if a safe remote persistence mechanism is already available, use it
- otherwise use a deterministic temporary id scheme scoped to the config entry and explicitly document this as Phase 5 debt

Recommendation: if remote persistence is not trivial yet, avoid inventing fake host-derived ids silently. Either persist remotely in a focused helper or make the temporary status explicit and confined.

- [ ] **Step 3: Add tests for registration of configured SSH direct environments**

Add in-process tests with mocked SSH runners showing:

- configured environments are registered with the manager
- their detector output is stored in environment-scoped bags
- a broken SSH environment reports failure without breaking local startup

- [ ] **Step 4: Run focused daemon tests**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
```

Expected: in-process daemon tests pass with static SSH environment registration.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-core/src/environment_manager.rs
git commit -m "feat: register static ssh direct environments in environment manager"
```

## Task 7: Add SSH direct-environment discovery and repo probing workflows

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` only if helper APIs are needed
- Modify: any new manager APIs needed for per-environment repo discovery

- [ ] **Step 1: Add manager-backed repo/provider discovery for non-local direct environments**

Introduce a path that can run repo discovery against a selected direct environment instead of always using the local direct environment bag and runner.

Do not rewrite all command routing yet; the immediate goal is to make the abstraction valid and testable for SSH direct environments.

- [ ] **Step 2: Keep the local repo path behavior stable**

The local tracked-repo flow should continue to work exactly as before. SSH direct-environment discovery should be additive in this tranche, not a rewrite of all repo ownership semantics.

- [ ] **Step 3: Add focused tests for environment-scoped repo probing**

Add tests proving:

- repo detectors can run against a chosen SSH direct environment runner
- provider discovery for that environment uses its environment bag rather than the local one

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test -p flotilla-core in_process_daemon -- --nocapture
```

Expected: environment-scoped discovery tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/providers/discovery/mod.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor: enable repo discovery against direct environments"
```

## Task 8: Verify the tranche and capture follow-on debt

**Files:**
- No intended code changes unless verification exposes a bug

- [ ] **Step 1: Run the sandbox-safe workspace test command**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: the workspace test suite passes in the sandbox-safe configuration.

- [ ] **Step 2: Run the pinned format check**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
```

Expected: no formatting diffs.

- [ ] **Step 3: Run clippy**

Run:

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: no clippy warnings.

- [ ] **Step 4: Record the remaining intentional debt**

Before declaring this tranche done, confirm these remain explicitly deferred:

- provider attribution and summaries fully keyed by `EnvironmentId`
- `QualifiedPath` / real `HostId`
- remote persisted ids for non-local direct environments, if not yet implemented
- peer mesh `NodeId`

- [ ] **Step 5: Commit any final fixes**

```bash
git add -A
git commit -m "feat: add static ssh direct environments under environment manager"
```

## Follow-On After This Plan

Only plan these once this tranche is complete and verified:

- environment-scoped provider attribution and summary visibility
- `QualifiedPath` / stable `HostId` migration
- `NodeId` mesh identity migration
