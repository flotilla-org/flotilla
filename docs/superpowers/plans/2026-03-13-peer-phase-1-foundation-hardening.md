# Peer Phase 1 Foundation Hardening — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the peer networking layer with four sequential changes: extract PeerNetworkingTask (#292), add session ID to handshake (#259), fix HOL blocking in relay (#264), and add restart detection with keepalive (#258).

**Architecture:** Each issue builds on the previous. The extraction (#292) restructures ~600 lines from `server.rs` into a standalone `PeerNetworkingTask`. Subsequent issues modify the extracted code. All changes stay within `flotilla-daemon` and `flotilla-protocol`, with a small integration change in `src/main.rs`.

**Tech Stack:** Rust, tokio, async-trait, uuid (new), futures (new for join_all)

**Spec:** `docs/superpowers/specs/2026-03-13-peer-phase-1-foundation-hardening-design.md`

---

## Chunk 1: PeerNetworkingTask Extraction (#292)

### Task 1: Create PeerNetworkingTask struct and constructor

**Files:**
- Create: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-daemon/src/lib.rs:1-4`

- [ ] **Step 1: Create `peer_networking.rs` with struct and constructor**

The constructor mirrors the peer setup currently in `DaemonServer::new()` (server.rs:82-136). It loads hosts config, creates PeerManager, registers SSH transports. The `PeerNetworkingTask` owns the peer_data channel and PeerManager, returning shared handles to the caller.

```rust
// crates/flotilla-daemon/src/peer_networking.rs
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    ConfigLabel, DaemonEvent, GoodbyeReason, HostName, PeerConnectionState, PeerDataMessage, PeerWireMessage, ProviderData,
    RepoIdentity, RoutedPeerMessage, VectorClock, PROTOCOL_VERSION,
};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::peer::{
    merge_provider_data, synthetic_repo_path, ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope,
    OverlayUpdate, PeerManager, PeerSender, SshTransport,
};

/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
struct PeerConnectedNotice {
    peer: HostName,
    generation: u64,
}

/// Manages peer networking lifecycle: SSH connections, inbound message
/// processing, and outbound snapshot broadcasting.
///
/// Created via `new()` which loads peer config and sets up transports.
/// Call `spawn()` to start the three background task groups. The returned
/// `Arc<Mutex<PeerManager>>` and `mpsc::Sender<InboundPeerEnvelope>` let
/// `DaemonServer` feed inbound socket-peer messages into the same pipeline.
pub struct PeerNetworkingTask {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
}

impl PeerNetworkingTask {
    /// Create a new peer networking task.
    ///
    /// Loads `hosts.toml` from config, creates a `PeerManager`, and registers
    /// SSH transports for each configured peer. Returns the task plus shared
    /// handles that `DaemonServer` needs for socket-peer integration.
    pub fn new(
        daemon: Arc<InProcessDaemon>,
        config: &ConfigStore,
    ) -> Result<(Self, Arc<Mutex<PeerManager>>, mpsc::Sender<InboundPeerEnvelope>), String> {
        let host_name = daemon.host_name().clone();
        let hosts_config = config.load_hosts()?;

        let peer_count = hosts_config.hosts.len();
        let mut peer_manager = PeerManager::new(host_name.clone());
        for (name, host_config) in hosts_config.hosts {
            let peer_host = HostName::new(&host_config.expected_host_name);
            if peer_host == host_name {
                warn!(
                    host = %host_name,
                    "peer config uses same name as local host — messages will be ignored"
                );
            }
            match SshTransport::new(host_name.clone(), flotilla_protocol::ConfigLabel(name.clone()), host_config) {
                Ok(transport) => {
                    peer_manager.add_peer(peer_host, Box::new(transport));
                }
                Err(e) => {
                    warn!(host = %name, err = %e, "skipping peer with invalid host name");
                }
            }
        }

        info!(host = %host_name, %peer_count, "initialized PeerNetworkingTask");

        // Emit initial disconnected status for all configured peers
        for peer_host in peer_manager.configured_peer_names() {
            daemon.send_event(DaemonEvent::PeerStatusChanged {
                host: peer_host,
                status: PeerConnectionState::Disconnected,
            });
        }

        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);
        let peer_manager = Arc::new(Mutex::new(peer_manager));

        Ok((
            Self {
                daemon,
                peer_manager: Arc::clone(&peer_manager),
                peer_data_tx: peer_data_tx.clone(),
                peer_data_rx: Some(peer_data_rx),
            },
            peer_manager,
            peer_data_tx,
        ))
    }
}
```

- [ ] **Step 2: Add module to `lib.rs`**

```rust
// crates/flotilla-daemon/src/lib.rs
pub mod cli;
pub mod peer;
pub mod peer_networking;
pub mod server;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-daemon 2>&1 | head -20`
Expected: Compiles (struct exists but `spawn()` not yet implemented).

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/peer_networking.rs crates/flotilla-daemon/src/lib.rs
git commit -m "feat(peer): add PeerNetworkingTask struct and constructor (#292)"
```

### Task 2: Move helper functions from server.rs to peer_networking.rs

**Files:**
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-daemon/src/server.rs:690-955`

Move these free functions from `server.rs` to `peer_networking.rs`:
- `rebuild_peer_overlays` (server.rs:691-740)
- `dispatch_resync_requests` (server.rs:742-763)
- `disconnect_peer_and_rebuild` (server.rs:765-826)
- `send_local_to_peers` (server.rs:838-874)
- `send_local_to_peer` (server.rs:884-936)
- `forward_until_closed` (server.rs:942-955)

These are called exclusively from the peer networking task groups. `PeerConnectedNotice` was already created in Task 1.

- [ ] **Step 1: Cut functions from server.rs, paste into peer_networking.rs**

Move each function verbatim. They keep their signatures — they're free functions that take `&Arc<Mutex<PeerManager>>` and `&Arc<InProcessDaemon>`.

- [ ] **Step 2: Update imports in peer_networking.rs**

Add the necessary imports for the moved functions (e.g. `flotilla_protocol::RoutedPeerMessage`, `crate::peer::OverlayUpdate`, etc.).

- [ ] **Step 3: Move `PeerConnectedNotice` from server.rs to peer_networking.rs**

`PeerConnectedNotice` (server.rs:50-56) is used by both the peer networking task groups and `handle_client`. Move it to `peer_networking.rs` as `pub(crate)`. Delete the original from server.rs. Add `use crate::peer_networking::PeerConnectedNotice;` to server.rs.

- [ ] **Step 4: Make moved functions `pub(crate)` in peer_networking.rs**

`disconnect_peer_and_rebuild` is called from `handle_client` in server.rs (line 1180), so it needs `pub(crate)` visibility.

- [ ] **Step 5: Update server.rs to call the moved functions via `crate::peer_networking::`**

Replace calls in server.rs (lines 266, 325, 537, 824, 1180) to use `crate::peer_networking::disconnect_peer_and_rebuild`, etc. Also update the test at line 1879 which calls `disconnect_peer_and_rebuild` — add `use crate::peer_networking::disconnect_peer_and_rebuild;` to the test module.

- [ ] **Step 6: Verify it compiles and tests pass**

Run: `cargo test -p flotilla-daemon 2>&1 | tail -5`
Expected: All tests pass — behavior unchanged, just code moved.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-daemon/src/peer_networking.rs crates/flotilla-daemon/src/server.rs
git commit -m "refactor(peer): move helper functions to peer_networking module (#292)"
```

### Task 3: Implement `spawn()` — move the three task groups

**Files:**
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-daemon/src/server.rs:215-624`

**NOTE: Steps 1-6 in this task are atomic — the code will not compile until all six are complete. Do not attempt to build between steps.**

- [ ] **Step 1: Add `spawn()` method to PeerNetworkingTask**

Move the three task groups from `DaemonServer::run()` (lines 215-624) into `PeerNetworkingTask::spawn(self) -> tokio::task::JoinHandle<()>`. This is largely a cut-and-paste with s/peer_manager/self.peer_manager/, s/daemon/self.daemon/, etc.

The `spawn()` method:
1. Takes `self` (consumes the task)
2. Takes `peer_data_rx` from `self.peer_data_rx` via `.take().expect()`
3. Creates `(peer_connected_tx, peer_connected_rx)` unbounded channel
4. Spawns three task groups inside a single `tokio::spawn`
5. Returns the `JoinHandle`

```rust
impl PeerNetworkingTask {
    // ... (constructor from Task 1)

    /// Start the peer networking background tasks.
    ///
    /// Spawns three concurrent task groups:
    /// 1. Per-peer SSH connection loops with reconnect
    /// 2. Inbound message processor (relay + handle + overlay updates)
    /// 3. Outbound snapshot broadcaster
    ///
    /// Consumes `self` — call only once.
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        let peer_data_rx = self.peer_data_rx.take().expect("spawn() called twice");
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        // ... (the three task groups, moved from server.rs lines 222-624)
        // Replace variable names:
        //   peer_manager_task -> self.peer_manager
        //   peer_daemon -> self.daemon
        //   peer_data_tx_for_ssh -> self.peer_data_tx
        //   outbound_peer_manager -> Arc::clone(&self.peer_manager)
        // etc.
        todo!()
    }
}
```

- [ ] **Step 2: Move the SSH connection loop (server.rs:222-345)**

This is the first task group inside `spawn()`. Copy it verbatim, updating variable references.

- [ ] **Step 3: Move the inbound processor (server.rs:347-543)**

Second task group. The `loop { tokio::select! { ... } }` that processes `peer_data_rx`.

- [ ] **Step 4: Move the outbound broadcaster (server.rs:545-624)**

Third task group. The outbound snapshot broadcasting task.

- [ ] **Step 5: Remove the three task groups from `DaemonServer::run()`**

Replace lines 215-624 of server.rs with a call to create and spawn the `PeerNetworkingTask`. The new `run()` flow becomes:

```rust
// In DaemonServer::run(), replacing lines 215-624:

// Create and spawn peer networking
let (peer_networking, peer_manager, _peer_data_tx) =
    PeerNetworkingTask::new(Arc::clone(&daemon), &config)?;
// DaemonServer already has peer_data_tx and peer_manager from construction,
// but now they come from PeerNetworkingTask.
let _peer_handle = peer_networking.spawn();
```

Wait — `DaemonServer` currently constructs peer_manager and peer_data channel in its `new()` method. We need to change `DaemonServer::new()` to delegate to `PeerNetworkingTask::new()`.

- [ ] **Step 6: Refactor DaemonServer::new() to use PeerNetworkingTask::new()**

`DaemonServer::new()` currently:
1. Loads daemon config, creates host name
2. Loads hosts config, creates PeerManager, registers SSH transports
3. Creates InProcessDaemon
4. Creates peer_data channel
5. Stores peer_manager and peer_data_tx/rx in DaemonServer fields

Refactor so that:
1. `DaemonServer::new()` loads daemon config, creates InProcessDaemon (same)
2. `DaemonServer` stores a `PeerNetworkingTask` (pre-spawn) instead of raw peer_manager/peer_data fields
3. `DaemonServer::run()` calls `peer_networking.spawn()` and uses the returned `peer_manager` and `peer_data_tx`

Actually, simpler: `DaemonServer` fields change to store the outputs of `PeerNetworkingTask::new()`:

```rust
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    follower: bool,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_networking: Option<PeerNetworkingTask>,
}
```

In `new()`, replace the peer setup block with:
```rust
let (peer_networking, peer_manager, peer_data_tx) =
    PeerNetworkingTask::new(Arc::clone(&daemon), &config)?;
```

In `run()`, take and spawn:
```rust
let peer_networking = self.peer_networking.take().expect("run() called twice");
let _peer_handle = peer_networking.spawn();
```

Remove `peer_data_rx` field (it's inside `PeerNetworkingTask`). Remove `take_peer_data_rx()`.

Note: `SocketPeerSender` stays in `server.rs` — it is used by `handle_client` for inbound socket peers and is NOT part of the extraction.

- [ ] **Step 7: Verify it compiles and tests pass**

Run: `cargo test -p flotilla-daemon 2>&1 | tail -5`

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-daemon/src/peer_networking.rs crates/flotilla-daemon/src/server.rs
git commit -m "refactor(peer): extract PeerNetworkingTask::spawn() from DaemonServer::run() (#292)"
```

### Task 4: Enable peer networking in embedded mode

**Files:**
- Modify: `src/main.rs:79-160`

- [ ] **Step 1: Add embedded mode peer networking**

In `run_tui()`, when `embedded` is true, load daemon config for host name before creating InProcessDaemon, then spawn PeerNetworkingTask:

```rust
// In run_tui(), replace the daemon creation block (lines 113-123):
let config_clone = Arc::clone(&config);
let daemon_task = tokio::spawn(async move {
    let daemon: Result<Arc<dyn DaemonHandle>, String> = if embedded {
        // Load daemon config for host name (peer identity)
        let daemon_config = config_clone.load_daemon_config();
        let host_name = daemon_config
            .host_name
            .map(HostName::new)
            .unwrap_or_else(HostName::local);
        let d = InProcessDaemon::new_with_options(
            repo_roots,
            Arc::clone(&config_clone),
            daemon_config.follower,
            host_name,
        )
        .await;

        // Spawn peer networking if peers are configured
        match flotilla_daemon::peer_networking::PeerNetworkingTask::new(
            Arc::clone(&d),
            &config_clone,
        ) {
            Ok((peer_networking, _peer_manager, _peer_data_tx)) => {
                peer_networking.spawn();
            }
            Err(e) => {
                // No peers configured or config error — embedded mode
                // works fine without peer networking
                tracing::warn!(err = %e, "peer networking not started in embedded mode");
            }
        }

        Ok(d as Arc<dyn DaemonHandle>)
    } else {
        // ... socket mode unchanged
    };
    daemon
});
```

- [ ] **Step 2: Add necessary imports to main.rs**

```rust
use flotilla_protocol::HostName;
```

`flotilla_daemon` is already a dependency of the root crate (check `Cargo.toml`).

- [ ] **Step 3: Verify it compiles**

Run: `cargo build 2>&1 | tail -5`

- [ ] **Step 4: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -10`
Expected: All tests pass. No behavioral changes — same code, reorganized.

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Run: `cargo +nightly fmt`

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat(peer): enable peer networking in embedded mode (#292)"
```

---

## Chunk 2: Protocol Version Handshake + Session ID (#259)

### Task 5: Add uuid dependency and session_id to InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`
- Modify: `crates/flotilla-core/src/in_process.rs:270-320`

- [ ] **Step 1: Add uuid dependency**

Add to `crates/flotilla-core/Cargo.toml` under `[dependencies]`:
```toml
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: Add session_id field to InProcessDaemon**

In `in_process.rs`, add `session_id: uuid::Uuid` to the struct fields (around line 298). Initialize it in `new_with_options()` with `uuid::Uuid::new_v4()`.

Add a public accessor:
```rust
pub fn session_id(&self) -> uuid::Uuid {
    self.session_id
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/Cargo.toml crates/flotilla-core/src/in_process.rs
git commit -m "feat(core): add session_id to InProcessDaemon (#259)"
```

### Task 6: Add session_id to Hello message and bump PROTOCOL_VERSION

**Files:**
- Modify: `crates/flotilla-protocol/Cargo.toml`
- Modify: `crates/flotilla-protocol/src/lib.rs:46,71,154-160`

- [ ] **Step 1: Add uuid dependency to flotilla-protocol**

```toml
uuid = { version = "1", features = ["v4", "serde"] }
```

- [ ] **Step 2: Bump PROTOCOL_VERSION**

```rust
pub const PROTOCOL_VERSION: u32 = 2;
```

- [ ] **Step 3: Add session_id to Hello variant**

```rust
#[serde(rename = "hello")]
Hello {
    protocol_version: u32,
    host_name: HostName,
    #[serde(default = "uuid::Uuid::nil")]
    session_id: uuid::Uuid,
},
```

The `#[serde(default)]` allows deserializing old Hello messages without session_id during the transition (they'll get nil UUID). This is for robustness, not backwards compat.

- [ ] **Step 4: Add Rejected variant to PeerConnectionState**

Remove `Copy` derive, add `Rejected`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerConnectionState {
    Connected,
    Disconnected,
    Connecting,
    Reconnecting,
    Rejected { reason: String },
}
```

- [ ] **Step 5: Fix all compile errors from removing Copy and adding Rejected variant**

Adding `Rejected { reason: String }` removes `Copy` (String is not Copy) and breaks exhaustive matches. Files that need updating:

- `crates/flotilla-tui/src/cli.rs` — match on PeerConnectionState (lines 95-99, test at 361-364). Add `Rejected` arm.
- `crates/flotilla-tui/src/app/mod.rs` — `From<PeerConnectionState>` impl (lines 49-53) and `PeerStatus` enum which derives `Copy`. `PeerStatus` needs a `Rejected` variant or map `Rejected` → `Disconnected`. If `PeerStatus` stores a String, it also loses `Copy`.
- `crates/flotilla-core/src/in_process.rs` — uses `*status` (lines 976, 1305) which requires `Copy`. Change to `status.clone()`.
- `crates/flotilla-tui/src/ui.rs` — peer status rendering. Add `Rejected` arm.

Search for `PeerConnectionState` across the workspace: `cargo build --workspace 2>&1` and fix all errors.

- [ ] **Step 6: Fix Hello construction sites**

Search for `Message::Hello {` across the codebase. Each needs `session_id` added:
- `ssh_transport.rs:188-191` — SshTransport needs a `local_session_id: uuid::Uuid` field set during construction. `PeerNetworkingTask::new()` passes `daemon.session_id()` to `SshTransport::new()` (add parameter). Use this field when constructing the Hello message.
- `server.rs:1087` — inbound socket Hello response. Use `daemon.session_id()`.
- Test sites: `server.rs:1543,1693,1772`, `flotilla-protocol/src/framing.rs:25`, `flotilla-protocol/src/lib.rs:415` — add `session_id: uuid::Uuid::nil()` for test Hello messages.

- [ ] **Step 7: Fix Hello match/destructure sites**

Search for `Message::Hello {` pattern matches. Each needs to destructure `session_id`:
- `ssh_transport.rs:320-332` (`validate_remote_hello`) — extract and return session_id (changed in Task 7)
- `server.rs:1076` — extract session_id from inbound peer Hello
- Any test assertion matches

- [ ] **Step 9: Verify it compiles and tests pass**

Run: `cargo test --workspace 2>&1 | tail -10`

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat(protocol): add session_id to Hello, bump PROTOCOL_VERSION to 2 (#259)"
```

### Task 7: Return session_id from handshake and store in ActiveConnection

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/ssh_transport.rs:183,320-333`
- Modify: `crates/flotilla-daemon/src/peer/transport.rs`
- Modify: `crates/flotilla-daemon/src/peer/channel_transport.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:60-63,396-422,673-731`
- Modify: `crates/flotilla-daemon/src/server.rs:1076-1134`

- [ ] **Step 1: Change `validate_remote_hello` to return session_id**

```rust
fn validate_remote_hello(expected_host_name: &HostName, hello: Message) -> Result<uuid::Uuid, String> {
    match hello {
        Message::Hello { protocol_version, host_name, session_id } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(format!("peer protocol version mismatch: expected {}, got {}", PROTOCOL_VERSION, protocol_version));
            }
            if host_name != *expected_host_name {
                return Err(format!("peer host mismatch: expected {}, got {}", expected_host_name, host_name));
            }
            Ok(session_id)
        }
        other => Err(format!("expected peer hello, got {:?}", other)),
    }
}
```

- [ ] **Step 2: Store session_id in SshTransport, return from connect_socket**

Add `remote_session_id: Option<uuid::Uuid>` field to `SshTransport`. `connect_socket` stores the validated session_id and also stores it in the field:

```rust
async fn connect_socket(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
    // ... existing code ...
    let remote_session_id = Self::validate_remote_hello(&self.expected_host_name, hello)?;
    self.remote_session_id = Some(remote_session_id);
    // ... rest unchanged ...
}
```

Add accessor: `pub fn remote_session_id(&self) -> Option<uuid::Uuid> { self.remote_session_id }`

- [ ] **Step 3: Add session_id to ActiveConnection**

```rust
struct ActiveConnection {
    generation: u64,
    meta: ConnectionMeta,
    session_id: Option<uuid::Uuid>,
}
```

Update `activate_connection` to accept optional session_id:

```rust
pub fn activate_connection(
    &mut self,
    host: HostName,
    sender: Arc<dyn PeerSender>,
    meta: ConnectionMeta,
    session_id: Option<uuid::Uuid>,
) -> ActivationResult {
    // ... existing logic ...
    self.active_connections.insert(host.clone(), ActiveConnection { generation, meta: meta.clone(), session_id });
    // ... rest unchanged ...
}
```

Add accessor:
```rust
pub fn peer_session_id(&self, host: &HostName) -> Option<uuid::Uuid> {
    self.active_connections.get(host).and_then(|c| c.session_id)
}
```

- [ ] **Step 4: Update all `activate_connection` call sites**

The new `session_id` parameter must be passed everywhere `activate_connection` is called:
- `connect_all()` in manager.rs:695 — get from transport: `transport.remote_session_id()`
- `reconnect_peer()` in manager.rs:867 — same: `transport.remote_session_id()`
- `server.rs:1106` (socket inbound Hello) — extract from the Hello message
- `test_support.rs:20` (`ensure_test_connection_generation`) — pass `None`

- [ ] **Step 5: Add `remote_session_id()` to PeerTransport trait**

```rust
// transport.rs
#[async_trait]
pub trait PeerTransport: Send + Sync {
    // ... existing methods ...

    /// Return the session ID received during the last handshake, if any.
    fn remote_session_id(&self) -> Option<uuid::Uuid> {
        None
    }
}
```

Implement in `SshTransport`:
```rust
fn remote_session_id(&self) -> Option<uuid::Uuid> {
    self.remote_session_id
}
```

`ChannelTransport` uses the default (returns `None`). Test code can set a fixed UUID if needed.

- [ ] **Step 6: Emit Rejected status on version mismatch**

In `server.rs` handle_client Hello branch, on version mismatch emit `PeerConnectionState::Rejected` instead of just returning:

```rust
Message::Hello { protocol_version, host_name, session_id } => {
    if protocol_version != PROTOCOL_VERSION {
        warn!(
            peer = %host_name,
            expected = PROTOCOL_VERSION,
            got = protocol_version,
            "peer protocol version mismatch"
        );
        daemon.send_event(DaemonEvent::PeerStatusChanged {
            host: host_name,
            status: PeerConnectionState::Rejected {
                reason: format!("protocol mismatch (local={}, remote={})", PROTOCOL_VERSION, protocol_version),
            },
        });
        return;
    }
    // ... rest of Hello handling, now also destructures session_id ...
}
```

- [ ] **Step 7: Verify it compiles and tests pass**

Run: `cargo test --workspace 2>&1 | tail -10`

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Run: `cargo +nightly fmt`

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(peer): store session_id from handshake, emit Rejected on mismatch (#259)"
```

### Task 8: Add TUI rendering for Rejected state

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs` (peer status rendering)

- [ ] **Step 1: Find peer status rendering code**

Search for `PeerConnectionState` in `ui.rs` to find where peer status is rendered.

- [ ] **Step 2: Add Rejected variant rendering**

Add a match arm for `Rejected { reason }` — display in red with the reason text.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs
git commit -m "feat(ui): render Rejected peer connection state (#259)"
```

---

## Chunk 3: Head-of-Line Blocking Fix (#264)

### Task 9: Add `prepare_relay()` to PeerManager

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:617-656`

- [ ] **Step 1: Write test for prepare_relay**

Add a test in `manager.rs` (or `channel_tests.rs`) that verifies `prepare_relay()` returns the correct targets:

```rust
#[tokio::test]
async fn prepare_relay_excludes_origin_and_self_and_already_seen() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("alpha");
    let b = net.add_peer("beta");
    let c = net.add_peer("charlie");
    // alpha connected to beta and charlie
    net.connect(a, b);
    net.connect(a, c);
    net.start().await;

    // Message from beta — should relay to charlie only (not back to beta, not to self)
    let msg = PeerDataMessage {
        origin_host: HostName::new("beta"),
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo_path: PathBuf::from("/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 1 },
    };
    let targets = net.manager(a).prepare_relay(&HostName::new("beta"), &msg);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].0, HostName::new("charlie"));
    // Clock should have alpha's stamp
    assert!(targets[0].2.clock.get(&HostName::new("alpha")) > 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-daemon prepare_relay 2>&1 | tail -10`
Expected: FAIL — `prepare_relay` doesn't exist yet.

- [ ] **Step 3: Implement `prepare_relay()` on PeerManager**

Add to manager.rs, alongside the existing `relay()`:

```rust
/// Snapshot relay targets without performing I/O.
///
/// Returns `(peer_name, sender, stamped_message)` tuples for each peer
/// that should receive the relayed message. The caller sends concurrently
/// outside the PeerManager lock.
pub fn prepare_relay(
    &self,
    origin: &HostName,
    msg: &PeerDataMessage,
) -> Vec<(HostName, Arc<dyn PeerSender>, PeerDataMessage)> {
    let mut relayed_msg = msg.clone();
    relayed_msg.clock.tick(&self.local_host);

    self.senders
        .iter()
        .filter(|(name, _)| *name != origin && *name != &self.local_host && msg.clock.get(name) == 0)
        .map(|(name, sender)| (name.clone(), Arc::clone(sender), relayed_msg.clone()))
        .collect()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-daemon prepare_relay 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs
git commit -m "feat(peer): add prepare_relay() for lock-free concurrent sends (#264)"
```

### Task 10: Switch inbound processor to use prepare_relay + concurrent sends

**Files:**
- Modify: `crates/flotilla-daemon/src/peer_networking.rs` (inbound processor)
- Modify: `crates/flotilla-daemon/Cargo.toml`

- [ ] **Step 1: Add futures dependency**

Add to `crates/flotilla-daemon/Cargo.toml`:
```toml
futures = "0.3"
```

- [ ] **Step 2: Replace relay() call in inbound processor with prepare_relay + concurrent sends**

In `peer_networking.rs`, in the inbound processor task, replace:
```rust
// Old:
let mut pm = peer_manager_task.lock().await;
if let PeerWireMessage::Data(msg) = &env.msg {
    pm.relay(&origin, msg).await;
}
let result = pm.handle_inbound(env).await;
```

With:
```rust
// New: prepare relay targets under lock, send outside lock
let relay_targets = if let PeerWireMessage::Data(msg) = &env.msg {
    let pm = peer_manager_task.lock().await;
    pm.prepare_relay(&origin, msg)
} else {
    vec![]
};

// Send concurrently outside lock with per-peer timeout
if !relay_targets.is_empty() {
    let sends = relay_targets.into_iter().map(|(name, sender, msg)| async move {
        match tokio::time::timeout(Duration::from_secs(5), sender.send(PeerWireMessage::Data(msg))).await {
            Ok(Ok(())) => {
                debug!(from = %origin, to = %name, "relayed peer data");
            }
            Ok(Err(e)) => {
                warn!(to = %name, err = %e, "relay send failed");
            }
            Err(_) => {
                warn!(to = %name, "relay send timed out (5s)");
            }
        }
    });
    futures::future::join_all(sends).await;
}

// Re-acquire lock for handle_inbound
let mut pm = peer_manager_task.lock().await;
let result = pm.handle_inbound(env).await;
```

- [ ] **Step 3: Remove old `relay()` method from PeerManager**

Delete the `relay()` method from `manager.rs:617-656`.

- [ ] **Step 4: Update TestNetwork to use prepare_relay**

In `test_support.rs`, update `process_peer()` and `inject_local_data()`:

```rust
pub async fn inject_local_data(&mut self, peer_idx: usize, msg: PeerDataMessage) {
    let peer = &self.peers[peer_idx];
    let targets = peer.manager.prepare_relay(&peer.name, &msg);
    for (_, sender, relayed_msg) in targets {
        let _ = sender.send(PeerWireMessage::Data(relayed_msg)).await;
    }
}

pub async fn process_peer(&mut self, peer_idx: usize) -> usize {
    // ... collect messages same as before ...

    for (connection_peer, generation, msg) in messages {
        if let PeerWireMessage::Data(ref data_msg) = msg {
            let targets = peer.manager.prepare_relay(&data_msg.origin_host, data_msg);
            for (_, sender, relayed_msg) in targets {
                let _ = sender.send(PeerWireMessage::Data(relayed_msg)).await;
            }
        }

        let env = InboundPeerEnvelope { msg, connection_generation: generation, connection_peer };
        peer.manager.handle_inbound(env).await;
    }

    count
}
```

- [ ] **Step 5: Verify all tests pass**

Run: `cargo test --workspace 2>&1 | tail -10`

- [ ] **Step 6: Run clippy and fmt**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Run: `cargo +nightly fmt`

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "fix(peer): eliminate head-of-line blocking in relay with concurrent sends (#264)"
```

---

## Chunk 4: Restart Detection + Keepalive (#258)

### Task 11: Add Ping/Pong to PeerWireMessage

**Files:**
- Modify: `crates/flotilla-protocol/src/peer.rs:67-71`

- [ ] **Step 1: Write roundtrip test for Ping/Pong**

```rust
#[test]
fn ping_pong_roundtrip() {
    let ping = PeerWireMessage::Ping { timestamp: 1234567890 };
    let json = serde_json::to_string(&ping).expect("serialize ping");
    let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize ping");
    match decoded {
        PeerWireMessage::Ping { timestamp } => assert_eq!(timestamp, 1234567890),
        other => panic!("expected Ping, got {:?}", other),
    }

    let pong = PeerWireMessage::Pong { timestamp: 1234567890 };
    let json = serde_json::to_string(&pong).expect("serialize pong");
    let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize pong");
    match decoded {
        PeerWireMessage::Pong { timestamp } => assert_eq!(timestamp, 1234567890),
        other => panic!("expected Pong, got {:?}", other),
    }
}
```

- [ ] **Step 2: Add Ping/Pong variants**

```rust
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
    Goodbye { reason: GoodbyeReason },
    Ping { timestamp: u64 },
    Pong { timestamp: u64 },
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-protocol ping_pong 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 4: Fix exhaustive match errors across the workspace**

Search for `PeerWireMessage` matches. Add `Ping`/`Pong` arms where needed:
- `peer_networking.rs` inbound processor — the origin/repo_path extraction match (moved from server.rs:357-371) needs `Ping`/`Pong` arms that extract `(env.connection_peer.clone(), PathBuf::new())` (same as Goodbye). Also add a guard to skip relay for Ping/Pong (they are not `PeerWireMessage::Data`).
- `server.rs` handle_client peer message loop — already passes all `Message::Peer(*)` through, no match on variants
- `manager.rs` handle_inbound — add `HandleResult::Ignored` return for Ping/Pong in the match

- [ ] **Step 5: Verify it compiles**

Run: `cargo build --workspace 2>&1 | tail -5`

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/peer.rs
git commit -m "feat(protocol): add Ping/Pong wire messages for keepalive (#258)"
```

### Task 12: Handle Ping/Pong in SSH transport reader

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/ssh_transport.rs:183-278`

- [ ] **Step 1: Give reader task access to outbound sender for Pong replies**

In `connect_socket()`, the reader task currently only has `inbound_tx`. Pass a clone of `outbound_tx` (the channel to the writer task) so it can send Pong responses:

```rust
// After creating outbound channel:
let pong_tx = self.outbound_tx.as_ref().cloned();

// Reader task gains pong_tx:
let reader_handle = tokio::spawn(async move {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let msg: Message = match serde_json::from_str(&line) { ... };
                match msg {
                    Message::Peer(peer_msg) => {
                        match *peer_msg {
                            PeerWireMessage::Ping { timestamp } => {
                                // Reply with Pong directly via writer channel
                                if let Some(ref tx) = pong_tx {
                                    let _ = tx.send(PeerWireMessage::Pong { timestamp }).await;
                                }
                            }
                            PeerWireMessage::Pong { timestamp } => {
                                // Forward to inbound — the forwarding task
                                // updates last_message_at on any recv
                                if inbound_tx.send(PeerWireMessage::Pong { timestamp }).await.is_err() {
                                    break;
                                }
                            }
                            other => {
                                if inbound_tx.send(other).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    _ => { /* ignore non-peer messages */ }
                }
            }
            // ... EOF/error handling unchanged ...
        }
    }
});
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-daemon 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-daemon/src/peer/ssh_transport.rs
git commit -m "feat(peer): handle Ping/Pong in SSH transport reader (#258)"
```

### Task 13: Add keepalive ping sender and liveness tracking

**Files:**
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`

- [ ] **Step 1: Add keepalive ping to per-peer SSH connection loops**

In the per-peer reconnect task inside `spawn()`, after `forward_until_closed`, add a ping interval. Actually, `forward_until_closed` blocks until the connection drops, so pings need to be sent concurrently.

Restructure the forwarding to race against a ping timer:

```rust
// Replace forward_until_closed with a keepalive-aware version
async fn forward_with_keepalive(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
    sender: Arc<dyn PeerSender>,
) -> ForwardResult {
    const PING_INTERVAL: Duration = Duration::from_secs(30);
    const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(90);

    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_message_at = Instant::now();

    loop {
        tokio::select! {
            msg = inbound_rx.recv() => {
                match msg {
                    Some(peer_msg) => {
                        last_message_at = Instant::now();
                        // Skip forwarding Pong messages to the inbound processor
                        if matches!(&peer_msg, PeerWireMessage::Pong { .. }) {
                            continue;
                        }
                        if let Err(e) = tx.send(InboundPeerEnvelope {
                            msg: peer_msg,
                            connection_generation: generation,
                            connection_peer: peer_name.clone(),
                        }).await {
                            warn!(peer = %peer_name, err = %e, "forwarding channel closed");
                            return ForwardResult::Shutdown;
                        }
                    }
                    None => {
                        return ForwardResult::Disconnected;
                    }
                }
            }
            _ = ping_interval.tick() => {
                // Check liveness
                if last_message_at.elapsed() > KEEPALIVE_TIMEOUT {
                    warn!(
                        peer = %peer_name,
                        elapsed_secs = last_message_at.elapsed().as_secs(),
                        "keepalive timeout — no messages received in 90s"
                    );
                    return ForwardResult::KeepaliveTimeout;
                }
                // Send ping
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if let Err(e) = sender.send(PeerWireMessage::Ping { timestamp }).await {
                    debug!(peer = %peer_name, err = %e, "failed to send keepalive ping");
                }
            }
        }
    }
}

enum ForwardResult {
    Disconnected,  // peer connection dropped (EOF)
    Shutdown,      // main channel closed (daemon shutting down)
    KeepaliveTimeout, // no messages received within timeout
}
```

- [ ] **Step 2: Update the per-peer task to use `forward_with_keepalive`**

Replace `forward_until_closed` calls in the per-peer SSH loop with `forward_with_keepalive`. The sender is obtained from `PeerManager` after connect:

```rust
// After successful connect/reconnect:
let sender = {
    let pm = pm.lock().await;
    pm.get_sender_if_current(&peer_name, generation)
};
let Some(sender) = sender else { continue; };

match forward_with_keepalive(&tx, &mut inbound_rx, &peer_name, generation, sender).await {
    ForwardResult::Shutdown => return,
    ForwardResult::Disconnected => {
        info!(peer = %peer_name, "SSH connection dropped, will reconnect");
    }
    ForwardResult::KeepaliveTimeout => {
        info!(peer = %peer_name, "keepalive timeout, forcing reconnect");
        // Reset backoff since this is a fresh detection
        attempt = 1;
    }
}
// disconnect + rebuild as before
```

- [ ] **Step 3: Remove old `forward_until_closed` function**

It's replaced by `forward_with_keepalive`.

- [ ] **Step 4: Verify it compiles and tests pass**

Run: `cargo test --workspace 2>&1 | tail -10`

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer_networking.rs
git commit -m "feat(peer): add keepalive ping/pong with 90s timeout (#258)"
```

### Task 14: Session ID comparison on reconnect

**Files:**
- Modify: `crates/flotilla-daemon/src/peer_networking.rs`
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Verify `peer_session_id` accessor exists on PeerManager**

Already done in Task 7, step 3. Verify it exists.

- [ ] **Step 2: Add `transport_remote_session_id` to PeerManager**

```rust
pub fn transport_remote_session_id(&self, name: &HostName) -> Option<uuid::Uuid> {
    self.peers.get(name).and_then(|t| t.remote_session_id())
}
```

- [ ] **Step 3: Track previous session_id and clear stale data on change**

The per-peer reconnect task tracks `last_known_session_id` in local state. After reconnect, compare old vs new. **Important**: `reconnect_peer()` does NOT call `disconnect_peer()` — it calls `transport.disconnect()` directly, which does NOT clear stale peer data. When the session ID changes (remote daemon restarted), we must explicitly call `disconnect_peer_and_rebuild` to clear stale data before the new connection is used.

```rust
// Per-peer task local state:
let mut last_known_session_id: Option<uuid::Uuid> = None;

// After initial connect succeeds:
last_known_session_id = {
    let pm = pm.lock().await;
    pm.peer_session_id(&peer_name)
};

// After reconnect succeeds (inside the reconnect match arm):
let current_session_id = {
    let pm = pm.lock().await;
    pm.peer_session_id(&peer_name)
};

if let (Some(prev), Some(curr)) = (last_known_session_id, current_session_id) {
    if prev != curr {
        info!(
            peer = %peer_name,
            "remote daemon restarted (session_id changed), clearing stale data"
        );
        // reconnect_peer only does transport.disconnect() + reconnect,
        // which does NOT clear stale peer data. We must explicitly
        // clear it so the UI doesn't show stale checkouts/branches.
        let plan = disconnect_peer_and_rebuild(
            &pm, &daemon_for_cleanup, &peer_name, generation,
        ).await;
        // Data is now cleared. The new connection is already active
        // (reconnect_peer re-activated it), and the PeerConnectedNotice
        // will trigger sending current local state to the peer.
    }
}
last_known_session_id = current_session_id;
```

- [ ] **Step 4: Verify it compiles and tests pass**

Run: `cargo test --workspace 2>&1 | tail -10`

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Run: `cargo +nightly fmt`

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(peer): detect remote daemon restart via session_id comparison (#258)"
```

### Task 15: Final verification and cleanup

**Files:** All modified files

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: All tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: No warnings.

- [ ] **Step 3: Run fmt**

Run: `cargo +nightly fmt --check 2>&1 | tail -5`
Expected: No formatting issues.

- [ ] **Step 4: Verify build**

Run: `cargo build --workspace 2>&1 | tail -5`
Expected: Clean build.
