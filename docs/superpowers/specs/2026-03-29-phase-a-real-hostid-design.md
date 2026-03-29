# Phase A: Wire Real HostId Through Discovery

**Status:** Design — ready for implementation planning.
**Related:** #560 (environment model tracking), #557 (QualifiedPath landed), environment model spec (2026-03-28)

## Problem

`HostId` exists as a type (`QualifiedPath::host(HostId, PathBuf)`) and `host_identity.rs` exists with machine-id resolution and atomic UUID creation. But nothing calls them. The daemon still uses `HostName` for path qualification — `EnvironmentBag` carries `HostName`, factories read it, providers call `QualifiedPath::from_host_path()` which stringifies the hostname into `HostId`. The real UUID-based `HostId` is unused.

## Goal

The daemon's local machine gets a real stable `HostId` (UUID), and the discovery → factory → provider pipeline uses it for checkout path qualification instead of the hostname string.

## Changes

### DaemonConfig

Add `machine_id: Option<String>` for the NFS shared-home case (where `/etc/machine-id` and `IOPlatformUUID` are unavailable):

```rust
pub struct DaemonConfig {
    #[serde(default)]
    pub follower: bool,
    pub host_name: Option<String>,
    #[serde(default)]
    pub suppress_local_environment: bool,
    pub machine_id: Option<String>,
}
```

### HostId Resolution

`DaemonServer::new()` in `crates/flotilla-daemon/src/server.rs` is the production owner. It already reads `DaemonConfig` and has the config store and runner. Before constructing `InProcessDaemon`, it:

1. Reads `DaemonConfig` for `machine_id`.
2. Calls `machine_scoped_state_dir(config.state_dir(), config_machine_id, runner)` (exists in `host_identity.rs`).
3. Calls `resolve_or_create_host_id(scoped_dir)` (exists in `host_identity.rs`).
4. Injects the resulting `HostId` into `InProcessDaemon::new()`.

Tests inject `HostId` directly — no filesystem resolution needed.

### InProcessDaemon Constructor

Gains a `host_id: HostId` parameter:

```rust
pub async fn new(
    repo_paths: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    discovery: DiscoveryRuntime,
    host_name: HostName,
    host_id: HostId,
) -> Arc<Self>
```

Stores `host_id` as a field. Sets it on the host bag:

```rust
let mut host_bag = discovery::run_host_detectors(...).await;
host_bag.set_host_id(host_id.clone());
```

### EnvironmentBag

Replace `host_name: Option<HostName>` with `host_id: Option<HostId>`:

```rust
pub struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
    host_id: Option<flotilla_protocol::qualified_path::HostId>,
}
```

Methods:

```rust
pub fn set_host_id(&mut self, host_id: HostId) {
    self.host_id = Some(host_id);
}

pub fn host_id(&self) -> Option<&HostId> {
    self.host_id.as_ref()
}
```

`merge()` preserves `host_id` from `self`, falls back to `other` — same pattern as today's `host_name` merge.

The old `set_host_name()` / `host_name()` methods are removed.

### Checkout Manager Factories

All three factories read `env.host_id()` instead of `env.host_name()` and pass `HostId` to the provider constructor:

**`CloneCheckoutManagerFactory::probe()`:**
```rust
let host_id = env.host_id().cloned()
    .unwrap_or_else(|| HostId::new(HostName::local().as_str()));
Ok(Arc::new(CloneCheckoutManager::new(runner, reference_dir, host_id)))
```

**`GitCheckoutManagerFactory::probe()`:**
```rust
let host_id = env.host_id().cloned()
    .unwrap_or_else(|| HostId::new(HostName::local().as_str()));
Ok(Arc::new(GitCheckoutManager::new(checkout_config.path, runner, host_id)))
```

**`WtCheckoutManagerFactory::probe()`:**
```rust
let host_id = env.host_id().cloned()
    .unwrap_or_else(|| HostId::new(HostName::local().as_str()));
Ok(Arc::new(WtCheckoutManager::new(runner, host_id)))
```

**Fallback policy:** The `HostName::local()` fallback exists only for two cases:
1. **Tests** — factories probed without a full discovery runtime.
2. **Docker discovery hack** — explicitly deferred to Phase B.

In the local host discovery path (the normal daemon startup → host detection → repo discovery flow), absence of `host_id` on the bag is a bug. The production factory code should `expect("host_id must be set on bag during local discovery")` rather than silently falling back. The fallback only applies when `FLOTILLA_ENVIRONMENT_ID` is present in the bag (Docker path) or in test code.

### Provider Structs

All three checkout managers store `HostId` instead of `HostName`:

**`CloneCheckoutManager`:**
```rust
pub struct CloneCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    reference_dir: ExecutionEnvironmentPath,
    host_id: HostId,
}
```

**`GitCheckoutManager`:**
```rust
pub struct GitCheckoutManager {
    checkout_path: String,
    env: minijinja::Environment<'static>,
    runner: Arc<dyn CommandRunner>,
    host_id: HostId,
}
```

**`WtCheckoutManager`:**
```rust
pub struct WtCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    host_id: HostId,
}
```

All call sites change from `QualifiedPath::from_host_path(&self.host_name, path)` to `QualifiedPath::host(self.host_id.clone(), path)`.

### normalize_local_provider_hosts()

**Critical:** `normalize_local_provider_hosts()` in `in_process.rs` rewrites every checkout's `QualifiedPath` via `from_host_path(host_name, path)` after discovery. This would overwrite the real UUID-backed `HostId` from providers. It must take `HostId` instead of `HostName` and call `QualifiedPath::host(host_id, path)`. The companion `normalize_correlation_keys()` needs the same change.

### Other from_host_path() Production Callers

`from_host_path()` is used in 29 production call sites beyond the factories and providers. All must switch to `QualifiedPath::host(host_id, path)`:

- `in_process.rs` — `normalize_local_provider_hosts()`, `normalize_correlation_keys()` (take `HostId`)
- `executor.rs`, `executor/session_actions.rs`, `executor/terminals.rs`, `executor/workspace.rs` — executor paths that construct `QualifiedPath` (thread `HostId` through `ExecutorStepResolver`)
- `refresh.rs` — refresh path
- `convert.rs` — core-to-protocol conversion
- `repo_state.rs` — repo state management
- `peer/merge.rs` — peer data merge (takes `HostName` for the local host — needs `HostId`)

Each of these currently receives or captures `HostName` and calls `from_host_path()`. They need `HostId` threaded to them instead.

### from_host_path() Removal

After all production callers are migrated, `from_host_path()` moves behind `#[cfg(any(test, feature = "test-support"))]`. There are ~100 test call sites that can continue using it as a convenience helper (they don't need real UUIDs).

### Docker Discovery

Left unchanged. The executor's `DiscoverEnvironmentProviders` handler builds its own `EnvironmentBag` from raw env vars without setting `host_id`. Factories fall back to `HostId::new(HostName::local().as_str())`. This is already wrong (same as today's `HostName::local()` fallback) and is fixed in Phase B.

### Host Summary and Discovery Responses

No changes needed:
- `build_local_host_summary()` takes `HostName` as a separate parameter — doesn't read from the bag.
- `host_bag.assertions()` for discovery responses doesn't involve host identity.

## Not Changed

- `HostName` for mesh identity (peer maps, vector clocks, routing, display)
- `StepExecutionContext`
- `HostSummary` structure
- Docker environment discovery
- `EnvironmentId` for local machine
- `DaemonHandle` trait (only `InProcessDaemon` needs `HostId` for now; `SocketDaemon` doesn't construct providers)
- `suppress_local_environment` behavior

## Testing

- Existing `host_identity.rs` tests cover `HostId` generation and stability.
- Existing provider/factory tests pass with `HostId` instead of `HostName` after updating test helpers.
- Replay fixture tests are unaffected — fixtures capture command interactions, not internal identity resolution.
- The `qp()` test helper in `test_support.rs` already creates `QualifiedPath::host(HostId::new("test-host"), path)`.
