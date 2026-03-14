# InProcessDaemon Discovery Runtime

**Date**: 2026-03-14
**Issue**: #315
**Status**: Approved

## Summary

`InProcessDaemon` currently hardwires process-backed discovery dependencies during construction and again during `add_repo()`. That makes daemon tests depend on whatever tools happen to exist on the host machine and prevents replay/fake infrastructure from covering startup-time detection.

The fix is to make discovery dependencies explicit and persistent for the lifetime of the daemon. `InProcessDaemon` will own a `DiscoveryRuntime` that contains the command runner, environment accessor, detector lists, and factory registry. All provider discovery done by the daemon will use that runtime.

## Goals

- Make all `InProcessDaemon` discovery deterministic under test.
- Use the same dependency model for startup discovery and later repo discovery.
- Avoid test-only seams or alternate codepaths.
- Keep the production wiring straightforward.

## Non-Goals

- Reworking the discovery pipeline itself.
- Changing provider behavior outside how discovery dependencies are supplied.
- Blocking on Docker Compose integration tests before multi-host Phase 2 work.

## Design

### DiscoveryRuntime

Add a new runtime bundle in the core discovery area:

```rust
pub struct DiscoveryRuntime {
    pub runner: Arc<dyn CommandRunner>,
    pub env: Arc<dyn EnvVars>,
    pub host_detectors: Vec<Box<dyn HostDetector>>,
    pub repo_detectors: Vec<Box<dyn RepoDetector>>,
    pub factories: FactoryRegistry,
}
```

Also add a production helper:

```rust
impl DiscoveryRuntime {
    pub fn for_process(follower: bool) -> Self { ... }
}
```

`for_process()` builds:

- `ProcessCommandRunner`
- `ProcessEnvVars`
- default host detectors
- default repo detectors
- follower-aware factory registry

### InProcessDaemon Constructor

Make the real daemon constructor explicit about its dependencies:

```rust
pub async fn new(
    repo_paths: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    discovery: DiscoveryRuntime,
    host_name: HostName,
) -> Arc<Self>
```

`follower` no longer needs to be a separate daemon constructor parameter because it is already encoded in `DiscoveryRuntime::factories`.

Call sites that want the normal process-backed behavior construct the runtime first:

```rust
let discovery = DiscoveryRuntime::for_process(follower);
let daemon = InProcessDaemon::new(repo_paths, config, discovery, host_name).await;
```

This is an internal application surface, so honesty is preferred over convenience wrappers.

### Stored Daemon State

`InProcessDaemon` stores:

- `discovery: DiscoveryRuntime`
- `host_bag: EnvironmentBag`

It no longer stores separate repo detector / factory fields outside the runtime. The runner is no longer an implicit startup-local dependency; it is part of the runtime and reused everywhere discovery runs.

### Startup Discovery

During daemon construction:

1. Run host detection once using `discovery.host_detectors`, `discovery.runner`, and `discovery.env`.
2. Store the resulting `host_bag`.
3. For each initial repo, call `discover_providers()` using the same `discovery` object.

This replaces the current hardwired use of `ProcessCommandRunner` and `ProcessEnvVars`.

### Later Repo Discovery

`add_repo()` and any future rediscovery path use the same stored runtime:

- `self.discovery.repo_detectors`
- `self.discovery.factories`
- `self.discovery.runner`
- `self.discovery.env`
- `self.host_bag`

This removes the current inconsistency where startup discovery and `add_repo()` do not share a single dependency model.

## Testing

### Primary Test Strategy

Fix `#315` by making daemon tests provide deterministic discovery dependencies.

Tests will be able to construct:

- a fake discovery runtime using `DiscoveryMockRunner` and `TestEnvVars`, or
- a replay-backed runtime where command probes go through `replay::test_runner()`

### Test Coverage

1. Add focused tests proving daemon construction and `add_repo()` use the injected runtime rather than ambient machine tools.
2. Update `crates/flotilla-core/tests/in_process_daemon.rs` to stop depending on host-installed CLIs.
3. Keep multi-host behavior testing for `#267/#268` primarily in the existing peer and daemon integration-style tests rather than overloading the daemon unit test file.

### Why This Is Enough

The repository already has meaningful peer-flow tests without Docker Compose:

- `crates/flotilla-daemon/tests/multi_host.rs`
- `crates/flotilla-daemon/src/peer/channel_tests.rs`

So Docker Compose is not a prerequisite for this preparatory work. The immediate blocker is nondeterministic daemon discovery, not lack of full containerized end-to-end coverage.

## Implementation Notes

- `DiscoveryRuntime` should be the only place that knows how to assemble process-backed discovery dependencies.
- The daemon should not special-case tests.
- If trait object ownership makes cloning awkward, store `Arc<dyn ...>` for the runner and env, and move detector/factory lists directly into the daemon-owned runtime.
- Existing production call sites in the daemon server, embedded mode, and tests will need to construct a runtime explicitly.

## Risks

- Constructor churn across call sites.
- A partial refactor that leaves one path still hardwired to `ProcessEnvVars` or process runner.

The mitigation is straightforward: make all daemon discovery call sites flow through `self.discovery` and add tests for both construction-time and `add_repo()` discovery.

## Recommended Follow-Up

After this lands, proceed with the Phase 2 batch:

1. `#267` followers write back to leader
2. `#268` merge conflict resolution

This gives a deterministic foundation for the daemon-level tests those changes will need.
