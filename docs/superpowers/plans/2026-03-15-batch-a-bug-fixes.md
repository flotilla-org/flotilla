# Batch A — Bug Fixes & Hardening

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix three independent multi-host bugs: the forwarded-command cancel race (#337), the stale `set_peer_providers` apply-ordering race (#304), and add an integration test for the peer-connect → local-state-send flow (#306).

**Architecture:** Three independent changes to `flotilla-daemon` and `flotilla-core`. Task 1 moves the `ForwardedCommand` registration earlier so cancels always find it. Task 2 adds a monotonic overlay version to `PeerManager` and makes `set_peer_providers` conditional. Task 3 wires up `spawn_peer_networking_runtime` in a test to verify the end-to-end outbound flow.

**Tech Stack:** Rust, tokio, flotilla-daemon, flotilla-core, flotilla-protocol

---

## Task 1: Fix routed remote command cancel race (#337)

**Problem:** `execute_forwarded_command` and `cancel_forwarded_command` are spawned independently via `tokio::spawn`. The execute task inserts the `Launching` entry into `ForwardedCommandMap` as its first async operation, but there is no guarantee it runs before the cancel task. If the cancel task runs first, it finds `None` and returns "remote command not found."

**Fix:** Insert the `Launching` entry synchronously in the dispatch loop (before spawning) so it is guaranteed to exist when any cancel is dispatched. Pass the pre-created `Notify` into `execute_forwarded_command`.

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs:633-643` (dispatch site)
- Modify: `crates/flotilla-daemon/src/server.rs:893-908` (execute_forwarded_command signature + body)
- Test: `crates/flotilla-daemon/src/server.rs` (existing `#[cfg(test)] mod tests`)

### Steps

- [ ] **Step 1: Write a failing test for the race**

Add a test in `crates/flotilla-daemon/src/server.rs` `mod tests` that spawns `cancel_forwarded_command` before any entry exists in the map, demonstrating the "remote command not found" failure.

```rust
#[tokio::test]
async fn cancel_before_execute_registration_finds_entry() {
    // Setup: daemon + peer manager + forwarded_commands map (empty)
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(
        HostName::new("relay"),
        Arc::new(CapturePeerSender { sent: Arc::clone(&sent) }),
    );

    // Spawn cancel against request_id 99 — entry does NOT exist yet.
    let handle = tokio::spawn(cancel_forwarded_command(
        Arc::clone(&daemon),
        Arc::clone(&peer_manager),
        Arc::clone(&forwarded_commands),
        42,                          // cancel_id
        HostName::new("desktop"),    // requester_host
        HostName::new("relay"),      // reply_via
        99,                          // command_request_id
    ));

    // Give the cancel task a moment, then simulate late registration.
    tokio::time::sleep(StdDuration::from_millis(50)).await;

    // Insert the Launching entry (simulating what execute would do).
    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(
        99,
        ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } },
    );
    // Transition to Running and notify.
    if let Some(entry) = forwarded_commands.lock().await.get_mut(&99) {
        entry.state = ForwardedCommandState::Running { command_id: 456 };
    }
    ready.notify_waiters();

    handle.await.expect("cancel task");

    // With the fix, cancel should find the entry and attempt cancel (not "not found").
    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Routed(RoutedPeerMessage::CommandCancelResponse { error, .. }) => {
            // Should NOT contain "remote command not found"
            assert!(
                !error.as_deref().unwrap_or("").contains("remote command not found"),
                "cancel should not fail with 'not found', got: {error:?}"
            );
        }
        other => panic!("expected cancel response, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p flotilla-daemon --lib -- tests::cancel_before_execute_registration_finds_entry`

Expected: FAIL — the cancel task sees `None` and returns "remote command not found" before the late registration happens, because `await_forwarded_command_id` returns `Err` immediately on `None`.

- [ ] **Step 3: Implement the fix — move registration before spawn**

In `crates/flotilla-daemon/src/server.rs`, modify the `HandleResult::CommandRequested` dispatch arm (lines 633-643) to insert the `Launching` entry before spawning:

```rust
HandleResult::CommandRequested { request_id, requester_host, reply_via, command } => {
    drop(pm);
    let ready = Arc::new(Notify::new());
    forwarded_commands_task
        .lock()
        .await
        .insert(request_id, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
    tokio::spawn(execute_forwarded_command(
        Arc::clone(&peer_daemon),
        Arc::clone(&peer_manager_task),
        Arc::clone(&forwarded_commands_task),
        request_id,
        requester_host,
        reply_via,
        command,
        ready,
    ));
}
```

Then modify `execute_forwarded_command` to accept `ready: Arc<Notify>` as a parameter and remove the internal insert:

**Signature** (line 893-901) becomes:
```rust
async fn execute_forwarded_command(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    forwarded_commands: ForwardedCommandMap,
    request_id: u64,
    requester_host: HostName,
    reply_via: HostName,
    command: Command,
    ready: Arc<Notify>,
)
```

**Remove** lines 904-908 (the `let ready = ...` + `forwarded_commands.lock().await.insert(...)` block). The `ready` variable now comes from the parameter.

- [ ] **Step 3b: Update existing test callers of `execute_forwarded_command`**

Three existing tests call `execute_forwarded_command` directly and need the new `ready` parameter. For each, pre-insert a `Launching` entry and pass the `ready` (matching the new caller contract):

- Line 2329 (`execute_forwarded_command_proxies_lifecycle_and_response`)
- Line 2442 (PrepareTerminalForCheckout test)
- Line 2551 (routed checkout test)

For each test, add before the `execute_forwarded_command` call:
```rust
let ready = Arc::new(Notify::new());
forwarded_commands.lock().await.insert(
    request_id,
    ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } },
);
```
And pass `ready` as the final argument. (Use the same `request_id` value that the test passes to `execute_forwarded_command`.)

- [ ] **Step 4: Run tests to verify the fix**

Run: `cargo test -p flotilla-daemon --lib`

Expected: All tests pass, including the new test and the existing `cancel_forwarded_command_waits_for_launching_registration` test.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "fix: register forwarded command before spawn to prevent cancel race (#337)"
```

---

## Task 2: Versioned `set_peer_providers` to eliminate apply-ordering race (#304)

**Problem:** All overlay write paths (disconnect cleanup, inbound data, resync sweep) share a read-then-apply pattern: read peer data under the `PeerManager` lock, drop the lock, then call `set_peer_providers` on `InProcessDaemon`. Because `set_peer_providers` is a blind replace, a stale apply can overwrite newer data if another path wrote in between.

**Fix:** Add a monotonic `overlay_version` counter to `PeerManager` that increments on every peer-data mutation. Each `set_peer_providers` call carries the version; the method only applies if the version is newer than what's stored.

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:169-196` (PeerManager struct — add `overlay_version` field)
- Modify: `crates/flotilla-daemon/src/peer/manager.rs` (bump version in `handle_inbound`, `disconnect_peer`, `store_snapshot_from`)
- Modify: `crates/flotilla-daemon/src/peer/manager.rs:136-142` (OverlayUpdate — add version field)
- Modify: `crates/flotilla-core/src/in_process.rs:490` (add `peer_overlay_versions` field)
- Modify: `crates/flotilla-core/src/in_process.rs:769-778` (`set_peer_providers` — add version check)
- Modify: `crates/flotilla-daemon/src/server.rs:554-580` (HandleResult::Updated caller — pass version)
- Modify: `crates/flotilla-daemon/src/server.rs:814-862` (rebuild_peer_overlays — pass version)
- Modify: `crates/flotilla-daemon/src/server.rs:1132-1192` (disconnect_peer_and_rebuild — pass version)
- Test: `crates/flotilla-daemon/src/peer/manager.rs` (unit tests for overlay_version)
- Test: `crates/flotilla-core/src/in_process.rs` or `crates/flotilla-daemon/src/server.rs` (test stale-apply rejection)

### Steps

- [ ] **Step 1: Write a failing test for stale apply**

In `crates/flotilla-daemon/src/server.rs` `mod tests`, add a test that verifies a stale `set_peer_providers` call does NOT overwrite newer data:

```rust
#[tokio::test]
async fn set_peer_providers_rejects_stale_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::new("local")).await;

    let fresh_peers = vec![(HostName::new("hostB"), ProviderData {
        checkouts: IndexMap::from([(HostPath::new(HostName::new("hostB"), "/b/repo"), checkout("fresh"))]),
        ..Default::default()
    })];
    let stale_peers = vec![(HostName::new("hostB"), ProviderData {
        checkouts: IndexMap::from([(HostPath::new(HostName::new("hostB"), "/b/repo"), checkout("stale"))]),
        ..Default::default()
    })];

    // Apply fresh data at version 5
    daemon.set_peer_providers(&repo, fresh_peers.clone(), 5).await;

    // Attempt stale apply at version 3 — should be rejected
    daemon.set_peer_providers(&repo, stale_peers, 3).await;

    // Verify fresh data is still present
    let identity = daemon.tracked_repo_identity_for_path(&repo).await.unwrap();
    let pp = daemon.peer_providers_for_test(&identity).await;
    let branch = pp[0].1.checkouts.values().next().unwrap().branch.as_str();
    assert_eq!(branch, "fresh", "stale version should have been rejected");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-daemon --lib -- tests::set_peer_providers_rejects_stale_version`

Expected: Compile error — `set_peer_providers` doesn't accept a version parameter yet.

- [ ] **Step 3: Add `overlay_version` to PeerManager**

In `crates/flotilla-daemon/src/peer/manager.rs`:

Add field to `PeerManager` struct (after line 195):
```rust
overlay_version: u64,
```

Initialize in `PeerManager::new()`:
```rust
overlay_version: 0,
```

Add accessor:
```rust
/// Current overlay version — callers capture this while holding the lock
/// and pass it to `set_peer_providers` so stale applies are rejected.
pub fn overlay_version(&self) -> u64 {
    self.overlay_version
}
```

Add private bump helper:
```rust
fn bump_overlay_version(&mut self) -> u64 {
    self.overlay_version += 1;
    self.overlay_version
}
```

- [ ] **Step 4: Bump overlay_version on peer data mutations**

In `crates/flotilla-daemon/src/peer/manager.rs`, call `self.bump_overlay_version()` at each mutation point:

1. **`store_snapshot_from`** — when returning `HandleResult::Updated`, bump before returning.
2. **`disconnect_peer`** — bump once after clearing the peer's data, so all `OverlayUpdate`s in the plan carry the post-disconnect version.

Find each function and add the bump call. The exact insertion points:
- In `store_snapshot_from` (or wherever `HandleResult::Updated` is constructed): bump just before the return.
- In `disconnect_peer`: bump after removing peer data, capture the version into each `OverlayUpdate::SetProviders`.

- [ ] **Step 5: Add version to `OverlayUpdate::SetProviders`**

In `crates/flotilla-daemon/src/peer/manager.rs` line 139, add the version field:

```rust
pub enum OverlayUpdate {
    SetProviders { identity: RepoIdentity, peers: Vec<(HostName, ProviderData)>, overlay_version: u64 },
    RemoveRepo { identity: RepoIdentity, path: PathBuf },
}
```

Update the construction site in `disconnect_peer` to include the version.

- [ ] **Step 6: Add version storage and gating to `set_peer_providers`**

In `crates/flotilla-core/src/in_process.rs`:

Add a new field alongside `peer_providers` (near line 490):
```rust
peer_overlay_versions: RwLock<HashMap<flotilla_protocol::RepoIdentity, u64>>,
```

Initialize in `InProcessDaemon::new()` (near line 582):
```rust
peer_overlay_versions: RwLock::new(HashMap::new()),
```

Modify `set_peer_providers` (line 769-778) to accept and check a version:
```rust
pub async fn set_peer_providers(&self, repo_path: &Path, peers: Vec<(HostName, ProviderData)>, overlay_version: u64) {
    let Some(identity) = self.tracked_repo_identity_for_path(repo_path).await else {
        return;
    };
    {
        let mut versions = self.peer_overlay_versions.write().await;
        let stored = versions.entry(identity.clone()).or_insert(0);
        if overlay_version < *stored {
            return; // stale — a newer version has already been applied
        }
        *stored = overlay_version;
    }
    {
        let mut pp = self.peer_providers.write().await;
        pp.insert(identity.clone(), peers);
    }
    self.broadcast_snapshot_inner(repo_path, false).await;
}
```

Also add a test accessor (behind `#[cfg(test)]` or pub):
```rust
pub async fn peer_providers_for_test(&self, identity: &flotilla_protocol::RepoIdentity) -> Vec<(HostName, ProviderData)> {
    self.peer_providers.read().await.get(identity).cloned().unwrap_or_default()
}
```

Clean up version tracking on repo removal — inside the existing `if removed_identity { ... }` block in `remove_repo` (near line 1891), alongside the existing `pp.remove(&repo_identity)`:
```rust
if removed_identity {
    let mut pp = self.peer_providers.write().await;
    pp.remove(&repo_identity);
    // Also clean up the version tracker
    drop(pp);
    self.peer_overlay_versions.write().await.remove(&repo_identity);
}
```

- [ ] **Step 7: Update all `set_peer_providers` callers to pass version**

Three call sites in `crates/flotilla-daemon/src/server.rs`:

**Call site 1: `HandleResult::Updated` (lines 554-580)**

Capture version while holding the PM lock, pass to `set_peer_providers`:
```rust
HandleResult::Updated(ref updated_repo_id) => {
    let overlay_version = pm.overlay_version();
    let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = pm
        .get_peer_data()
        .iter()
        .filter_map(|(host, repos)| {
            repos.get(updated_repo_id).map(|state| (host.clone(), state.provider_data.clone()))
        })
        .collect();
    drop(pm);

    if let Some(local_path) = peer_daemon.preferred_local_path_for_identity(updated_repo_id).await {
        peer_daemon.set_peer_providers(&local_path, peers, overlay_version).await;
    } else {
        let synthetic = crate::peer::synthetic_repo_path(&origin, &repo_path);
        let merged = crate::peer::merge_provider_data(
            &flotilla_protocol::ProviderData::default(),
            peer_daemon.host_name(),
            &peers.iter().map(|(h, d)| (h.clone(), d)).collect::<Vec<_>>(),
        );
        if let Err(e) = peer_daemon.add_virtual_repo(updated_repo_id.clone(), synthetic.clone(), merged).await {
            warn!(repo = %updated_repo_id, err = %e, "failed to add virtual repo");
        } else {
            peer_daemon.set_peer_providers(&synthetic, peers, overlay_version).await;
            let mut pm2 = peer_manager_task.lock().await;
            pm2.register_remote_repo(updated_repo_id.clone(), synthetic);
        }
    }
}
```

**Call site 2: `rebuild_peer_overlays` (lines 820-843)**

Capture version under lock:
```rust
let (peers, overlay_version) = {
    let pm = peer_manager.lock().await;
    let v = pm.overlay_version();
    let peers = pm.get_peer_data()
        .iter()
        .filter_map(|(host, repos)| repos.get(&repo_id).map(|state| (host.clone(), state.provider_data.clone())))
        .collect();
    (peers, v)
};
daemon.set_peer_providers(&local_path, peers, overlay_version).await;
```

Same pattern for the remote-only branch (lines 835-843).

**Call site 3: `disconnect_peer_and_rebuild` (lines 1157-1171)**

Read `overlay_version` from the `OverlayUpdate::SetProviders` variant:
```rust
crate::peer::OverlayUpdate::SetProviders { identity, peers, overlay_version } => {
    if let Some(local_path) = daemon.preferred_local_path_for_identity(identity).await {
        daemon.set_peer_providers(&local_path, peers.clone(), *overlay_version).await;
    } else if let Some(synthetic_path) = { ... } {
        daemon.set_peer_providers(&synthetic_path, peers.clone(), *overlay_version).await;
    }
}
```

- [ ] **Step 7b: Update test callers of `set_peer_providers`**

Six test callers use the old 2-argument signature. Add `0` as the version for test callers that don't care about versioning (they're testing other behavior):

- `crates/flotilla-daemon/tests/multi_host.rs` line 361: `.set_peer_providers(&leader_repo, vec![...], 0).await`
- `crates/flotilla-daemon/tests/multi_host.rs` line 411: `.set_peer_providers(&repo, vec![...], 0).await`
- `crates/flotilla-core/tests/in_process_daemon.rs` line 696: `.set_peer_providers(&repo, vec![...], 0).await`
- `crates/flotilla-daemon/src/server.rs` line 2904: `.set_peer_providers(&synthetic, vec![...], 0).await`
- `crates/flotilla-daemon/src/server.rs` line 3257: `.set_peer_providers(&synthetic, vec![...], 0).await`

For each, just append `, 0` as the third argument. These tests don't exercise the versioning behavior — they test other aspects of peer data merging.

- [ ] **Step 8: Remove the stale-apply comment**

In `crates/flotilla-daemon/src/server.rs` lines 1147-1156, remove or replace the comment about the residual apply-ordering race, since it is now fixed:
```rust
// Overlay updates carry a version from the PeerManager, so
// set_peer_providers will reject stale applies that lost the race
// against fresher inbound data.
```

- [ ] **Step 9: Run all tests**

Run: `cargo test -p flotilla-daemon && cargo test -p flotilla-core`

Expected: All pass, including the new stale-rejection test.

- [ ] **Step 10: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`

Expected: Clean.

- [ ] **Step 11: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs crates/flotilla-core/src/in_process.rs
git commit -m "fix: versioned set_peer_providers to eliminate stale-apply race (#304)"
```

---

## Task 3: Integration test for peer connect/reconnect local state send (#306)

**Problem:** The end-to-end flow (peer connects → outbound task receives `PeerConnectedNotice` → `send_local_to_peer` sends local state → peer receives it) is not covered by an integration test. Existing tests cover building blocks individually but not the wired-up path through the outbound task's `select!` loop.

**Fix:** Write an integration test that spawns just the outbound task (via `spawn_peer_networking_runtime` with `peer_data_rx: None` to skip inbound logic), pre-registers a `CapturePeerSender` on the PeerManager, and drives the flow via `PeerConnectedNotice`. This directly tests the outbound task's `select!` loop without needing real transport plumbing.

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs:78` (make `PeerConnectedNotice` public)
- Modify: `crates/flotilla-daemon/src/server.rs` after line 132 (add `spawn_test_peer_networking` helper)
- Create: `crates/flotilla-daemon/tests/peer_connect_flow.rs` (new integration test file)

### Steps

- [ ] **Step 1: Add test helper and make `PeerConnectedNotice` public**

In `crates/flotilla-daemon/src/server.rs` line 78, make the struct public:

```rust
pub struct PeerConnectedNotice {
    pub peer: HostName,
    pub generation: u64,
}
```

Add a public test helper after `spawn_embedded_peer_networking` (after line 132). This spawns the outbound task without the inbound connection machinery:

```rust
/// Spawn the peer networking runtime with pre-built components.
///
/// Test-only entry point: callers provide a PeerManager with pre-configured
/// senders (e.g. CapturePeerSender). Passes `None` for `peer_data_rx` to skip
/// the inbound connection task — tests drive the outbound task via the returned
/// `PeerConnectedNotice` sender.
#[doc(hidden)]
pub fn spawn_test_peer_networking(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(256);
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        None,  // No inbound task — test drives outbound via PeerConnectedNotice
        peer_data_tx,
        pending_remote_commands,
        forwarded_commands,
        pending_remote_cancels,
    )
}
```

- [ ] **Step 2: Write the integration test**

Create `crates/flotilla-daemon/tests/peer_connect_flow.rs`:

```rust
//! Integration test for peer connect → local state send flow (#306).
//!
//! Verifies the end-to-end path: peer connects → outbound task receives
//! PeerConnectedNotice → send_local_to_peer sends data → peer receives it
//! via the PeerManager's sender.
//!
//! Strategy: spawn only the outbound task (inbound skipped via None rx),
//! pre-register a CapturePeerSender, and drive connections via the
//! PeerConnectedNotice channel.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, init_git_repo},
};
use flotilla_daemon::{
    peer::{test_support::ensure_test_connection_generation, PeerManager, PeerSender},
    server::PeerConnectedNotice,
};
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::sync::Mutex;

struct CapturePeerSender {
    sent: Arc<StdMutex<Vec<PeerWireMessage>>>,
}

#[async_trait]
impl PeerSender for CapturePeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}

#[tokio::test]
async fn peer_connect_triggers_local_state_send() {
    // --- Setup daemon A with a repo ---
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    init_git_repo(&repo_path);
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    let daemon = InProcessDaemon::new(
        vec![repo_path.clone()],
        config,
        fake_discovery(false),
        host_a.clone(),
    )
    .await;

    // Refresh so local_data_version > 0
    daemon.refresh(&repo_path).await.expect("refresh");

    // --- Set up PeerManager with a capture sender for host-b ---
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(CapturePeerSender { sent: Arc::clone(&sent) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(host_a.clone())));
    let generation = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };

    // --- Spawn the outbound task ---
    let (_handle, peer_connected_tx) =
        flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    // --- Send PeerConnectedNotice (simulating peer connection) ---
    peer_connected_tx
        .send(PeerConnectedNotice { peer: host_b.clone(), generation })
        .expect("send notice");

    // --- Wait for outbound task to process the notice ---
    // The outbound task runs in a spawned tokio task; give it time to send.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // --- Verify captured messages ---
    let messages = sent.lock().expect("lock");
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::HostSummary(s) if s.host_name == host_a)),
        "peer should receive HostSummary from host-a, got: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::Data(d) if d.origin_host == host_a)),
        "peer should receive repo data from host-a, got: {messages:?}"
    );
}

#[tokio::test]
async fn peer_reconnect_resends_local_state() {
    // --- Same setup ---
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    init_git_repo(&repo_path);
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    let daemon = InProcessDaemon::new(
        vec![repo_path.clone()],
        config,
        fake_discovery(false),
        host_a.clone(),
    )
    .await;
    daemon.refresh(&repo_path).await.expect("refresh");

    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(CapturePeerSender { sent: Arc::clone(&sent) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(host_a.clone())));
    let gen1 = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };

    let (_handle, peer_connected_tx) =
        flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    // --- First connection ---
    peer_connected_tx
        .send(PeerConnectedNotice { peer: host_b.clone(), generation: gen1 })
        .expect("send notice 1");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let first_count = sent.lock().expect("lock").len();
    assert!(first_count > 0, "first connect should have sent messages");

    // --- Simulate disconnect + reconnect (new generation) ---
    let gen2 = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(&host_b, gen1);
        // Re-register sender with new generation
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };
    assert!(gen2 > gen1, "reconnect should have a higher generation");

    // --- Second connection ---
    peer_connected_tx
        .send(PeerConnectedNotice { peer: host_b.clone(), generation: gen2 })
        .expect("send notice 2");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // --- Verify state was resent ---
    let total_count = sent.lock().expect("lock").len();
    assert!(
        total_count > first_count,
        "reconnect should resend local state (first: {first_count}, total: {total_count})"
    );
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p flotilla-daemon --test peer_connect_flow`

Expected: Both tests PASS — the outbound task sends HostSummary + repo data on connect, and resends on reconnect.

- [ ] **Step 4: Run all daemon tests**

Run: `cargo test -p flotilla-daemon`

Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs crates/flotilla-daemon/tests/peer_connect_flow.rs
git commit -m "test: integration test for peer connect/reconnect local state send (#306)"
```

---

## Final Steps

- [ ] **Run full test suite**

```bash
cargo test --locked
```

- [ ] **Run clippy and fmt**

```bash
cargo clippy --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt
```

- [ ] **Final commit (if fmt changes)**

```bash
git add -u
git commit -m "chore: fmt"
```
