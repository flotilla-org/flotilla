# Peer Connect/Reconnect Bug Fixes (#290, #263) Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two peer connection lifecycle bugs — send local state on connect/reconnect (#290) and eliminate the cleanup race on rapid disconnect/reconnect (#263).

**Architecture:** A notification channel carries `PeerConnectedNotice` from connection sites to the outbound task, which sends local state to the specific peer. For the race fix, `disconnect_peer` computes overlay updates atomically under its `&mut self` borrow, eliminating the lock gap that allowed stale writes.

**Tech Stack:** Rust, tokio (async runtime, mpsc channels, Mutex, RwLock), async-trait

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/flotilla-daemon/src/peer/manager.rs` | Modify | Add `OverlayUpdate` enum, extend `DisconnectPlan`, update `disconnect_peer` to compute overlays atomically, add `get_sender_if_current` |
| `crates/flotilla-daemon/src/peer/mod.rs` | Modify | Re-export `OverlayUpdate` |
| `crates/flotilla-daemon/src/server.rs` | Modify | Add `PeerConnectedNotice`, notification channel plumbing, `send_local_to_peer` helper, update `disconnect_peer_and_rebuild` to apply pre-computed overlays |
| `crates/flotilla-core/src/in_process.rs` | Modify | Add `tracked_repo_paths()` and `repo_identity_snapshot()` methods |

## Chunk 1: Atomic disconnect + overlay (#263)

### Task 1: Add `OverlayUpdate` enum and extend `DisconnectPlan`

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:112-117`

- [ ] **Step 1: Add `OverlayUpdate` enum and extend `DisconnectPlan`**

In `crates/flotilla-daemon/src/peer/manager.rs`, add `OverlayUpdate` above `DisconnectPlan` and add the new field:

```rust
/// Pre-computed overlay update to apply to InProcessDaemon after releasing the PeerManager lock.
#[derive(Debug, Clone)]
pub enum OverlayUpdate {
    /// Update peer_providers for a repo with remaining peer data.
    SetProviders { path: PathBuf, peers: Vec<(HostName, ProviderData)> },
    /// Remove a virtual repo — no peers remain.
    RemoveRepo { identity: RepoIdentity, path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct DisconnectPlan {
    pub was_active: bool,
    pub affected_repos: Vec<RepoIdentity>,
    pub resync_requests: Vec<RoutedPeerMessage>,
    /// Pre-computed overlay state for each affected repo, captured atomically
    /// with the disconnect under the same lock.
    pub overlay_updates: Vec<OverlayUpdate>,
}
```

Update the two early-return sites in `disconnect_peer` (line 869) to include `overlay_updates: Vec::new()`, and update the final return at line 955.

Also add `OverlayUpdate` to the re-export list in `crates/flotilla-daemon/src/peer/mod.rs`:

```rust
pub use manager::{
    synthetic_repo_path, ActivationResult, ConnectionDirection, ConnectionMeta, DisconnectPlan, HandleResult, InboundPeerEnvelope,
    OverlayUpdate, PeerManager, PendingResyncRequest, PerRepoPeerState, ReversePathHop, ReversePathKey, RouteHop, RouteState,
};
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-daemon 2>&1 | head -30`
Expected: Compiles (possibly with unused-field warnings, which is fine at this stage).

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs
git commit -m "refactor: add OverlayUpdate enum and extend DisconnectPlan"
```

### Task 2: Update `disconnect_peer` to compute overlay updates atomically

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:867-956` (`disconnect_peer`)

This is the core #263 fix. `disconnect_peer` currently modifies `peer_data` (removing or marking stale) and returns `affected_repos`. The caller then re-acquires the lock in `rebuild_peer_overlays` to read `peer_data` and compute overlays — that's the race window. Instead, compute overlays here while still holding `&mut self`.

The new parameter `local_repo_paths: &HashMap<RepoIdentity, PathBuf>` provides the identity → local path mapping (obtained from `InProcessDaemon.repo_identities` by the caller before acquiring the PeerManager lock). For remote-only repos, `self.known_remote_repos` provides the synthetic path.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `crates/flotilla-daemon/src/peer/manager.rs`:

```rust
#[tokio::test]
async fn disconnect_peer_returns_overlay_updates_for_remaining_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    // Two peers ("desktop" and "laptop") both have data for the same repo
    handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), || {
        Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
    })
    .await;
    handle_test_peer_data(&mut mgr, snapshot_msg("laptop", 1), || {
        Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
    })
    .await;

    let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

    // Build local_repo_paths mapping as caller would
    let mut local_repo_paths = HashMap::new();
    local_repo_paths.insert(test_repo(), PathBuf::from("/local/repo"));

    let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation, &local_repo_paths);

    assert!(plan.was_active);
    assert_eq!(plan.overlay_updates.len(), 1);
    match &plan.overlay_updates[0] {
        OverlayUpdate::SetProviders { path, peers } => {
            assert_eq!(path, &PathBuf::from("/local/repo"));
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].0, HostName::new("laptop"));
        }
        other => panic!("expected SetProviders, got {:?}", other),
    }
}

#[tokio::test]
async fn disconnect_peer_returns_remove_repo_for_remote_only_with_no_remaining_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), || {
        Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
    })
    .await;

    let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

    // Register as remote-only (no local repo)
    let synthetic_path = PathBuf::from("/virtual/github.com/owner/repo");
    mgr.register_remote_repo(test_repo(), synthetic_path.clone());

    // No local repo path mapping — this repo is remote-only
    let local_repo_paths = HashMap::new();

    let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation, &local_repo_paths);

    assert!(plan.was_active);
    assert_eq!(plan.overlay_updates.len(), 1);
    match &plan.overlay_updates[0] {
        OverlayUpdate::RemoveRepo { identity, path } => {
            assert_eq!(identity, &test_repo());
            assert_eq!(path, &synthetic_path);
        }
        other => panic!("expected RemoveRepo, got {:?}", other),
    }
    // known_remote_repos should be cleaned up
    assert!(!mgr.is_remote_repo(&test_repo()));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-daemon disconnect_peer_returns 2>&1 | tail -20`
Expected: Compilation error — `disconnect_peer` doesn't accept `local_repo_paths` yet.

- [ ] **Step 3: Update `disconnect_peer` signature and implementation**

Change `disconnect_peer` to accept the new parameter and compute overlay updates. The logic mirrors what `rebuild_peer_overlays` currently does in `server.rs:654-703`, but computed inline:

```rust
pub fn disconnect_peer(
    &mut self,
    name: &HostName,
    generation: u64,
    local_repo_paths: &HashMap<RepoIdentity, PathBuf>,
) -> DisconnectPlan {
    if !self.generation_is_current(name, generation) {
        return DisconnectPlan {
            was_active: false,
            affected_repos: Vec::new(),
            resync_requests: Vec::new(),
            overlay_updates: Vec::new(),
        };
    }

    self.senders.remove(name);
    self.active_connections.remove(name);
    self.generations.remove(name);
    self.displaced_senders.retain(|(host, _), _| host != name);
    self.reverse_paths.retain(|_, hop| hop.next_hop != *name);
    self.pending_resync_requests.retain(|key, _| key.target_host != *name);

    let mut affected_repos = Vec::new();
    let mut resync_requests = Vec::new();
    let origins: Vec<HostName> = self.peer_data.keys().cloned().collect();

    for origin in origins {
        let affected_for_origin: Vec<RepoIdentity> = self
            .peer_data
            .get(&origin)
            .map(|repos| {
                repos
                    .iter()
                    .filter(|(_, state)| state.via_peer == *name && state.via_generation == generation)
                    .map(|(repo_id, _)| repo_id.clone())
                    .collect()
            })
            .unwrap_or_default();

        if affected_for_origin.is_empty() {
            continue;
        }

        let replacement = self.promote_route_after_disconnect(&origin);
        if let Some(next_hop) = replacement {
            if let Some(repos) = self.peer_data.get_mut(&origin) {
                for repo_id in &affected_for_origin {
                    if let Some(state) = repos.get_mut(repo_id) {
                        state.stale = true;
                        state.via_peer = next_hop.next_hop.clone();
                        state.via_generation = next_hop.next_hop_generation;
                    }
                }
            }

            for repo_id in &affected_for_origin {
                let request_id = self.next_request_id();
                let key = ReversePathKey {
                    request_id,
                    requester_host: self.local_host.clone(),
                    target_host: origin.clone(),
                    repo_identity: repo_id.clone(),
                };
                self.pending_resync_requests
                    .insert(key, PendingResyncRequest { deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT });
                resync_requests.push(RoutedPeerMessage::RequestResync {
                    request_id,
                    requester_host: self.local_host.clone(),
                    target_host: origin.clone(),
                    remaining_hops: Self::DEFAULT_ROUTED_HOPS,
                    repo_identity: repo_id.clone(),
                    since_seq: 0,
                });
            }

            debug!(
                origin = %origin,
                via = %next_hop.next_hop,
                repos = affected_for_origin.len(),
                "retaining stale peer data while failover resync is pending"
            );
        } else {
            if let Some(repos) = self.peer_data.get_mut(&origin) {
                for repo_id in &affected_for_origin {
                    repos.remove(repo_id);
                }
                if repos.is_empty() {
                    self.peer_data.remove(&origin);
                }
            }
            self.routes.remove(&origin);
        }

        affected_repos.extend(affected_for_origin);
    }

    self.last_seen_clocks.retain(|(host, _), _| host != name);

    // Compute overlay updates atomically while still holding &mut self.
    let mut overlay_updates = Vec::new();
    for repo_id in &affected_repos {
        if let Some(local_path) = local_repo_paths.get(repo_id) {
            // Local repo — collect remaining peer data
            let peers: Vec<(HostName, ProviderData)> = self
                .peer_data
                .iter()
                .filter_map(|(host, repos)| repos.get(repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                .collect();
            overlay_updates.push(OverlayUpdate::SetProviders { path: local_path.clone(), peers });
        } else if self.has_peer_data_for(repo_id) {
            // Remote-only, still has data from other peers
            if let Some(synthetic_path) = self.known_remote_repos.get(repo_id).cloned() {
                let peers: Vec<(HostName, ProviderData)> = self
                    .peer_data
                    .iter()
                    .filter_map(|(host, repos)| repos.get(repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect();
                overlay_updates.push(OverlayUpdate::SetProviders { path: synthetic_path, peers });
            }
        } else if let Some(synthetic_path) = self.unregister_remote_repo(repo_id) {
            // Remote-only, no peers remain — remove the virtual tab
            overlay_updates.push(OverlayUpdate::RemoveRepo { identity: repo_id.clone(), path: synthetic_path });
        }
    }

    DisconnectPlan { was_active: true, affected_repos, resync_requests, overlay_updates }
}
```

- [ ] **Step 4: Fix all existing call sites**

All existing calls to `disconnect_peer` pass an empty `&HashMap::new()` for now (they'll get the real mapping in a later step). Search for `disconnect_peer(` in `manager.rs` tests and `server.rs`:

In `server.rs` (`disconnect_peer_and_rebuild`, line 736):
```rust
// Will be updated properly in Task 4
pm.disconnect_peer(peer_name, generation, &HashMap::new())
```

In test files: update all test calls similarly. There are ~7 calls in `manager.rs` tests.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-daemon 2>&1 | tail -20`
Expected: All tests pass, including the two new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs
git commit -m "fix(#263): compute overlay updates atomically in disconnect_peer"
```

### Task 3: Add `repo_identity_snapshot` to InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs:400-409`

The caller of `disconnect_peer` needs a snapshot of identity → path mappings. Add a method to get it.

- [ ] **Step 1: Add `repo_identity_snapshot` method**

Add after `find_identity_for_path` in `crates/flotilla-core/src/in_process.rs`:

```rust
/// Snapshot of all RepoIdentity → local path mappings.
///
/// Used by the disconnect path to pass the mapping into PeerManager
/// so overlay updates can be computed atomically under the PeerManager lock.
pub async fn repo_identity_snapshot(&self) -> HashMap<flotilla_protocol::RepoIdentity, PathBuf> {
    self.repo_identities.read().await.clone()
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -10`
Expected: Compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs
git commit -m "feat: add repo_identity_snapshot to InProcessDaemon"
```

### Task 4: Update `disconnect_peer_and_rebuild` to use pre-computed overlays

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs:728-743` (`disconnect_peer_and_rebuild`)

This wires Task 2 and Task 3 together. Instead of calling `rebuild_peer_overlays`, apply the pre-computed `OverlayUpdate` values.

- [ ] **Step 1: Update `disconnect_peer_and_rebuild`**

Replace the function in `crates/flotilla-daemon/src/server.rs`:

```rust
async fn disconnect_peer_and_rebuild(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &HostName,
    generation: u64,
) -> crate::peer::DisconnectPlan {
    // Snapshot identity mapping before acquiring PeerManager lock.
    // This mapping is stable during disconnect — local repos aren't
    // removed by peer disconnect.
    let local_repo_paths = daemon.repo_identity_snapshot().await;

    let plan = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(peer_name, generation, &local_repo_paths)
    };

    // Apply pre-computed overlay updates outside the PeerManager lock.
    for update in &plan.overlay_updates {
        match update {
            crate::peer::OverlayUpdate::SetProviders { path, peers } => {
                daemon.set_peer_providers(path, peers.clone()).await;
            }
            crate::peer::OverlayUpdate::RemoveRepo { identity, path } => {
                info!(
                    repo = %identity,
                    path = %path.display(),
                    "removing virtual repo — no peers remaining"
                );
                if let Err(e) = daemon.remove_repo(path).await {
                    warn!(
                        repo = %identity,
                        err = %e,
                        "failed to remove virtual repo"
                    );
                }
            }
        }
    }

    dispatch_resync_requests(peer_manager, plan.resync_requests.clone()).await;
    plan
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test -p flotilla-daemon 2>&1 | tail -20`
Expected: All pass.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p flotilla-daemon --all-targets --locked -- -D warnings 2>&1 | tail -20`
Expected: No warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "fix(#263): wire atomic overlay updates into disconnect_peer_and_rebuild"
```

## Chunk 2: Send local state on connect (#290)

### Task 5: Add `tracked_repo_paths` to InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`

- [ ] **Step 1: Add the method**

Add after `repo_identity_snapshot` in `crates/flotilla-core/src/in_process.rs`:

```rust
/// Returns the paths of all locally tracked repos.
///
/// Only local repo paths, not remote/virtual ones. Used by the outbound
/// task to send local data to a newly connected peer.
pub async fn tracked_repo_paths(&self) -> Vec<PathBuf> {
    self.repos.read().await.keys().cloned().collect()
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-core 2>&1 | head -10`
Expected: Compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs
git commit -m "feat: add tracked_repo_paths to InProcessDaemon"
```

### Task 6: Add `get_sender_if_current` to PeerManager

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`

- [ ] **Step 1: Write failing test**

Add to the `tests` module in `crates/flotilla-daemon/src/peer/manager.rs`:

```rust
#[tokio::test]
async fn get_sender_if_current_returns_sender_for_matching_generation() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    let generation = accepted_generation(mgr.activate_connection(
        HostName::new("peer"),
        sender,
        ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        },
    ));

    assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation).is_some());
    assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation + 1).is_none());
    assert!(mgr.get_sender_if_current(&HostName::new("unknown"), 1).is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-daemon get_sender_if_current 2>&1 | tail -10`
Expected: Compilation error — method doesn't exist.

- [ ] **Step 3: Implement the method**

Add to `impl PeerManager` in `crates/flotilla-daemon/src/peer/manager.rs`, near `active_peer_senders`:

```rust
/// Returns the sender for a peer only if the given generation matches
/// the peer's current generation. Used by targeted sends to avoid
/// sending to a connection that has been superseded.
pub fn get_sender_if_current(&self, peer: &HostName, generation: u64) -> Option<Arc<dyn PeerSender>> {
    if !self.generation_is_current(peer, generation) {
        return None;
    }
    self.senders.get(peer).cloned()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-daemon get_sender_if_current 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs
git commit -m "feat: add get_sender_if_current to PeerManager"
```

### Task 7: Add `PeerConnectedNotice` and `send_local_to_peer` helper

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Add `PeerConnectedNotice` struct**

Add near the top of `crates/flotilla-daemon/src/server.rs`, after the `SocketPeerSender` impl:

```rust
/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
struct PeerConnectedNotice {
    peer: HostName,
    generation: u64,
}
```

- [ ] **Step 2: Add `send_local_to_peer` helper**

Add after `send_local_to_peers` in `crates/flotilla-daemon/src/server.rs`:

```rust
/// Send current local state for all repos to a specific newly-connected peer.
///
/// Unlike `send_local_to_peers` (which broadcasts), this targets a single peer
/// that has just connected and has no state. Bypasses `last_sent_versions` since
/// the peer needs everything regardless of what was previously sent to others.
///
/// The generation guard ensures this is a no-op if the connection has already
/// been superseded between the notice being sent and this function running.
async fn send_local_to_peer(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    peer: &HostName,
    generation: u64,
) -> bool {
    let repo_paths = daemon.tracked_repo_paths().await;
    let mut any_sent = false;

    // Resolve the sender once before iterating repos. If the connection
    // has already been superseded, skip the entire loop.
    let sender = {
        let pm = peer_manager.lock().await;
        pm.get_sender_if_current(peer, generation)
    };
    let Some(sender) = sender else {
        debug!(peer = %peer, "peer connection superseded, skipping local state send");
        return false;
    };

    for repo_path in repo_paths {
        let Some((local_providers, version)) = daemon.get_local_providers(&repo_path).await else {
            continue;
        };
        let Some(identity) = daemon.find_identity_for_path(&repo_path).await else {
            continue;
        };

        clock.tick(host_name);
        let msg = PeerDataMessage {
            origin_host: host_name.clone(),
            repo_identity: identity,
            repo_path: repo_path.clone(),
            clock: clock.clone(),
            kind: flotilla_protocol::PeerDataKind::Snapshot { data: Box::new(local_providers), seq: version },
        };

        if let Err(e) = sender.send(PeerWireMessage::Data(msg)).await {
            debug!(peer = %peer, err = %e, "failed to send local state to peer");
        } else {
            any_sent = true;
        }
    }
    any_sent
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-daemon 2>&1 | head -20`
Expected: Compiles (possibly with dead-code warning since nothing calls it yet).

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "feat: add PeerConnectedNotice and send_local_to_peer helper"
```

### Task 8: Wire the notification channel into the outbound task and connection sites

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs:200-590` (channel creation, sender cloning, outbound task select loop)

This is the main wiring task. Create the channel, send notices from all three connection sites, and receive in the outbound task.

- [ ] **Step 1: Create the channel in `DaemonServer::run()`**

In the `run()` method, after the `peer_data_tx`/`peer_data_rx` channel creation, add:

```rust
let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();
```

- [ ] **Step 2: Send notices from the SSH initial connect and reconnect paths**

The SSH per-peer tasks are spawned at line 247 inside an outer `tokio::spawn` (line 212). The notification sender must be threaded through both layers:

1. Clone `peer_connected_tx` into the outer peer-manager task scope (before line 212's `tokio::spawn`):
   ```rust
   let peer_connected_tx_for_ssh = peer_connected_tx.clone();
   ```

2. Inside the outer task, before the per-peer loop (line 241), clone per-peer:
   ```rust
   let peer_connected_tx_clone = peer_connected_tx_for_ssh.clone();
   ```

3. Move `peer_connected_tx_clone` into each per-peer `tokio::spawn`.

4. **Initial connect** — between line 249 (`if let Some((generation, mut inbound_rx)) = initial_rx`) and line 250 (`forward_until_closed`), send:
   ```rust
   let _ = peer_connected_tx_clone.send(PeerConnectedNotice {
       peer: peer_name.clone(),
       generation,
   });
   ```

5. **Reconnect** — after the `PeerStatusChanged::Connected` event at line 300-303, send:
   ```rust
   let _ = peer_connected_tx_clone.send(PeerConnectedNotice {
       peer: peer_name.clone(),
       generation,
   });
   ```

Both sites use the same `peer_connected_tx_clone` (same spawned task).

- [ ] **Step 3: Send notices from the inbound socket peer handler**

The inbound socket path uses `handle_client` (server.rs:823), not a separate function. Add `peer_connected_tx` as a new parameter:

```rust
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
)
```

At the call site (line 609), clone and pass:
```rust
let peer_connected_tx = peer_connected_tx.clone();
// ... in the tokio::spawn:
handle_client(stream, daemon, shutdown_rx, peer_data_tx, peer_manager, peer_connected_tx, client_count, client_notify).await;
```

After the `PeerStatusChanged::Connected` event at line 990, add:
```rust
let _ = peer_connected_tx.send(PeerConnectedNotice {
    peer: host_name.clone(),
    generation,
});
```

**Important:** Update the two existing tests that call `handle_client` directly:
- `handle_client_forwards_peer_data_and_registers_peer` (line 1362): add `mpsc::unbounded_channel::<PeerConnectedNotice>()` and pass the sender.
- `handle_client_relays_outbound_peer_messages` (line 1520): same.

- [ ] **Step 5: Update the outbound task to `select!` on both channels**

Replace the outbound task loop (lines 544-587) with:

```rust
loop {
    tokio::select! {
        notice = peer_connected_rx.recv() => {
            let Some(notice) = notice else { break };
            debug!(peer = %notice.peer, generation = notice.generation, "sending local state to newly connected peer");
            send_local_to_peer(
                &outbound_daemon,
                &outbound_peer_manager,
                &host_name,
                &mut outbound_clock,
                &notice.peer,
                notice.generation,
            )
            .await;
        }
        event = event_rx.recv() => {
            let repo_path = match event {
                Ok(DaemonEvent::SnapshotFull(snapshot)) => Some(snapshot.repo.clone()),
                Ok(DaemonEvent::SnapshotDelta(delta)) => Some(delta.repo.clone()),
                Ok(_) => None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "outbound peer event subscriber lagged");
                    None
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            };
            if let Some(repo_path) = repo_path {
                let Some((local_providers, version)) = outbound_daemon.get_local_providers(&repo_path).await else {
                    continue;
                };
                let last = last_sent_versions.get(&repo_path).copied().unwrap_or(0);
                if version <= last {
                    continue;
                }
                let sent = send_local_to_peers(
                    &outbound_daemon,
                    &outbound_peer_manager,
                    &host_name,
                    &mut outbound_clock,
                    &repo_path,
                    local_providers,
                    version,
                )
                .await;
                if sent {
                    last_sent_versions.insert(repo_path, version);
                }
            }
        }
    }
}
```

Move `peer_connected_rx` into the outbound task's spawned closure.

- [ ] **Step 6: Verify it compiles**

Run: `cargo build -p flotilla-daemon 2>&1 | head -30`
Expected: Compiles cleanly.

- [ ] **Step 7: Run all tests**

Run: `cargo test -p flotilla-daemon 2>&1 | tail -20`
Expected: All pass.

- [ ] **Step 8: Run clippy**

Run: `cargo clippy -p flotilla-daemon --all-targets --locked -- -D warnings 2>&1 | tail -20`
Expected: No warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "feat(#290): wire peer-connected notification channel for local state send"
```

## Chunk 3: Full verification

### Task 9: Format, lint, and full test suite

**Files:** All modified files

- [ ] **Step 1: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 2: Clippy (all targets)**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -30`
Expected: No warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo test --locked 2>&1 | tail -30`
Expected: All tests pass.

- [ ] **Step 4: Commit any formatting changes**

```bash
git add -A
git commit -m "chore: rustfmt"
```

(Only if formatting produced changes.)
