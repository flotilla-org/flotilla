# Host Snapshot Events and TUI Host Panel Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface host information in the TUI via `HostSnapshot` daemon events, always-visible hosts panel, and per-host path shortening.

**Architecture:** Add `HostSnapshot` as a new `DaemonEvent` variant with its own sequence counter. Generalise `replay_since` cursors from `RepoIdentity` to a `StreamKey` enum supporting both repo and host streams. Replace `TuiModel.my_host`/`peer_hosts` with a unified `hosts: HashMap<HostName, TuiHostState>` map. Thread host home directories into path shortening.

**Tech Stack:** Rust, serde, ratatui, tokio broadcast channels.

**Spec:** `docs/superpowers/specs/2026-03-16-host-snapshot-events-design.md`

---

## Chunk 1: Protocol Types and Wire Format

### Task 1: Add `HostSnapshot` and `StreamKey` to flotilla-protocol

**Files:**
- Modify: `crates/flotilla-protocol/src/host_summary.rs` — add `HostSnapshot` struct, `Default` for `SystemInfo`
- Modify: `crates/flotilla-protocol/src/lib.rs` — add `HostSnapshot` to `DaemonEvent`, add `StreamKey` enum, update `ReplayCursor`, update re-exports

- [ ] **Step 1: Add `Default` derive to `SystemInfo`**

In `crates/flotilla-protocol/src/host_summary.rs`, add `Default` to the derive list on `SystemInfo`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SystemInfo {
```

(All fields are `Option` or already `Default`-deriving `HostEnvironment`.)

- [ ] **Step 2: Add `HostSnapshot` struct to the host_summary module**

In `crates/flotilla-protocol/src/host_summary.rs`, add after the existing types:

```rust
/// Full snapshot of one host's state — system info, inventory, provider health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub seq: u64,
    pub host_name: HostName,
    pub is_local: bool,
    pub connection_status: crate::PeerConnectionState,
    pub summary: HostSummary,
}
```

Add `HostSnapshot` to the `pub use host_summary::{...}` line in `lib.rs`.

- [ ] **Step 3: Add `StreamKey` enum to `lib.rs`**

In `crates/flotilla-protocol/src/lib.rs`, add:

```rust
/// Key for identifying an event stream in replay cursors.
/// Each stream has its own independent sequence counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StreamKey {
    #[serde(rename = "repo")]
    Repo { identity: RepoIdentity },
    #[serde(rename = "host")]
    Host { host_name: HostName },
}
```

- [ ] **Step 4: Update `ReplayCursor` to use `StreamKey`**

Change the existing `ReplayCursor` struct in `crates/flotilla-protocol/src/lib.rs` from:

```rust
pub struct ReplayCursor {
    pub repo_identity: RepoIdentity,
    pub seq: u64,
}
```

to:

```rust
pub struct ReplayCursor {
    pub stream: StreamKey,
    pub seq: u64,
}
```

- [ ] **Step 5: Add `HostSnapshot` variant to `DaemonEvent`**

In `crates/flotilla-protocol/src/lib.rs`, add to the `DaemonEvent` enum:

```rust
/// Full host snapshot — sent on initial connect/replay and when
/// a host's summary or connection status changes.
#[serde(rename = "host_snapshot")]
HostSnapshot(Box<HostSnapshot>),
```

- [ ] **Step 6: Add roundtrip tests**

Add to the test module in `crates/flotilla-protocol/src/host_summary.rs`:

```rust
#[test]
fn host_snapshot_roundtrips() {
    let snapshot = HostSnapshot {
        seq: 1,
        host_name: HostName::new("desktop"),
        is_local: true,
        connection_status: crate::PeerConnectionState::Connected,
        summary: HostSummary {
            host_name: HostName::new("desktop"),
            system: SystemInfo {
                home_dir: Some(PathBuf::from("/home/dev")),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(8),
                memory_total_mb: Some(16384),
                environment: HostEnvironment::Unknown,
            },
            inventory: ToolInventory::default(),
            providers: vec![],
        },
    };
    assert_roundtrip(&snapshot);
}
```

Add to `crates/flotilla-protocol/src/lib.rs` test module:

```rust
#[test]
fn stream_key_repo_roundtrip() {
    let key = StreamKey::Repo { identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() } };
    let json = serde_json::to_string(&key).expect("serialize");
    let decoded: StreamKey = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, key);
}

#[test]
fn stream_key_host_roundtrip() {
    let key = StreamKey::Host { host_name: HostName::new("desktop") };
    let json = serde_json::to_string(&key).expect("serialize");
    let decoded: StreamKey = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, key);
}

#[test]
fn daemon_event_host_snapshot_roundtrip() {
    let event = DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
        seq: 1,
        host_name: HostName::new("desktop"),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary {
            host_name: HostName::new("desktop"),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
        },
    }));
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, DaemonEvent::HostSnapshot(_)));
}

#[test]
fn replay_cursor_with_stream_key_roundtrip() {
    let cursor = ReplayCursor {
        stream: StreamKey::Host { host_name: HostName::new("laptop") },
        seq: 42,
    };
    let json = serde_json::to_string(&cursor).expect("serialize");
    let decoded: ReplayCursor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded.seq, 42);
    assert!(matches!(decoded.stream, StreamKey::Host { .. }));
}
```

- [ ] **Step 7: Fix broken existing tests**

The `ReplayCursor` change from `repo_identity` to `stream` will break existing roundtrip tests in `lib.rs`. Update them to use `StreamKey::Repo { identity: ... }`.

- [ ] **Step 8: Run full protocol tests and commit**

Run: `cargo test -p flotilla-protocol --locked`
Expected: All pass.

```bash
git add crates/flotilla-protocol/
git commit -m "feat: add HostSnapshot, StreamKey, and updated ReplayCursor to protocol"
```

---

## Chunk 2: DaemonHandle Trait and Core Implementations

### Task 2: Update `DaemonHandle` trait signature

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs` — change `replay_since` signature
- Modify: `crates/flotilla-tui/src/app/test_support.rs` — update `StubDaemon` impl

- [ ] **Step 1: Change `replay_since` signature in `DaemonHandle`**

In `crates/flotilla-core/src/daemon.rs`, change:

```rust
async fn replay_since(&self, last_seen: &HashMap<RepoIdentity, u64>) -> Result<Vec<DaemonEvent>, String>;
```

to:

```rust
async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String>;
```

Update the imports to include `StreamKey`.

- [ ] **Step 2: Update `StubDaemon` in test_support.rs**

In `crates/flotilla-tui/src/app/test_support.rs`, update the `replay_since` signature:

```rust
async fn replay_since(&self, _last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
    Ok(vec![])
}
```

Update imports accordingly.

- [ ] **Step 3: Verify compilation progress**

Run: `cargo check --workspace --locked`
Expected: Compilation errors in `flotilla-core` (InProcessDaemon), `flotilla-client` (SocketDaemon), and `flotilla-daemon` (server) — these are expected and fixed in the next tasks.

### Task 3: Update `InProcessDaemon::replay_since`

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs` — update signature, add host seq tracking, add host snapshot emission in replay, replace PeerStatusChanged in replay

- [ ] **Step 1: Add host seq counter field**

Add to `InProcessDaemon` struct:

```rust
/// Monotonic sequence counter for host snapshot events.
host_seq: std::sync::atomic::AtomicU64,
```

Initialize to `1` in `new()` (the local host snapshot starts at seq 1).

- [ ] **Step 2: Add `next_host_seq` helper**

```rust
pub fn next_host_seq(&self) -> u64 {
    self.host_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}
```

- [ ] **Step 3: Update `replay_since` implementation**

Change the signature to accept `HashMap<StreamKey, u64>`. For the repo iteration, extract repo cursors:

```rust
let repo_cursor = |identity: &RepoIdentity| -> Option<u64> {
    last_seen.get(&StreamKey::Repo { identity: identity.clone() }).copied()
};
```

Replace `last_seen.get(&state.identity)` with `repo_cursor(&state.identity)`.

Replace the `PeerStatusChanged` block (lines 1961-1965) with `HostSnapshot` events:

```rust
// Emit local host snapshot
let local_seq = self.host_seq.load(std::sync::atomic::Ordering::Relaxed);
let local_stale = last_seen.get(&StreamKey::Host { host_name: self.host_name.clone() })
    .map_or(true, |&seq| seq < local_seq);
if local_stale {
    events.push(DaemonEvent::HostSnapshot(Box::new(flotilla_protocol::HostSnapshot {
        seq: local_seq,
        host_name: self.host_name.clone(),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: self.local_host_summary.clone(),
    })));
}

// Emit host snapshots for known peers (connected + configured-but-disconnected)
let peer_status = self.peer_status.read().await;
let summaries = self.peer_host_summaries.read().await;
let configured = self.configured_peer_names.read().await;

let all_peer_names: HashSet<_> = peer_status.keys().chain(configured.iter()).collect();
for host_name in all_peer_names {
    let status = peer_status.get(host_name).cloned().unwrap_or(PeerConnectionState::Disconnected);
    let summary = summaries.get(host_name).cloned().unwrap_or_else(|| HostSummary {
        host_name: host_name.clone(),
        system: SystemInfo::default(),
        inventory: ToolInventory::default(),
        providers: vec![],
    });
    events.push(DaemonEvent::HostSnapshot(Box::new(flotilla_protocol::HostSnapshot {
        seq: local_seq,
        host_name: host_name.clone(),
        is_local: false,
        connection_status: status,
        summary,
    })));
}
```

- [ ] **Step 4: Update existing `replay_since` tests**

Tests in `crates/flotilla-core/tests/in_process_daemon.rs` that call `replay_since` need:
1. Updated call signature: `HashMap<StreamKey, u64>` instead of `HashMap<RepoIdentity, u64>`
2. Updated assertions: look for `HostSnapshot` events instead of `PeerStatusChanged`

- [ ] **Step 5: Run tests and commit**

Run: `cargo test -p flotilla-core --locked`
Run: `cargo test -p flotilla-core --locked --features test-support --test in_process_daemon`
Expected: Pass.

```bash
git add crates/flotilla-core/ crates/flotilla-protocol/
git commit -m "feat: emit HostSnapshot events from InProcessDaemon replay_since"
```

### Task 4: Update socket client (`flotilla-client`)

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs` — update `SeqMap`, `encode_replay_cursors`, `recover_from_gap`, event handling, and all related tests

- [ ] **Step 1: Update `SeqMap` type alias**

Change (line 29):
```rust
type SeqMap = std::sync::RwLock<HashMap<RepoIdentity, u64>>;
```
to:
```rust
type SeqMap = std::sync::RwLock<HashMap<StreamKey, u64>>;
```

Note: This is `std::sync::RwLock` deliberately (not tokio) — single-operation critical sections.

- [ ] **Step 2: Update `encode_replay_cursors`**

Change (line 384-386):
```rust
fn encode_replay_cursors(last_seen: &HashMap<StreamKey, u64>) -> Vec<ReplayCursor> {
    last_seen.iter().map(|(stream, &seq)| ReplayCursor { stream: stream.clone(), seq }).collect()
}
```

- [ ] **Step 3: Update `replay_since` seq seeding**

The code at ~line 627-635 seeds `local_seqs` from replay events. Add handling for `HostSnapshot`:

```rust
for event in &events {
    let (stream_key, seq) = match event {
        DaemonEvent::RepoSnapshot(snap) => (StreamKey::Repo { identity: snap.repo_identity.clone() }, snap.seq),
        DaemonEvent::RepoDelta(delta) => (StreamKey::Repo { identity: delta.repo_identity.clone() }, delta.seq),
        DaemonEvent::HostSnapshot(snap) => (StreamKey::Host { host_name: snap.host_name.clone() }, snap.seq),
        _ => continue,
    };
    let mut seqs = self.local_seqs.write().unwrap();
    seqs.entry(stream_key).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
}
```

- [ ] **Step 4: Update background reader `handle_event`**

In the `handle_event` function (~line 401-496), add a `DaemonEvent::HostSnapshot` match arm alongside the existing `RepoSnapshot`/`RepoDelta` handlers. Extract the stream key and seq, update `local_seqs`, forward the event via `event_tx.send()`.

The exhaustive match at ~line 488-494 that currently forwards other event types (including `PeerStatusChanged`) needs the new `HostSnapshot` arm added before the catch-all.

- [ ] **Step 5: Update `recover_from_gap`**

The function reads `local_seqs` and builds a replay request. Since the map is now `HashMap<StreamKey, u64>`, the encoding is handled by the updated `encode_replay_cursors`. The `recovering` map remains keyed by `RepoIdentity` — host streams don't need separate gap recovery since they use full snapshots. Verify compilation.

- [ ] **Step 6: Update all client tests**

The following tests in `crates/flotilla-client/src/lib.rs` construct `SeqMap` using `HashMap<RepoIdentity, u64>` and need updating to `HashMap<StreamKey, u64>`:

- `handle_event_updates_local_seqs_for_full_and_matching_delta` (~line 877)
- `handle_event_buffers_delta_when_recovery_already_running` (~line 911)
- `recover_from_gap_requests_replay_and_applies_seqs` (~line 936)
- `handle_event_starts_recovery_for_unknown_repo_delta` (~line 1000)
- `handle_event_starts_recovery_on_seq_gap` (~line 1026)
- `handle_event_forwards_repo_added` (~line 1055)
- `handle_event_forwards_command_started` (~line 1077)
- `handle_event_forwards_command_finished` (~line 1104)
- `handle_event_forwards_command_step_update` (~line 1131)
- `handle_event_forwards_peer_status_changed` (~line 1161)
- `handle_event_repo_removed_evicts_seq_and_forwards` (~line 1187)
- `recover_from_gap_handles_parse_error_gracefully` (~line 1215)
- `recover_from_gap_handles_request_failure_gracefully` (~line 1248)
- `recover_from_gap_handles_error_response_gracefully` (~line 1264)
- `recover_from_gap_applies_full_snapshot_seqs` (~line 1294)
- `recover_from_gap_does_not_regress_seq_from_concurrent_live_update` (~line 1329)
- `recover_from_gap_forwards_non_snapshot_replay_events` (~line 1366)
- `recover_from_gap_handles_empty_replay` (~line 1408)

Key change pattern: anywhere that inserts into or reads from `SeqMap`, wrap the `RepoIdentity` in `StreamKey::Repo { identity: ... }`.

- [ ] **Step 7: Run tests and commit**

Run: `cargo test -p flotilla-client --locked`
Expected: All pass.

```bash
git add crates/flotilla-client/
git commit -m "feat: update socket client for StreamKey-based replay cursors and HostSnapshot events"
```

### Task 5: Update server dispatch (`flotilla-daemon`)

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs` — update `Request::ReplaySince` dispatch, add `HostSnapshot` emission alongside `PeerStatusChanged` live events

- [ ] **Step 1: Update server replay dispatch**

At ~line 1832-1838, change the `ReplaySince` handler:

```rust
Request::ReplaySince { last_seen } => {
    let last_seen = last_seen.into_iter().map(|entry| (entry.stream, entry.seq)).collect();
    match ctx.daemon.replay_since(&last_seen).await {
        Ok(events) => Message::ok_response(id, Response::ReplaySince(events)),
        Err(e) => Message::error_response(id, e),
    }
}
```

- [ ] **Step 2: Emit `HostSnapshot` on `PeerWireMessage::HostSummary` receipt**

In `server.rs`, the `PeerWireMessage::HostSummary` handler (~line 596 in the peer relay loop) stores the summary via `set_peer_host_summaries`. After storing, also emit a `HostSnapshot` event:

```rust
daemon.send_event(DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
    seq: daemon.next_host_seq(),
    host_name: peer_name.clone(),
    is_local: false,
    connection_status: PeerConnectionState::Connected,
    summary: summary.clone(),
})));
```

- [ ] **Step 3: Emit `HostSnapshot` alongside existing `PeerStatusChanged` live events**

Find each `daemon.send_event(DaemonEvent::PeerStatusChanged { ... })` call in `server.rs` (there are ~8 call sites: initial connect, disconnect, reconnect, etc.). After each one, also emit a `HostSnapshot` with the current summary if available. Use `daemon.local_host_summary()` for the local host and look up peer summaries via `peer_host_summaries`. For disconnecting peers, use last-known summary. For configured-but-never-connected peers, use default empty summary.

The `PeerStatusChanged` stays for low-latency connection state changes; `HostSnapshot` provides the full picture.

- [ ] **Step 4: Update server tests**

The test at ~line 2660-2680 that calls `replay_since(&HashMap::new())` and checks for `PeerStatusChanged` events needs updating:
1. Change the `HashMap` type to `HashMap<StreamKey, u64>`
2. Change assertions to look for `HostSnapshot` events instead of `PeerStatusChanged`

Also update `Request::ReplaySince` test construction to use the new `ReplayCursor` format with `StreamKey`.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test -p flotilla-daemon --locked` (with sandbox skip if needed)
Expected: Pass.

```bash
git add crates/flotilla-daemon/
git commit -m "feat: update server dispatch for StreamKey replay cursors and HostSnapshot emission"
```

---

## Chunk 3: TUI Model, Event Handling, and Rendering

### Task 6: Add `TuiHostState` and update `TuiModel`

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs` — add `TuiHostState`, replace `my_host`/`peer_hosts` with `hosts` map, add helper methods, update event handlers

- [ ] **Step 1: Define `TuiHostState`**

In `crates/flotilla-tui/src/app/mod.rs`, add:

```rust
use flotilla_protocol::HostSummary;

/// Combined host state for display in the TUI.
#[derive(Debug, Clone)]
pub struct TuiHostState {
    pub host_name: HostName,
    pub is_local: bool,
    pub status: PeerStatus,
    pub summary: HostSummary,
}
```

Note: We store `PeerStatus` (the TUI enum) rather than `PeerConnectionState` (the protocol enum) — deliberate deviation from spec, consistent with existing TUI patterns.

- [ ] **Step 2: Replace `my_host` and `peer_hosts` with `hosts`**

In `TuiModel`, replace:
```rust
pub my_host: Option<HostName>,
pub peer_hosts: Vec<PeerHostStatus>,
```
with:
```rust
pub hosts: HashMap<HostName, TuiHostState>,
```

Update `from_repo_info` initialization:
```rust
hosts: HashMap::new(),
```

- [ ] **Step 3: Add helper methods**

```rust
impl TuiModel {
    pub fn my_host(&self) -> Option<&HostName> {
        self.hosts.values().find(|h| h.is_local).map(|h| &h.host_name)
    }

    pub fn peer_host_names(&self) -> Vec<HostName> {
        let mut peers: Vec<_> = self.hosts.values().filter(|h| !h.is_local).map(|h| h.host_name.clone()).collect();
        peers.sort();
        peers
    }

    pub fn home_dir_for_host(&self, host: &HostName) -> Option<&std::path::Path> {
        self.hosts.get(host).and_then(|h| h.summary.system.home_dir.as_deref())
    }
}
```

- [ ] **Step 4: Update `handle_daemon_event`**

Add the `HostSnapshot` arm:

```rust
DaemonEvent::HostSnapshot(snap) => {
    let status = PeerStatus::from(snap.connection_status);
    self.model.hosts.insert(snap.host_name.clone(), TuiHostState {
        host_name: snap.host_name,
        is_local: snap.is_local,
        status,
        summary: snap.summary,
    });
}
```

Update the `PeerStatusChanged` arm to use `hosts` map:

```rust
DaemonEvent::PeerStatusChanged { host, status } => {
    let peer_status = PeerStatus::from(status);
    let clear_target =
        matches!(peer_status, PeerStatus::Disconnected | PeerStatus::Rejected) && self.ui.target_host.as_ref() == Some(&host);
    if let Some(entry) = self.model.hosts.get_mut(&host) {
        entry.status = peer_status;
    }
    if clear_target {
        self.ui.target_host = None;
    }
}
```

Remove the `PeerHostStatus` struct (the `PeerStatus` enum stays).

- [ ] **Step 5: Remove `my_host` bootstrap from `apply_snapshot`**

In `apply_snapshot` (line 477-478), remove:
```rust
if self.model.my_host.is_none() {
    self.model.my_host = Some(snap.host_name.clone());
}
```

The local host identity is now set by `HostSnapshot(is_local: true)` during replay.

- [ ] **Step 6: Update `item_execution_host`**

Change (line 355-360):
```rust
fn item_execution_host(&self, item: &WorkItem) -> Option<HostName> {
    match self.model.my_host() {
        Some(my_host) if item.host != *my_host => Some(item.host.clone()),
        _ => None,
    }
}
```

- [ ] **Step 7: Update `peer_status_item` and `collect_visible_status_items`**

Change `peer_status_item` (line 195) to take `&TuiHostState`:

```rust
fn peer_status_item(index: usize, host: &TuiHostState) -> Option<VisibleStatusItem> {
    let label = match host.status {
        PeerStatus::Disconnected => "HOST DOWN",
        PeerStatus::Connecting => "HOST CONNECTING",
        PeerStatus::Reconnecting => "HOST RECONNECTING",
        PeerStatus::Connected => return None,
        PeerStatus::Rejected => "HOST REJECTED",
    };
    Some(VisibleStatusItem { id: index + 1, text: format!("{label} {}", host.host_name) })
}
```

Update `collect_visible_status_items` (line 206-219) to iterate `model.hosts.values()` filtering `!h.is_local` instead of `model.peer_hosts`.

- [ ] **Step 8: Fix compilation in key_handlers.rs**

Update `CycleHost` handler (line 265):
```rust
let peer_hosts = self.model.peer_host_names();
self.ui.cycle_target_host(&peer_hosts);
```

Update all `self.model.my_host` field references to `self.model.my_host()` method calls. The `is_allowed_for_host` function expects `&Option<HostName>`, so callers need: `let my_host = self.model.my_host().cloned();` then pass `&my_host`.

Key locations in `key_handlers.rs`: lines 510, 552, 560, 607.

- [ ] **Step 9: Update existing tests**

Tests that set `app.model.peer_hosts = vec![PeerHostStatus { ... }]` need updating to use `app.model.hosts.insert(...)` with `TuiHostState`. Tests that set `app.model.my_host = Some(...)` need updating to insert a `TuiHostState` with `is_local: true`.

All affected test locations:

| File | Lines | Usage |
|------|-------|-------|
| `app/mod.rs` | ~959 | `peer_hosts = vec![PeerHostStatus { ... }]` |
| `app/mod.rs` | ~1259 | `peer_hosts = vec![PeerHostStatus { ... }]` |
| `app/mod.rs` | ~1501 | `my_host = Some(HostName::new(...))` |
| `app/mod.rs` | ~1531 | `my_host = Some(HostName::new(...))` |
| `app/key_handlers.rs` | ~1213 | `peer_hosts = vec![PeerHostStatus { ... }, ...]` |
| `app/key_handlers.rs` | ~1283 | `peer_hosts = vec![PeerHostStatus { ... }]` |
| `app/intent.rs` | ~606 | `my_host = Some(HostName::local())` |
| `app/intent.rs` | ~774 | `my_host = Some(HostName::local())` |
| `app/intent.rs` | ~990 | `my_host = Some(HostName::local())` |
| `app/intent.rs` | ~1175-1229 | Multiple `my_host` test cases |

Helper for tests — consider adding a builder:
```rust
fn insert_local_host(model: &mut TuiModel, name: &str) {
    let host_name = HostName::new(name);
    model.hosts.insert(host_name.clone(), TuiHostState {
        host_name,
        is_local: true,
        status: PeerStatus::Connected,
        summary: HostSummary { host_name: HostName::new(name), system: SystemInfo::default(), inventory: ToolInventory::default(), providers: vec![] },
    });
}
```

- [ ] **Step 10: Add new TUI unit tests**

Add tests for the new functionality:

```rust
#[test]
fn host_snapshot_event_populates_hosts_map() {
    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
        seq: 1,
        host_name: HostName::new("desktop"),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary { /* ... */ },
    })));
    assert_eq!(app.model.my_host(), Some(&HostName::new("desktop")));
    assert!(app.model.hosts.get(&HostName::new("desktop")).unwrap().is_local);
}

#[test]
fn my_host_returns_none_before_host_snapshot() {
    let app = stub_app();
    assert!(app.model.my_host().is_none());
}

#[test]
fn peer_host_names_returns_sorted_non_local() {
    let mut app = stub_app();
    // Insert local + two peers
    insert_local_host(&mut app.model, "local");
    insert_peer_host(&mut app.model, "beta");
    insert_peer_host(&mut app.model, "alpha");
    assert_eq!(app.model.peer_host_names(), vec![HostName::new("alpha"), HostName::new("beta")]);
}
```

- [ ] **Step 11: Run tests and commit**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

```bash
git add crates/flotilla-tui/src/app/
git commit -m "feat: replace my_host/peer_hosts with unified hosts map, handle HostSnapshot events"
```

### Task 7: Update hosts panel rendering

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs` — update `render_config_screen` and `render_hosts_status`

- [ ] **Step 1: Always show the hosts panel**

In `render_config_screen` (line 1188-1207), remove the `if model.peer_hosts.is_empty()` gate. Always split the left panel:

```rust
fn render_config_screen(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let host_count = model.hosts.len();
    let host_height = (host_count as u16 + 2).min(8);
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(host_height)])
        .split(chunks[0]);
    render_global_status(model, theme, frame, left_chunks[0]);
    render_hosts_status(model, theme, frame, left_chunks[1]);

    render_event_log(ui, theme, frame, chunks[1]);
}
```

- [ ] **Step 2: Rewrite `render_hosts_status` as a Table**

Replace the `List`-based implementation with a `Table` showing system info and provider health:

```rust
fn render_hosts_status(model: &TuiModel, theme: &Theme, frame: &mut Frame, area: Rect) {
    // Sort: local first, then peers alphabetically
    let mut hosts: Vec<&TuiHostState> = model.hosts.values().collect();
    hosts.sort_by(|a, b| match (a.is_local, b.is_local) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.host_name.cmp(&b.host_name),
    });

    let rows: Vec<Row> = hosts.iter().map(|h| {
        let (icon, icon_style) = match h.status {
            PeerStatus::Connected => ("\u{25cf}", Style::default().fg(theme.status_ok)),
            PeerStatus::Disconnected => ("\u{25cb}", Style::default().fg(theme.error)),
            PeerStatus::Connecting => ("\u{25d0}", Style::default().fg(theme.warning)),
            PeerStatus::Reconnecting => ("\u{25d0}", Style::default().fg(theme.warning)),
            PeerStatus::Rejected => ("\u{2717}", Style::default().fg(theme.error)),
        };

        let name = if h.is_local {
            format!("{} (local)", h.host_name)
        } else {
            h.host_name.to_string()
        };

        let sys = &h.summary.system;
        let os_arch = match (sys.os.as_deref(), sys.arch.as_deref()) {
            (Some(os), Some(arch)) => format!("{os}/{arch}"),
            (Some(os), None) => os.to_string(),
            _ => "\u{2014}".to_string(),
        };
        let cpus = sys.cpu_count.map_or("\u{2014}".to_string(), |c| format!("{c} CPUs"));
        let mem = sys.memory_total_mb.map_or("\u{2014}".to_string(), |m| {
            if m >= 1024 { format!("{} GB", m / 1024) } else { format!("{m} MB") }
        });

        let providers: String = h.summary.providers.iter().map(|p| {
            let check = if p.healthy { "\u{2713}" } else { "\u{2717}" };
            format!("{} {check}", p.name)
        }).collect::<Vec<_>>().join("  ");

        Row::new(vec![
            Cell::from(Span::styled(format!("{icon} "), icon_style)),
            Cell::from(name),
            Cell::from(os_arch),
            Cell::from(cpus),
            Cell::from(mem),
            Cell::from(providers),
        ])
    }).collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Fill(1),
    ];

    let table = Table::new(rows, widths)
        .block(Block::bordered().style(theme.block_style()).title(" Hosts "));
    frame.render_widget(table, area);
}
```

Update imports: add `TuiHostState`, `PeerStatus` to the `use crate::app::{...}` line.

- [ ] **Step 3: Run tests and commit**

Run: `cargo test -p flotilla-tui --locked`
Expected: Pass.

```bash
git add crates/flotilla-tui/src/ui.rs
git commit -m "feat: always-visible hosts panel with system info and provider health"
```

### Task 8: Path shortening with per-host home directory

**Files:**
- Modify: `crates/flotilla-tui/src/ui_helpers.rs` — add `home_dir` parameter to `shorten_path` and `shorten_against_home`
- Modify: `crates/flotilla-tui/src/ui.rs` — thread `home_dir` into `build_item_row`

- [ ] **Step 1: Update `shorten_against_home` signature**

In `crates/flotilla-tui/src/ui_helpers.rs`, change (line 199):

```rust
fn shorten_against_home(path: &Path, home_dir: Option<&Path>) -> String {
    if let Some(home) = home_dir {
```

- [ ] **Step 2: Update `shorten_path` signature and all internal calls**

Add `home_dir: Option<&Path>` parameter. Update **both** calls to `shorten_against_home` — the main display at line 145 and the fallback at line 196:

```rust
pub fn shorten_path(path: &Path, repo_root: &Path, col_width: usize, home_dir: Option<&Path>) -> String {
    let main_display = shorten_against_home(repo_root, home_dir);
    // ... (lines 148-193 unchanged) ...
    // Fallback at end (line 196):
    shorten_against_home(path, home_dir)
}
```

- [ ] **Step 3: Update existing tests**

All test calls to `shorten_path` need the new parameter. For tests with home-relative paths, pass the specific home dir. For tests with absolute paths outside home, pass `None`.

```rust
// Tests using dirs::home_dir():
fn shorten_path_main_checkout_under_home() {
    let home = dirs::home_dir().expect("home dir");
    let root = home.join("dev/flotilla");
    assert_eq!(shorten_path(&root, &root, 40, Some(&home)), "~/dev/flotilla");
}

// Tests with absolute paths not under home:
fn shorten_path_main_checkout() {
    let root = Path::new("/dev/project");
    assert_eq!(shorten_path(root, root, 40, None), "/dev/project");
}
```

- [ ] **Step 4: Add tests for remote host home directory**

```rust
#[test]
fn shorten_path_remote_host_home() {
    let remote_home = Path::new("/home/remoteuser");
    let root = Path::new("/home/remoteuser/dev/project");
    assert_eq!(shorten_path(root, root, 40, Some(remote_home)), "~/dev/project");
}

#[test]
fn shorten_path_no_home_dir() {
    let root = Path::new("/srv/repos/project");
    assert_eq!(shorten_path(root, root, 40, None), "/srv/repos/project");
}
```

- [ ] **Step 5: Update `build_item_row` to accept and use `home_dir`**

In `crates/flotilla-tui/src/ui.rs`, add `home_dir: Option<&Path>` to `build_item_row` (line 632):

```rust
fn build_item_row<'a>(
    item: &WorkItem,
    providers: &ProviderData,
    col_widths: &[u16],
    repo_root: &Path,
    prev_source: Option<&str>,
    pending: Option<&PendingAction>,
    theme: &Theme,
    home_dir: Option<&Path>,
) -> Row<'a> {
```

Update the `shorten_path` call inside (line 655):
```rust
ui_helpers::shorten_path(&p.path, repo_root, path_width, home_dir)
```

- [ ] **Step 6: Update the caller to resolve home_dir**

At the call site (line 563-566), resolve the home dir from the model's hosts map. Hold the `dirs::home_dir()` result in a let binding to avoid dangling reference:

```rust
GroupEntry::Item(item) => {
    let pending = rui.pending_actions.get(&item.identity);
    let local_home = dirs::home_dir();
    let home_dir = item.checkout_key()
        .and_then(|co| model.hosts.get(&co.host))
        .and_then(|h| h.summary.system.home_dir.as_deref())
        .or(local_home.as_deref());
    let mut row = build_item_row(
        item, &rm.providers, &col_widths, model.active_repo_root(),
        prev_source.as_deref(), pending, theme, home_dir,
    );
```

Note: `home_dir` is `Option<&Path>` — pass it directly to `build_item_row`, not `home_dir.as_deref()`.

- [ ] **Step 7: Run tests and commit**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

```bash
git add crates/flotilla-tui/src/ui_helpers.rs crates/flotilla-tui/src/ui.rs
git commit -m "feat: per-host path shortening using home_dir from host summaries"
```

---

## Chunk 4: Event Loop and CLI Watch Updates

### Task 9: Update TUI event loop and CLI watch

**Files:**
- Modify: `crates/flotilla-tui/src/run.rs` — update `replay_since` call
- Modify: `crates/flotilla-tui/src/cli.rs` — update `replay_since` call, `replay_seqs` tracking, and `event_seq` helper

- [ ] **Step 1: Update `run.rs` replay_since call**

The call at `run.rs` line 26 currently passes `&HashMap::new()`. Update with type annotation:

```rust
let replay_events = app.daemon
    .replay_since(&HashMap::<flotilla_protocol::StreamKey, u64>::new())
    .await
    .unwrap_or_default();
```

- [ ] **Step 2: Update CLI watch replay_since call**

In `crates/flotilla-tui/src/cli.rs`, the `watch` function (~line 449-450) has:
- A `replay_since` call with `&HashMap::new()` — update type to `HashMap<StreamKey, u64>`
- A `replay_seqs: HashMap<RepoIdentity, u64>` tracking map (~line 449) — update to `HashMap<StreamKey, u64>`
- An `event_seq` helper function (~line 260) that extracts `(RepoIdentity, u64)` from events — update to return `Option<(StreamKey, u64)>` and add a `HostSnapshot` arm

- [ ] **Step 3: Run full workspace build, tests, lint, format**

Run:
```bash
cargo test --workspace --locked
cargo clippy --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt
```
Expected: All pass, clean.

```bash
git add -A
git commit -m "feat: update TUI event loop and CLI watch for StreamKey-based replay"
```

### Task 10: Final verification

- [ ] **Step 1: Build**

Run: `cargo build --locked`
Expected: Clean build.

- [ ] **Step 2: Run all tests**

Run: `cargo test --workspace --locked`
Expected: All pass.

- [ ] **Step 3: Lint**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: Clean.

- [ ] **Step 4: Format**

Run: `cargo +nightly-2026-03-12 fmt`
