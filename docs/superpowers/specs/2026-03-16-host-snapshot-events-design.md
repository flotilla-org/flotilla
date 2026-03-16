# Host Snapshot Events and TUI Host Panel Design

**Date:** 2026-03-16
**Status:** Approved

## Goal

Surface host information in the TUI by:

1. Adding `HostSnapshot` daemon events parallel to the existing `RepoSnapshot` / `RepoDelta` pattern.
2. Always showing a hosts panel on the overview tab — local host first, then peers — with system info and provider health.
3. Using each host's `home_dir` from its `HostSummary` to shorten checkout paths in the main table (not just the local home directory).

## Background

Host summaries (`HostSummary`) are already collected and stored by the daemon — the local summary at startup, remote summaries when peers send `PeerWireMessage::HostSummary`. But this data is only available via query APIs (`list_hosts`, `get_host_status`). It is never pushed to TUI clients as events.

The TUI currently tracks peer hosts via `PeerStatusChanged` events (connection status only) and hides the hosts panel entirely when no peers are configured. The `my_host` field is bootstrapped from the first `RepoSnapshot.host_name`, creating an implicit dependency on repo events for host identity.

## Event Stream Key Types

The system has three logical event stream dimensions:

| Stream | Key | Examples |
|--------|-----|----------|
| **Host** | `HostName` | System info, tool inventory, provider health |
| **Repo** (host-independent) | `RepoIdentity` | Issues, PRs, remote branches |
| **Host × Repo** | `(HostName, RepoIdentity)` | Checkouts, workspaces, sessions |

Currently the repo and host×repo streams are combined in `RepoSnapshot`/`RepoDelta`. This design adds the host stream as a first-class event type with its own sequence counter, using the same snapshot pattern for symmetry and future extensibility.

## Design

### New Protocol Types

Add to `flotilla-protocol`:

```rust
/// Full snapshot of one host's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub seq: u64,
    pub host_name: HostName,
    pub is_local: bool,
    pub connection_status: PeerConnectionState,
    pub summary: HostSummary,
}
```

Notes:

- `seq` gives the host stream its own sequence counter, independent of repo seqs. A single monotonic counter shared across all hosts (not per-host) — simpler than per-host seqs and sufficient since host events are infrequent.
- `is_local` lets the TUI distinguish the local host without external comparison.
- `connection_status` is included so the TUI gets the full picture from one event type.
- No `HostDelta` initially. Unlike `RepoDelta` which carries `changes: Vec<Change>` with actual differential data, a host delta would currently be identical to a snapshot (the summary is a single blob, not a list). Add `HostDelta` later when host-level data includes lists that benefit from incremental updates.

### New DaemonEvent Variant

```rust
pub enum DaemonEvent {
    // ... existing variants ...

    /// Full host snapshot — sent on initial connect/replay and when
    /// a host's summary or connection status changes.
    #[serde(rename = "host_snapshot")]
    HostSnapshot(Box<HostSnapshot>),
}
```

### Replay Cursor Generalisation

The `DaemonHandle` trait's `replay_since` currently takes `HashMap<RepoIdentity, u64>`. The wire protocol uses `Request::ReplaySince { last_seen: Vec<ReplayCursor> }` where `ReplayCursor` has `repo_identity: RepoIdentity` and `seq: u64`.

Both need to support host stream cursors.

**Introduce `StreamKey`:**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StreamKey {
    #[serde(rename = "repo")]
    Repo { identity: RepoIdentity },
    #[serde(rename = "host")]
    Host { host_name: HostName },
}
```

**Change the `DaemonHandle` trait:**

```rust
async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String>;
```

**Replace `ReplayCursor`** in the wire protocol:

```rust
// Before:
pub struct ReplayCursor { pub repo_identity: RepoIdentity, pub seq: u64 }
ReplaySince { last_seen: Vec<ReplayCursor> }

// After:
pub struct ReplayCursor { pub stream: StreamKey, pub seq: u64 }
ReplaySince { last_seen: Vec<ReplayCursor> }
```

**Update `encode_replay_cursors`** in `flotilla-client/src/lib.rs` to convert `HashMap<StreamKey, u64>` to `Vec<ReplayCursor>`.

**Update server-side dispatch** in `flotilla-daemon/src/server.rs` to deserialize the new cursor format and pass it through.

On the daemon side:

- For `StreamKey::Repo` entries: existing repo replay logic (delta log or full snapshot).
- For `StreamKey::Host` entries: compare seq, return `HostSnapshot` if stale or missing.
- Hosts not in `last_seen`: return `HostSnapshot` for the local host, all connected peers, and configured-but-disconnected peers (with `Disconnected` status and empty/default summary).
- Repos not in `last_seen`: existing behaviour (return `RepoSnapshot`).

### Relationship to Existing `PeerStatusChanged` Replay

The current `replay_since` implementation appends `PeerStatusChanged` events for all known peers (in `in_process.rs` lines 1961-1965). With `HostSnapshot` now carrying `connection_status`, the `PeerStatusChanged` events in replay are redundant.

**Decision:** Remove `PeerStatusChanged` from `replay_since` output. `HostSnapshot` events in replay carry the authoritative connection status. `PeerStatusChanged` continues to be emitted as a live event for low-latency connection state changes during normal operation.

The TUI handles both:

- `PeerStatusChanged` (live): update connection indicator immediately on existing entry.
- `HostSnapshot` (replay + live): full host model update.

### Daemon Emission Points

**Local host:**

- At startup (available immediately in `replay_since`): emit `HostSnapshot` with `is_local: true`, `connection_status: Connected`.
- The local host seq starts at 1 and is static for now (no dynamic refresh of local host info).

**Remote hosts (connected):**

- When `PeerWireMessage::HostSummary` arrives from a peer: emit `HostSnapshot` with `is_local: false`, `connection_status: Connected`.
- On peer disconnect: emit `HostSnapshot` with `connection_status: Disconnected` and the last-known summary. The TUI retains the host info while showing it as disconnected.

**Remote hosts (configured but never connected):**

- In `replay_since`: emit `HostSnapshot` with `connection_status: Disconnected` and a default empty summary for each configured peer that has never connected. This ensures the hosts panel shows all configured hosts from the start.

**Sequence numbering:**

A single monotonic `host_seq: u64` counter on the daemon, incremented on every `HostSnapshot` emission regardless of which host changed. Simpler than per-host counters, and host events are infrequent enough that a shared counter adds no meaningful overhead.

### Socket Client Changes

The socket client's `local_seqs` (currently `type SeqMap = std::sync::RwLock<HashMap<RepoIdentity, u64>>`, using `std::sync::RwLock` deliberately for single-operation critical sections) becomes:

```rust
type SeqMap = std::sync::RwLock<HashMap<StreamKey, u64>>;
```

The background reader extracts the stream key and seq from `RepoSnapshot`/`RepoDelta` events (as `StreamKey::Repo`) and from `HostSnapshot` events (as `StreamKey::Host`).

The `recover_from_gap` function (which reads `local_seqs` to build a replay request) needs updating to include both repo and host stream keys in the gap recovery `replay_since` call.

### TUI Model Changes

```rust
pub struct TuiModel {
    // ... existing fields ...

    /// Host state keyed by hostname.
    /// Includes the local host and all known peers.
    pub hosts: HashMap<HostName, TuiHostState>,
}

pub struct TuiHostState {
    pub host_name: HostName,
    pub is_local: bool,
    pub connection_status: PeerConnectionState,
    pub summary: HostSummary,
}
```

The existing `my_host: Option<HostName>` and `peer_hosts: Vec<PeerHostStatus>` fields are replaced by `hosts`.

**Helper methods** for backward-compatible access:

```rust
impl TuiModel {
    pub fn my_host(&self) -> Option<&HostName> {
        self.hosts.values().find(|h| h.is_local).map(|h| &h.host_name)
    }

    pub fn peer_host_names(&self) -> Vec<HostName> {
        let mut peers: Vec<_> = self.hosts.values()
            .filter(|h| !h.is_local)
            .map(|h| h.host_name.clone())
            .collect();
        peers.sort();
        peers
    }
}
```

**Bootstrap ordering:** Currently `my_host` is set from `RepoSnapshot.host_name` in `apply_snapshot`. With this change, `my_host` is set from the `HostSnapshot` event with `is_local: true`. Since `replay_since` emits host snapshots alongside repo snapshots (and the local host snapshot is always available), the local host identity is established during replay before any code needs it. The `RepoSnapshot.host_name` field can be left in place but is no longer used by the TUI model to set `my_host`.

### Callsite Migration

The following callsites reference `model.my_host` or `model.peer_hosts` and need updating:

| File | Usage | Migration |
|------|-------|-----------|
| `app/mod.rs:apply_snapshot` | Sets `my_host` from `snap.host_name` | Remove — `my_host()` derived from `hosts` map |
| `app/mod.rs:item_execution_host` | Compares `item.host != *my_host` | Use `self.model.my_host()` |
| `app/mod.rs:handle_daemon_event(PeerStatusChanged)` | Mutates `peer_hosts` vec | Update connection status in `hosts` map |
| `app/mod.rs` (status bar items) | Iterates `peer_hosts` | Use `self.model.hosts.values()` |
| `app/key_handlers.rs:CycleHost` | Reads `peer_hosts` for cycling | Use `self.model.peer_host_names()` |
| `app/key_handlers.rs` (intent resolution) | Reads `my_host` for `is_allowed_for_host` | Use `self.model.my_host()` |
| `app/intent.rs:is_allowed_for_host` | Takes `my_host: &Option<HostName>` | Signature stays, callers pass `self.model.my_host().cloned()` |
| `ui.rs:render_config_screen` | Gates hosts panel on `peer_hosts.is_empty()` | Always render, use `model.hosts` |
| `ui.rs:render_hosts_status` | Takes `&[PeerHostStatus]` | Takes `&HashMap<HostName, TuiHostState>` |

### TUI Event Handling

In `handle_daemon_event`:

```rust
DaemonEvent::HostSnapshot(snap) => {
    self.model.hosts.insert(snap.host_name.clone(), TuiHostState {
        host_name: snap.host_name,
        is_local: snap.is_local,
        connection_status: snap.connection_status,
        summary: snap.summary,
    });
}
DaemonEvent::PeerStatusChanged { host, status } => {
    // Update connection status on existing entry if present.
    if let Some(entry) = self.model.hosts.get_mut(&host) {
        entry.connection_status = status.into();
    }
    // If no entry yet, the HostSnapshot will arrive shortly.
}
```

### Hosts Panel Rendering

The hosts panel on the overview/config tab always renders (not gated on `peer_hosts.is_empty()`).

Display order: local host first, then peers sorted alphabetically.

Each host row shows:

```
● hostname (local)    linux/aarch64  8 CPUs  16 GB   Git ✓  GitHub ✓
○ remote-vm           linux/x86_64   4 CPUs   8 GB   Git ✓  GitHub ✗
```

Layout per row:

| Element | Source |
|---------|--------|
| Connection icon | `●`/`○`/`◐`/`✗` from connection status |
| Hostname | `host_name` |
| Local tag | `(local)` if `is_local` |
| OS/arch | `system.os` / `system.arch` |
| CPU count | `system.cpu_count` |
| Memory | `system.memory_total_mb` formatted as GB |
| Provider health | From `summary.providers` — compact `Name ✓/✗` |

Use a `Table` widget (replacing the current `List`) for column alignment. The provider health column can be a compact roll-up: show category abbreviations with check/cross marks. Missing optional fields render as `—`.

### Path Shortening for Remote Hosts

Currently `shorten_path` calls `shorten_against_home()` which uses `dirs::home_dir()` — the local machine's home directory. For remote host checkouts, this won't match.

Change `shorten_path` and `shorten_against_home` to accept an explicit `home_dir: Option<&Path>` parameter:

```rust
pub fn shorten_path(path: &Path, repo_root: &Path, col_width: usize, home_dir: Option<&Path>) -> String {
    let main_display = shorten_against_home(repo_root, home_dir);
    // ... rest unchanged, all internal calls to shorten_against_home pass home_dir ...
    // Including the fallback at the end of the function (line 196)
    shorten_against_home(path, home_dir)
}

fn shorten_against_home(path: &Path, home_dir: Option<&Path>) -> String {
    if let Some(home) = home_dir {
        if let Ok(rel) = path.strip_prefix(home) {
            let s = rel.to_string_lossy();
            if s.is_empty() {
                return "~".to_string();
            }
            return format!("~/{s}");
        }
    }
    path.display().to_string()
}
```

**Caller changes in `render_work_item_row`:** This function (`ui.rs` line ~640) currently takes `(item, providers, repo_root, col_widths, theme)` and does not have access to host data. Add a `home_dir: Option<&Path>` parameter. The caller (`render_repo_table`) resolves the home directory before calling:

```rust
let home_dir = item.checkout_key()
    .and_then(|co| model.hosts.get(&co.host))
    .and_then(|h| h.summary.system.home_dir.as_deref())
    .or_else(|| dirs::home_dir().as_deref());
```

This makes path shortening symmetric: `~/dev/repo` works for both local and remote checkouts, using each host's own home directory. Falls back to local `dirs::home_dir()` if no host snapshot has arrived yet.

## Error Handling

- If the local host summary is unavailable at startup (shouldn't happen, but defensively): emit a `HostSnapshot` with empty/default `SystemInfo`. The TUI shows what it has.
- If a peer connects but hasn't sent its `HostSummary` yet: the `PeerStatusChanged` live event updates connection status on any existing entry. The `HostSnapshot` with full summary arrives once the peer message is processed.
- Missing optional fields in `SystemInfo` render as `—` in the TUI.
- Configured-but-never-connected peers appear in the hosts panel with `Disconnected` status and empty system info.

## Testing Strategy

### Protocol Tests

- `HostSnapshot` serde roundtrip.
- `DaemonEvent::HostSnapshot` roundtrip.
- `StreamKey` serde roundtrip (both `Repo` and `Host` variants).
- Updated `ReplayCursor` roundtrip with `StreamKey`.

### Daemon Tests

- `replay_since` with empty cursors returns `HostSnapshot` for local host (and configured peers).
- `replay_since` with empty cursors no longer returns `PeerStatusChanged` events.
- `replay_since` with current host seq returns no host events.
- `replay_since` with stale host seq returns `HostSnapshot`.
- Peer connect triggers `HostSnapshot` emission.
- Peer disconnect triggers `HostSnapshot` with `Disconnected` status.

### TUI Tests

- `handle_daemon_event(HostSnapshot)` populates `model.hosts`.
- `my_host()` returns the local host name.
- `peer_host_names()` returns sorted non-local host names.
- Hosts panel renders local host first, peers after.
- Path shortening uses remote host's home directory for remote checkouts.
- Path shortening falls back to `dirs::home_dir()` when no host snapshot exists.

### Integration Tests

- Full round-trip: daemon emits `HostSnapshot` → socket client receives → TUI model updated.
- Reconnect: socket client replays host streams alongside repo streams.
