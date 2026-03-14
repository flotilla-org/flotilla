# InProcessDaemon Discovery Runtime Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make all `InProcessDaemon` discovery use an explicit `DiscoveryRuntime` so daemon construction and `add_repo()` can run with deterministic fake or replay-backed dependencies.

**Architecture:** Introduce a `DiscoveryRuntime` bundle in the discovery module, make `InProcessDaemon` own that runtime plus its computed `host_bag`, and route all repo discovery through the stored runtime. Production call sites will construct `DiscoveryRuntime::for_process(follower)`, while tests will provide deterministic runtimes directly.

**Tech Stack:** Rust, Tokio, async-trait, existing discovery pipeline, existing daemon integration tests

---

## File Map

- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
  Defines `DiscoveryRuntime`, the process-backed runtime constructor, and any trait/object ownership needed for daemon reuse.
- Modify: `crates/flotilla-core/src/in_process.rs`
  Stores the runtime, switches constructor shape, runs startup discovery through the runtime, and removes hardwired `ProcessEnvVars`/`ProcessCommandRunner` usage.
- Modify: `src/main.rs`
  Embedded-mode daemon construction must build and pass `DiscoveryRuntime::for_process(daemon_config.follower)`.
- Modify: `crates/flotilla-daemon/src/server.rs`
  Socket daemon construction must build and pass `DiscoveryRuntime::for_process(daemon_config.follower)`.
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
  Test call sites must construct explicit runtimes instead of the removed convenience constructors.
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
  Update the integration tests to construct deterministic runtimes and stop depending on ambient host-installed CLIs.
- Optional create: `crates/flotilla-core/tests/support/mod.rs`
  Shared fake `CommandRunner` / `EnvVars` helpers if `in_process_daemon.rs` becomes cleaner with reusable deterministic test support.

## Chunk 1: Add DiscoveryRuntime and Rewire Construction

### Task 1: Add a failing compile target for the new constructor shape

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write the failing test setup**

Add a helper near the top of `crates/flotilla-core/tests/in_process_daemon.rs` that tries to construct:

```rust
let discovery = DiscoveryRuntime::for_process(false);
let daemon = InProcessDaemon::new(vec![repo.clone()], config, discovery, HostName::local()).await;
```

Use it from one existing test helper such as `daemon_for_cwd()`.

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon daemon_broadcasts_snapshots -- --nocapture
```

Expected: FAIL to compile because `DiscoveryRuntime` and/or the new constructor signature do not exist yet.

- [ ] **Step 3: Commit the failing-test checkpoint if desired**

This step is optional for local flow; do not commit broken mainline code. Move directly to implementation.

### Task 2: Implement `DiscoveryRuntime`

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Add the runtime type**

Define:

```rust
pub struct DiscoveryRuntime {
    pub runner: Arc<dyn CommandRunner>,
    pub env: Arc<dyn EnvVars>,
    pub host_detectors: Vec<Box<dyn HostDetector>>,
    pub repo_detectors: Vec<Box<dyn RepoDetector>>,
    pub factories: FactoryRegistry,
}
```

- [ ] **Step 2: Add the process-backed constructor**

Implement:

```rust
impl DiscoveryRuntime {
    pub fn for_process(follower: bool) -> Self {
        let factories = if follower {
            FactoryRegistry::for_follower()
        } else {
            FactoryRegistry::default_all()
        };
        Self {
            runner: Arc::new(crate::providers::ProcessCommandRunner),
            env: Arc::new(ProcessEnvVars),
            host_detectors: detectors::default_host_detectors(),
            repo_detectors: detectors::default_repo_detectors(),
            factories,
        }
    }
}
```

- [ ] **Step 3: Keep ownership and visibility clean**

Ensure `DiscoveryRuntime` is public enough for `src/main.rs`, daemon server code, and integration tests to construct and pass around.

- [ ] **Step 4: Run focused core tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked providers::discovery -- --nocapture
```

Expected: PASS

### Task 3: Make `InProcessDaemon` use the runtime at startup

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Change the constructor signature**

Replace the current convenience constructor pattern with the explicit constructor:

```rust
pub async fn new(
    repo_paths: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    discovery: DiscoveryRuntime,
    host_name: HostName,
) -> Arc<Self>
```

- [ ] **Step 2: Store the runtime on the daemon**

Add a `discovery: DiscoveryRuntime` field and remove separate storage for repo detectors/factories that now live inside the runtime.

- [ ] **Step 3: Route startup host detection through the runtime**

Replace the constructor-local `ProcessCommandRunner` / `ProcessEnvVars` setup with:

```rust
let host_bag = discovery::run_host_detectors(
    &discovery.host_detectors,
    &*discovery.runner,
    &*discovery.env,
).await;
```

Use the same runtime when calling `discover_providers()` for initial repos.

- [ ] **Step 4: Run the original failing test**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon daemon_broadcasts_snapshots -- --nocapture
```

Expected: compiles again, but may still fail at runtime until call sites and deterministic test runtime are updated.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/mod.rs crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor(core): add explicit discovery runtime for daemon"
```

## Chunk 2: Migrate All Call Sites and Remove Hardwired Rediscovery

### Task 4: Update production call sites

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Write the failing compile check**

Build before updating call sites:

```bash
cargo build --locked
```

Expected: FAIL with constructor mismatch errors at `src/main.rs` and `crates/flotilla-daemon/src/server.rs`.

- [ ] **Step 2: Update embedded mode**

In `src/main.rs`, construct:

```rust
let discovery = flotilla_core::providers::discovery::DiscoveryRuntime::for_process(daemon_config.follower);
let d = InProcessDaemon::new(repo_roots, Arc::clone(&config_clone), discovery, host_name).await;
```

- [ ] **Step 3: Update daemon server**

In `crates/flotilla-daemon/src/server.rs`, do the same for the server-managed daemon.

- [ ] **Step 4: Verify the production code builds**

Run:

```bash
cargo build --locked
```

Expected: PASS

### Task 5: Route `add_repo()` through the stored runtime

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Write a failing targeted test**

Add or update a test in `crates/flotilla-core/tests/in_process_daemon.rs` that:

1. constructs a daemon with no initial repos using a deterministic runtime,
2. calls `add_repo()` on a temp repo,
3. asserts the repo is added successfully.

Before the implementation change, it should still try to use hardwired `ProcessEnvVars` in `add_repo()`.

- [ ] **Step 2: Run the targeted test to verify failure**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon add_and_remove_repo_updates_state_and_emits_events -- --nocapture
```

Expected: FAIL or hang because `add_repo()` still uses the wrong discovery dependencies.

- [ ] **Step 3: Implement the fix**

Replace the `discover_providers()` call in `add_repo()` so it uses:

```rust
&self.discovery.repo_detectors
&self.discovery.factories
Arc::clone(&self.discovery.runner)
&*self.discovery.env
```

No `ProcessEnvVars` or constructor-local process runner should remain in daemon discovery paths.

- [ ] **Step 4: Re-run the targeted test**

Run the same command again.

Expected: PASS

- [ ] **Step 5: Grep for leftover hardwiring**

Run:

```bash
rg -n "ProcessEnvVars|ProcessCommandRunner|new_with_options" crates/flotilla-core/src/in_process.rs src/main.rs crates/flotilla-daemon/src/server.rs crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-daemon/tests/multi_host.rs
```

Expected:
- no `ProcessEnvVars` or `ProcessCommandRunner` references in daemon discovery paths
- no remaining `new_with_options` call sites

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs src/main.rs crates/flotilla-daemon/src/server.rs
git commit -m "refactor: route all daemon discovery through runtime"
```

### Task 6: Update test and integration call sites

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`

- [ ] **Step 1: Replace old constructor calls**

Convert all `InProcessDaemon::new(...)` / `new_with_options(...)` call sites to explicit runtime construction.

- [ ] **Step 2: Use the right runtime shape**

For multi-host follower tests, build:

```rust
let discovery = DiscoveryRuntime::for_process(true);
```

For non-follower tests, use `DiscoveryRuntime::for_process(false)` unless the test is being converted to a deterministic fake runtime in the next chunk.

- [ ] **Step 3: Run the affected tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-daemon --locked --test multi_host -- --nocapture
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-daemon/tests/multi_host.rs
git commit -m "test: migrate daemon constructor call sites to discovery runtime"
```

## Chunk 3: Make InProcessDaemon Tests Deterministic

### Task 7: Add deterministic test discovery helpers

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Optional create: `crates/flotilla-core/tests/support/mod.rs`

- [ ] **Step 1: Write the failing deterministic test**

Add a focused test that constructs a daemon with:

- a fake or replay-backed runner,
- a fixed env implementation,
- explicit detector/factory lists through `DiscoveryRuntime`,

and asserts daemon startup completes and emits a snapshot without depending on ambient host tools.

If using a fake runner, the test helper should implement the public `CommandRunner` trait locally in the integration test.

- [ ] **Step 2: Run it to verify failure**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon daemon_broadcasts_snapshots -- --nocapture
```

Expected: FAIL because the existing test still depends on ambient machine discovery or provider command behavior.

- [ ] **Step 3: Build the minimal deterministic runner/env**

Implement a local fake runner that supports only the commands the daemon test path needs. Keep it minimal:

- answer `exists("git", ..)` as needed,
- provide canned `git` command output used by the selected test repos,
- return deterministic stderr/stdout for unsupported commands.

Implement a trivial env type:

```rust
struct StaticEnvVars(HashMap<String, String>);
impl EnvVars for StaticEnvVars { ... }
```

- [ ] **Step 4: Convert the test helper**

Replace the ambient `daemon_for_cwd()` setup with deterministic runtime construction, ideally using a temp repo rather than the real workspace root.

- [ ] **Step 5: Re-run the focused test**

Run the same command again.

Expected: PASS

### Task 8: Convert the remaining daemon integration tests

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Update each helper and constructor call**

Make all tests in the file use the deterministic runtime helper.

- [ ] **Step 2: Keep scope tight**

Do not expand test coverage unrelated to `#315`. Only change what is necessary to remove dependence on ambient host-installed CLIs and startup probes.

- [ ] **Step 3: Run the full target**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon -- --nocapture
```

Expected: PASS

- [ ] **Step 4: Run broader crate verification**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked -- --nocapture
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/tests/in_process_daemon.rs crates/flotilla-core/tests/support
git commit -m "test(core): make in-process daemon discovery deterministic"
```

## Final Verification

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --check
```

Expected: PASS  
If it fails on style differences specific to repo conventions, run:

```bash
cargo +nightly fmt --check
```

- [ ] **Step 2: Run the targeted daemon and multi-host tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-core --locked --test in_process_daemon
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-daemon --locked --test multi_host
```

Expected: PASS

- [ ] **Step 3: Run workspace verification if time permits**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 4: Commit the final integrated change**

```bash
git add crates/flotilla-core/src/providers/discovery/mod.rs crates/flotilla-core/src/in_process.rs src/main.rs crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/tests/multi_host.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "refactor: inject daemon discovery runtime (#315)"
```
