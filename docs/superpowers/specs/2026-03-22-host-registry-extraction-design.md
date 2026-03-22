# Host Registry Extraction

Extract host state management from `InProcessDaemon` into a focused `HostRegistry` struct.

## Problem

`InProcessDaemon` is 3,268 lines — the largest file in the codebase. Field affinity analysis reveals that four of its twenty fields (`hosts`, `configured_peer_names`, `topology_routes`, `local_host_summary`) form a cohesive cluster accessed together by 10+ methods and untouched by repo-centric operations. Six free functions already operate on the `hosts` map, and a private `host_queries` module exists solely to serve these methods. All the pieces of a sub-struct are present; they just lack a home.

## Design

### New module: `crates/flotilla-core/src/host_registry.rs`

Registered in `lib.rs` as `pub(crate) mod host_registry`. Replaces `mod host_queries` (absorbed).

### Struct

```rust
pub(crate) struct HostRegistry {
    host_name: HostName,
    hosts: RwLock<HashMap<HostName, HostState>>,
    configured_peer_names: RwLock<HashSet<HostName>>,
    topology_routes: RwLock<Vec<TopologyRoute>>,
    local_host_summary: HostSummary,
}
```

`HostState` moves into this module and stays private.

### Emit pattern

Mutation methods that produce events take an `emit: impl Fn(DaemonEvent)` parameter. In production, InProcessDaemon passes `|e| { let _ = self.event_tx.send(e); }`. In tests, any closure works — no broadcast channel required. The closure monomorphises away at zero runtime cost.

Methods that return values the caller needs for routing (e.g. `Option<HostSnapshot>` for peer forwarding) continue to return them alongside calling `emit`.

### Mutation methods

```rust
// Peer connection status changed — returns snapshot if changed.
pub(crate) async fn publish_peer_connection_status(
    &self, host: &HostName, status: PeerConnectionState,
    remote_counts: &HashMap<HostName, HostCounts>,
    emit: impl Fn(DaemonEvent),
) -> Option<HostSnapshot>

// Peer summary updated — returns snapshot if changed.
pub(crate) async fn publish_peer_summary(
    &self, host: &HostName, summary: HostSummary,
    emit: impl Fn(DaemonEvent),
) -> Option<HostSnapshot>

// Bulk-set peer summaries from peer manager.
pub(crate) async fn set_peer_host_summaries(
    &self, summaries: HashMap<HostName, HostSummary>,
    remote_counts: &HashMap<HostName, HostCounts>,
    emit: impl Fn(DaemonEvent),
)

// Update configured peer names from hosts.toml.
pub(crate) async fn set_configured_peer_names(
    &self, peers: Vec<HostName>,
    remote_counts: &HashMap<HostName, HostCounts>,
    emit: impl Fn(DaemonEvent),
)

// Replace topology routes.
pub(crate) async fn set_topology_routes(&self, routes: Vec<TopologyRoute>)

// Mirror host state from an incoming DaemonEvent (no emit — state update only).
// Uses try_write on the hosts lock — best-effort semantics.
// If the lock is contended, the update is silently skipped.
// This matches the current send_event behavior and is required because
// apply_event is called from synchronous contexts in the daemon server.
pub(crate) fn apply_event(&self, event: &DaemonEvent)
```

### Query methods

```rust
pub(crate) fn host_name(&self) -> &HostName
pub(crate) fn local_host_summary(&self) -> &HostSummary

pub(crate) async fn peer_connection_status(&self, host: &HostName) -> PeerConnectionState

pub(crate) async fn list_hosts(
    &self, local_counts: HostCounts, remote_counts: &HashMap<HostName, HostCounts>,
) -> HostListResponse

pub(crate) async fn get_host_status(
    &self, host: &str, local_counts: HostCounts, remote_counts: &HashMap<HostName, HostCounts>,
) -> Result<HostStatusResponse, String>

pub(crate) async fn get_host_providers(&self, host: &str) -> Result<HostProvidersResponse, String>

pub(crate) async fn get_topology(&self) -> TopologyResponse

// Replay host events for gap recovery (used by replay_since).
pub(crate) async fn replay_host_events(
    &self, last_seen: &HashMap<StreamKey, u64>,
) -> Vec<DaemonEvent>
```

Query methods that need repo/peer-derived counts (`local_counts`, `remote_counts`) receive them as parameters. InProcessDaemon computes these from `repos` and `peer_providers` before calling in.

Query methods that use `local_host_summary` for the local host (e.g. `get_host_status`, `get_host_providers`) resolve the fallback internally — callers do not pass the summary in.

### Private internals

These move from free functions in `in_process.rs` to private methods or helpers in `host_registry.rs`:

- `HostState` struct
- `ensure_remote_host_state`
- `build_host_snapshot`
- `default_host_summary`
- `update_host_status`
- `update_host_summary`
- `clear_host_summary`
- `should_present_host_state`
- `mark_host_removed`
- `sync_host_membership` (called internally by mutation methods)

### Absorbed module: `host_queries`

All functions from `host_queries.rs` move into `host_registry.rs` as private helpers:

- `known_hosts`
- `connection_status`
- `build_host_list_entry`
- `build_host_status`
- `build_host_providers`
- `build_topology`

`HostCounts` becomes `pub(crate)` on `host_registry`.

### Constructor

```rust
impl HostRegistry {
    pub(crate) fn new(host_name: HostName, local_host_summary: HostSummary) -> Self
}
```

Initializes `hosts` with the local host entry (`Connected`, summary present, seq 1). Same as the current inline initialization in `InProcessDaemon::new()`.

### InProcessDaemon changes

**Fields removed** (4):
- `hosts`
- `configured_peer_names`
- `topology_routes`
- `local_host_summary`

**Field added** (1):
- `host_registry: HostRegistry`

**Methods removed** from InProcessDaemon:
- `emit_host_membership_events` — replaced by emit closures
- `sync_host_membership` — moved into HostRegistry
- Host-specific branches in `send_event` — delegated to `host_registry.apply_event()`

**Methods that become thin delegates**:
- `publish_peer_connection_status` → `self.host_registry.publish_peer_connection_status(...)`
- `publish_peer_summary` → `self.host_registry.publish_peer_summary(...)`
- `set_configured_peer_names` → compute remote_counts, then delegate
- `set_peer_host_summaries` → compute remote_counts, then delegate
- `set_topology_routes` → `self.host_registry.set_topology_routes(...)`
- `list_hosts` → compute counts, then delegate
- `get_host_status` → compute counts, then delegate
- `get_host_providers` → delegate
- `get_topology` → delegate

**`send_event` changes**: The host-state mirroring match arms (PeerStatusChanged, HostSnapshot, HostRemoved) delegate to `self.host_registry.apply_event(&event)` before broadcasting via `event_tx`. Note: `apply_event` is only for events arriving through `send_event` (external sources, replay). HostRegistry mutation methods emit events directly through the emit closure, bypassing `send_event` — no double-application.

**`replay_since` changes**: The host-event replay block delegates to `self.host_registry.replay_host_events(last_seen)`. Repo replay stays in InProcessDaemon.

### Lock ordering

`HostRegistry` holds three internal `RwLock`s (`hosts`, `configured_peer_names`, `topology_routes`). InProcessDaemon methods that call into `HostRegistry` sometimes read `peer_providers` first to compute `remote_counts`. The implicit ordering is:

1. `peer_providers` (InProcessDaemon) — read to compute counts
2. `HostRegistry` internal locks — acquired by registry methods

Within `HostRegistry`, methods that call `sync_host_membership` release and re-acquire the `hosts` lock. This deliberate unlock-relock creates a window where another task could modify host state. This is safe because `sync_host_membership` is idempotent — it converges to the correct state regardless of interleaving.

### `host_name` duplication

Both `HostRegistry` and `InProcessDaemon` hold a `host_name: HostName`. InProcessDaemon uses its copy for non-host-registry purposes (snapshot building, command routing, provider normalization). HostRegistry uses its copy for host identity comparisons. Both copies are immutable after construction. This duplication is intentional — it avoids coupling the registry to its container.

### What doesn't change

- `HostName` — stays in `flotilla-protocol`, re-exported from core
- `host_summary.rs` — stays as a separate module (builds the local summary at startup)
- `DaemonHandle` trait — InProcessDaemon still implements it, delegates host queries
- `local_host_counts` / `remote_host_counts` — stay on InProcessDaemon (they need `repos` and `peer_providers`)

### Tests

- Unit tests for the free functions (`choose_event`, `now_iso8601`, etc.) stay in `in_process.rs`
- Tests for `host_queries` functions move to `host_registry.rs`
- New tests for HostRegistry mutation methods with captured emit closures
- Existing integration tests (`in_process_daemon`) are unaffected — they use the public `DaemonHandle` trait

### Estimated impact

- ~350 lines move out of `in_process.rs` (host state types, free functions, host_queries call sites, host-related DaemonHandle methods, replay_since host block)
- ~100 lines from `host_queries.rs` absorbed into `host_registry.rs`
- Net new file: ~500 lines
- `in_process.rs` drops from ~3,268 to ~2,920 lines
- InProcessDaemon field count drops from 20 to 17
- Import path changes: `crate::host_queries::HostCounts` → `crate::host_registry::HostCounts`
