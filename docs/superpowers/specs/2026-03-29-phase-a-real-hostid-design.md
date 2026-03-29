# Phase A: Wire Real HostId Through Discovery

**Status:** Design — ready for implementation planning.
**Related:** #560 (environment model tracking), #557 (QualifiedPath landed), environment model spec (2026-03-28)

## Problem

#557 replaced `HostPath` with `QualifiedPath` but smuggled hostname strings into `HostId` via `from_host_path()`. Every "host-qualified" path in the system actually contains a hostname string, not a real stable UUID. The type system can't distinguish migrated paths from legacy ones — `HostId` is doing double duty as both "real UUID" and "hostname string pretending to be a host ID."

Meanwhile `host_identity.rs` exists with machine-id resolution and atomic UUID creation, but nothing calls it.

## Goal

1. Restore type-level distinction between real `HostId` (UUID) and legacy `HostName`-derived paths.
2. Wire real `HostId` through the local daemon's discovery → factory → provider pipeline.
3. Make migration progress compiler-visible: `PathQualifier::HostName` usages are the remaining work.

## Core Change: Three-Variant PathQualifier

```rust
pub enum PathQualifier {
    /// Real stable host identity (UUID from host_identity.rs).
    /// Used for paths on hosts with resolved identity.
    Host(HostId),
    /// Legacy hostname-derived qualifier.
    /// Used for paths where only a HostName is available (remote/peer paths).
    /// Each usage is a migration target for future phases.
    HostName(HostName),
    /// Execution environment (Docker container, etc).
    Environment(EnvironmentId),
}
```

`from_host_path()` becomes the constructor for the `HostName` variant — it no longer pretends to produce a `HostId`:

```rust
pub fn from_host_path(host: &HostName, path: impl Into<PathBuf>) -> Self {
    Self { qualifier: PathQualifier::HostName(host.clone()), path: path.into() }
}
```

The compiler forces every match on `PathQualifier` to handle both `Host` and `HostName`, making it visible where migration is incomplete.

## Changes

### DaemonConfig

Add `machine_id: Option<String>` for the NFS shared-home case:

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

`DaemonServer::new()` in `crates/flotilla-daemon/src/server.rs:174` is the production owner. It receives `config: Arc<ConfigStore>` and `discovery: DiscoveryRuntime` (which carries `discovery.runner: Arc<dyn CommandRunner>`). Before constructing `InProcessDaemon`, it:

1. Reads `DaemonConfig` for `machine_id`.
2. Calls `machine_scoped_state_dir(config.state_dir(), config_machine_id, discovery.runner.as_ref())` (exists in `host_identity.rs`).
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

Carries `HostId` for the local daemon's identity. The old `host_name` field is replaced:

```rust
pub struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
    host_id: Option<flotilla_protocol::qualified_path::HostId>,
}
```

`set_host_id()` / `host_id()` replace `set_host_name()` / `host_name()`. `merge()` preserves `host_id` from `self`, falls back to `other`.

### Checkout Manager Factories — Local Path

All three factories read `env.host_id()` and produce `QualifiedPath::host(host_id, path)` for local checkouts:

```rust
let host_id = env.host_id().cloned()
    .expect("host_id must be set on bag during local discovery");
Ok(Arc::new(GitCheckoutManager::new(checkout_config.path, runner, host_id)))
```

These factories store `HostId` on the provider struct. The providers call `QualifiedPath::host(self.host_id.clone(), path)` — producing real `PathQualifier::Host(HostId)` paths.

### normalize_local_provider_hosts()

Takes `HostId` instead of `HostName`. Calls `QualifiedPath::host(host_id, path)` instead of `from_host_path()`. The companion `normalize_correlation_keys()` gets the same change.

### Call Sites That Stay on from_host_path()

These produce `PathQualifier::HostName(HostName)` — the legacy variant. The compiler makes each one visible:

- `executor/workspace.rs` — `target_host: &HostName` for remote workspace/attachable bindings
- `executor/terminals.rs` — terminal set allocation from `HostName`
- `executor/session_actions.rs` — session operations targeting remote hosts
- `executor.rs` — executor paths using `target_host`
- `peer/merge.rs` — peer data with hostname-derived identity
- `repo_state.rs` — may handle both local and peer data

These are the migration targets for future phases (Phase D / node identity).

### Merge Compatibility

`merge_provider_data()` currently compares `qp.host_id()` against `HostName`. After this change, local checkouts have `PathQualifier::Host(HostId)` and peer checkouts have `PathQualifier::HostName(HostName)`. The merge function needs to handle both:

```rust
fn is_local_checkout(qp: &QualifiedPath, local_host_id: &HostId) -> bool {
    match &qp.qualifier {
        PathQualifier::Host(id) => id == local_host_id,
        PathQualifier::HostName(name) => false,  // HostName-qualified paths are never "local" after Phase A
        PathQualifier::Environment(_) => false,
    }
}

fn is_peer_checkout(qp: &QualifiedPath, peer_host: &HostName) -> bool {
    match &qp.qualifier {
        PathQualifier::Host(_) => false,  // HostId-qualified paths come from real identity, not peer hostname
        PathQualifier::HostName(name) => name == peer_host,
        PathQualifier::Environment(_) => false,
    }
}
```

The merge function signature gains `local_host_id: &HostId` alongside or instead of `local_host: &HostName`.

### QualifiedPath API Changes

- `host_id()` returns `Option<&HostId>` — only for `Host` variant
- `host_name()` returns `Option<&HostName>` — only for `HostName` variant (new accessor)
- `from_host_path()` stays in production, now produces `HostName` variant
- `Display` / `FromStr` / serde need to handle both variants (e.g. `host:uuid:/path` vs `hostname:name:/path`, or a tag in serialized form)

### Serialization

The three variants need distinct serialized forms so that a `Host(HostId)` path round-trips differently from a `HostName(HostName)` path. Options:

- Tagged: `{"qualifier": {"Host": "uuid..."}, "path": "..."}` vs `{"qualifier": {"HostName": "desktop"}, "path": "..."}`
- Prefixed string: `host:uuid:/path` vs `hn:desktop:/path` vs `env:id:/path`

The existing `qualified_path_map` serde module and `Display`/`FromStr` impls need updating. The exact format is an implementation detail, but the round-trip must preserve the variant.

### Docker Discovery

Left unchanged. The executor's `DiscoverEnvironmentProviders` handler builds its own `EnvironmentBag` without `host_id`. Factories in this path need a fallback — they can use `from_host_path()` which now correctly produces `PathQualifier::HostName`, making the legacy status visible. Phase B fixes this.

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
- `DaemonHandle` trait
- `suppress_local_environment` behavior

## Testing

- `host_identity.rs` tests cover `HostId` generation and stability.
- Factory/provider tests updated to inject `HostId` and assert `PathQualifier::Host` on results.
- Existing `from_host_path()` test call sites continue working — they now produce `PathQualifier::HostName`, which is correct for test scenarios using hostname strings.
- `qp()` test helper produces `PathQualifier::Host(HostId::new("test-host"))` — unchanged.
- Snapshot tests may need updating if serialized `QualifiedPath` format changes.

## Migration Visibility

After Phase A, `grep -r 'PathQualifier::HostName\|from_host_path'` in production code shows exactly what's left to migrate. Each occurrence is a place where a real `HostId` should replace a hostname-derived path. The compiler enforces exhaustive matching, so new code must decide which variant to produce.
