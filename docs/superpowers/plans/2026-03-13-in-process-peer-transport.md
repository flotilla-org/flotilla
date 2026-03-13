# In-Process Peer Transport Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a production-grade `ChannelTransport` implementing `PeerTransport` via in-process channels, plus a `TestNetwork` harness for multi-peer integration tests.

**Architecture:** Paired `ChannelTransport` instances communicate via `tokio::mpsc` channels — what one sends, the other receives. A `TestNetwork` harness manages N peers connected in configurable topologies, replicating the relay-then-handle pattern from `server.rs`. Tests exercise `PeerManager` logic (snapshot exchange, dedup, relay) without SSH/network infrastructure.

**Tech Stack:** Rust, tokio (mpsc channels), async-trait. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-03-13-in-process-peer-transport-design.md`

---

## Chunk 1: ChannelTransport Implementation

### File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `crates/flotilla-daemon/src/peer/channel_transport.rs` | `ChannelTransport` + `ChannelSender` implementations |
| Modify | `crates/flotilla-daemon/src/peer/mod.rs` | Add module declaration + re-exports |

### Task 1: ChannelTransport Scaffolding

**Files:**
- Create: `crates/flotilla-daemon/src/peer/channel_transport.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`

- [ ] **Step 1: Write the failing test — pair starts disconnected**

Add to `crates/flotilla-daemon/src/peer/channel_transport.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::sync::mpsc;

use crate::peer::transport::{PeerConnectionStatus, PeerSender, PeerTransport};

const CHANNEL_BUFFER: usize = 256;

// Stub structs — enough to define the factory signature and fail the test
pub struct ChannelTransport;
pub struct ChannelSender;

pub fn channel_transport_pair(
    _local_name: HostName,
    _remote_name: HostName,
) -> (ChannelTransport, ChannelTransport) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_starts_disconnected() {
        let (a, b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        assert_eq!(a.status(), PeerConnectionStatus::Disconnected);
        assert_eq!(b.status(), PeerConnectionStatus::Disconnected);
    }
}
```

Add module to `crates/flotilla-daemon/src/peer/mod.rs` — insert after line 3 (`pub mod ssh_transport;`):

```rust
pub mod channel_transport;
```

And add re-exports after `pub use ssh_transport::SshTransport;` (line 12):

```rust
pub use channel_transport::{channel_transport_pair, ChannelTransport};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-daemon channel_transport::tests::pair_starts_disconnected`
Expected: FAIL — `todo!()` panics or `status()` method doesn't exist.

- [ ] **Step 3: Implement ChannelTransport struct and factory**

Replace the stubs in `crates/flotilla-daemon/src/peer/channel_transport.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::sync::mpsc;

use crate::peer::transport::{PeerConnectionStatus, PeerSender, PeerTransport};

const CHANNEL_BUFFER: usize = 256;

pub struct ChannelTransport {
    local_name: HostName,
    remote_name: HostName,
    status: PeerConnectionStatus,
    outbound_tx: Option<mpsc::Sender<PeerWireMessage>>,
    inbound_rx: Option<mpsc::Receiver<PeerWireMessage>>,
}

impl ChannelTransport {
    pub fn local_name(&self) -> &HostName {
        &self.local_name
    }

    pub fn remote_name(&self) -> &HostName {
        &self.remote_name
    }
}

pub struct ChannelSender {
    tx: tokio::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>,
}

#[async_trait]
impl PeerSender for ChannelSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        let tx = self.tx.lock().await;
        let tx = tx.as_ref().ok_or_else(|| "channel sender retired".to_string())?;
        tx.send(msg).await.map_err(|_| "channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        let tx = self.tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(PeerWireMessage::Goodbye { reason })
                .await
                .map_err(|_| "channel closed".to_string())?;
        }
        Ok(())
    }
}

#[async_trait]
impl PeerTransport for ChannelTransport {
    async fn connect(&mut self) -> Result<(), String> {
        if self.status != PeerConnectionStatus::Disconnected {
            return Err(format!("cannot connect: status is {:?}", self.status));
        }
        if self.outbound_tx.is_none() {
            return Err("cannot connect: transport already used and disconnected".to_string());
        }
        self.status = PeerConnectionStatus::Connecting;
        self.status = PeerConnectionStatus::Connected;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.outbound_tx.take();
        self.inbound_rx.take();
        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        if self.status != PeerConnectionStatus::Connected {
            return Err(format!("cannot subscribe: status is {:?}", self.status));
        }
        self.inbound_rx
            .take()
            .ok_or_else(|| "already subscribed (receiver already taken)".to_string())
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        if self.status != PeerConnectionStatus::Connected {
            return None;
        }
        self.outbound_tx.as_ref().map(|tx| {
            Arc::new(ChannelSender {
                tx: tokio::sync::Mutex::new(Some(tx.clone())),
            }) as Arc<dyn PeerSender>
        })
    }
}

pub fn channel_transport_pair(local_name: HostName, remote_name: HostName) -> (ChannelTransport, ChannelTransport) {
    let (a_to_b_tx, a_to_b_rx) = mpsc::channel(CHANNEL_BUFFER);
    let (b_to_a_tx, b_to_a_rx) = mpsc::channel(CHANNEL_BUFFER);

    let transport_a = ChannelTransport {
        local_name: local_name.clone(),
        remote_name: remote_name.clone(),
        status: PeerConnectionStatus::Disconnected,
        outbound_tx: Some(a_to_b_tx),
        inbound_rx: Some(b_to_a_rx),
    };

    let transport_b = ChannelTransport {
        local_name: remote_name,
        remote_name: local_name,
        status: PeerConnectionStatus::Disconnected,
        outbound_tx: Some(b_to_a_tx),
        inbound_rx: Some(a_to_b_rx),
    };

    (transport_a, transport_b)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-daemon channel_transport::tests::pair_starts_disconnected`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/peer/channel_transport.rs crates/flotilla-daemon/src/peer/mod.rs
git commit -m "feat: add ChannelTransport implementing PeerTransport via mpsc channels"
```

### Task 2: ChannelTransport Unit Tests

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/channel_transport.rs` (test module)

- [ ] **Step 1: Write the connection lifecycle tests**

Add to the `#[cfg(test)] mod tests` block in `channel_transport.rs`:

```rust
    #[test]
    fn pair_has_correct_names() {
        let (a, b) = channel_transport_pair(HostName::new("host-a"), HostName::new("host-b"));
        assert_eq!(a.local_name(), &HostName::new("host-a"));
        assert_eq!(a.remote_name(), &HostName::new("host-b"));
        assert_eq!(b.local_name(), &HostName::new("host-b"));
        assert_eq!(b.remote_name(), &HostName::new("host-a"));
    }

    #[tokio::test]
    async fn connect_transitions_to_connected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect should succeed");
        assert_eq!(a.status(), PeerConnectionStatus::Connected);
    }

    #[tokio::test]
    async fn connect_when_already_connected_fails() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("first connect");
        let err = a.connect().await.expect_err("second connect should fail");
        assert!(err.contains("cannot connect"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn disconnect_transitions_to_disconnected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect");
        a.disconnect().await.expect("disconnect");
        assert_eq!(a.status(), PeerConnectionStatus::Disconnected);
    }

    #[tokio::test]
    async fn sender_returns_none_before_connect() {
        let (a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        assert!(a.sender().is_none());
    }

    #[tokio::test]
    async fn sender_returns_some_after_connect() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect");
        assert!(a.sender().is_some());
    }

    #[tokio::test]
    async fn sender_returns_none_after_disconnect() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect");
        a.disconnect().await.expect("disconnect");
        assert!(a.sender().is_none());
    }

    #[tokio::test]
    async fn reconnect_after_disconnect_fails() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect");
        a.disconnect().await.expect("disconnect");
        let err = a.connect().await.expect_err("reconnect should fail");
        assert!(err.contains("already used"), "unexpected error: {err}");
    }
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p flotilla-daemon channel_transport::tests`
Expected: All PASS

- [ ] **Step 3: Write the messaging tests**

Add to the test module:

```rust
    use std::path::PathBuf;

    use flotilla_protocol::{PeerDataKind, PeerDataMessage, ProviderData, RepoIdentity, VectorClock};

    fn test_snapshot_msg(origin: &str, seq: u64) -> PeerWireMessage {
        PeerWireMessage::Data(PeerDataMessage {
            origin_host: HostName::new(origin),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
        })
    }

    #[tokio::test]
    async fn bidirectional_message_exchange() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect a");
        b.connect().await.expect("connect b");

        let sender_a = a.sender().expect("sender a");
        let sender_b = b.sender().expect("sender b");
        let mut rx_a = a.subscribe().await.expect("subscribe a");
        let mut rx_b = b.subscribe().await.expect("subscribe b");

        // A sends to B
        sender_a.send(test_snapshot_msg("a", 1)).await.expect("send a→b");
        let msg = rx_b.recv().await.expect("recv at b");
        assert!(matches!(msg, PeerWireMessage::Data(PeerDataMessage { origin_host, .. }) if origin_host == HostName::new("a")));

        // B sends to A
        sender_b.send(test_snapshot_msg("b", 1)).await.expect("send b→a");
        let msg = rx_a.recv().await.expect("recv at a");
        assert!(matches!(msg, PeerWireMessage::Data(PeerDataMessage { origin_host, .. }) if origin_host == HostName::new("b")));
    }

    #[tokio::test]
    async fn subscribe_is_one_shot() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect");
        let _rx = a.subscribe().await.expect("first subscribe");
        let err = a.subscribe().await.expect_err("second subscribe should fail");
        assert!(err.contains("already subscribed"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        let err = a.subscribe().await.expect_err("subscribe before connect");
        assert!(err.contains("cannot subscribe"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn retire_sends_goodbye_and_closes() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect a");
        b.connect().await.expect("connect b");

        let sender_a = a.sender().expect("sender a");
        let mut rx_b = b.subscribe().await.expect("subscribe b");

        sender_a.retire(GoodbyeReason::Superseded).await.expect("retire");

        let msg = rx_b.recv().await.expect("recv goodbye");
        assert!(matches!(msg, PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded }));

        // Subsequent sends should fail
        let err = sender_a.send(test_snapshot_msg("a", 2)).await.expect_err("send after retire");
        assert!(err.contains("retired"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn disconnect_closes_receiver() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("a"), HostName::new("b"));
        a.connect().await.expect("connect a");
        b.connect().await.expect("connect b");

        let mut rx_b = b.subscribe().await.expect("subscribe b");

        // Disconnect A — B's receiver should close
        a.disconnect().await.expect("disconnect a");

        // B should get None when trying to receive (channel closed)
        assert!(rx_b.recv().await.is_none(), "receiver should close after peer disconnect");
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-daemon channel_transport::tests`
Expected: All PASS

- [ ] **Step 5: Run full test suite and lint**

Run: `cargo test -p flotilla-daemon --locked && cargo clippy -p flotilla-daemon --all-targets --locked -- -D warnings`
Expected: All PASS, no warnings

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/peer/channel_transport.rs
git commit -m "test: ChannelTransport unit tests for lifecycle, messaging, and edge cases"
```

---

## Chunk 2: TestNetwork Harness and Multi-Peer Tests

### File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/flotilla-daemon/src/peer/test_support.rs` | Add `TestNetwork` harness alongside existing helpers |
| Create | `crates/flotilla-daemon/src/peer/channel_tests.rs` | Level A + Level B integration tests |
| Modify | `crates/flotilla-daemon/src/peer/mod.rs` | Add `#[cfg(test)] mod channel_tests` |

### Task 3: TestNetwork Harness

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/test_support.rs`
- Modify: `crates/flotilla-daemon/src/peer/mod.rs`

- [ ] **Step 1: Write the failing test — two peers exchange a snapshot**

Add `#[cfg(test)] mod channel_tests;` to `crates/flotilla-daemon/src/peer/mod.rs` (after the `test_support` line).

Create `crates/flotilla-daemon/src/peer/channel_tests.rs`:

```rust
use std::path::PathBuf;

use flotilla_protocol::{
    GoodbyeReason, HostName, PeerDataKind, PeerDataMessage, PeerWireMessage, ProviderData,
    RepoIdentity, VectorClock,
};

use crate::peer::test_support::TestNetwork;

fn test_repo() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

/// Create a snapshot message with the origin host's clock pre-ticked to `seq`.
/// This matches the production pattern where the originating host ticks its
/// own clock before sending, and aligns with the existing test helpers in
/// `manager.rs`.
fn snapshot_msg(origin: &str, repo: &RepoIdentity, seq: u64) -> PeerDataMessage {
    let mut clock = VectorClock::default();
    for _ in 0..seq {
        clock.tick(&HostName::new(origin));
    }
    PeerDataMessage {
        origin_host: HostName::new(origin),
        repo_identity: repo.clone(),
        repo_path: PathBuf::from("/repo"),
        clock,
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
    }
}

/// Helper: check if a peer's manager has stored data from a given origin for a repo.
fn has_peer_data(net: &TestNetwork, peer_idx: usize, origin: &str, repo: &RepoIdentity) -> bool {
    net.manager(peer_idx)
        .get_peer_data()
        .get(&HostName::new(origin))
        .and_then(|repos| repos.get(repo))
        .is_some()
}

#[tokio::test]
async fn two_peer_snapshot_exchange() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    // Inject a snapshot from host-a into its manager
    let repo = test_repo();
    let msg = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg.clone()).await;

    // Settle — host-b should receive and store the snapshot
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's snapshot");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-daemon peer::channel_tests::two_peer_snapshot_exchange`
Expected: FAIL — `TestNetwork` doesn't exist yet.

- [ ] **Step 3: Implement TestNetwork**

Add to `crates/flotilla-daemon/src/peer/test_support.rs`, below the existing helpers:

```rust
use std::path::PathBuf;

use flotilla_protocol::{PeerDataMessage, PeerWireMessage, RepoIdentity};
use tokio::sync::mpsc;

use crate::peer::{
    channel_transport::channel_transport_pair, InboundPeerEnvelope, PeerManager,
};

pub struct TestPeer {
    pub name: HostName,
    pub manager: PeerManager,
    receivers: Vec<(HostName, u64, mpsc::Receiver<PeerWireMessage>)>,
}

pub struct TestNetwork {
    peers: Vec<TestPeer>,
}

impl TestNetwork {
    pub fn new() -> Self {
        Self { peers: Vec::new() }
    }

    pub fn add_peer(&mut self, name: &str) -> usize {
        let host = HostName::new(name);
        let manager = PeerManager::new(host.clone());
        let idx = self.peers.len();
        self.peers.push(TestPeer { name: host, manager, receivers: Vec::new() });
        idx
    }

    pub fn connect(&mut self, a: usize, b: usize) {
        let name_a = self.peers[a].name.clone();
        let name_b = self.peers[b].name.clone();
        let (transport_a, transport_b) = channel_transport_pair(name_a.clone(), name_b.clone());
        self.peers[a].manager.add_peer(name_b, Box::new(transport_a));
        self.peers[b].manager.add_peer(name_a, Box::new(transport_b));
    }

    pub async fn start(&mut self) {
        for peer in &mut self.peers {
            let connections = peer.manager.connect_all().await;
            peer.receivers = connections;
        }
    }

    /// Inject a local data message into a peer's outbound path.
    /// The peer's manager relays it to connected peers via their senders.
    pub async fn inject_local_data(&mut self, peer_idx: usize, msg: PeerDataMessage) {
        let peer = &self.peers[peer_idx];
        peer.manager.relay(&peer.name, &msg).await;
    }

    /// Process all pending inbound messages for a single peer.
    /// Replicates the relay-then-handle pattern from server.rs.
    /// Returns the number of messages processed.
    pub async fn process_peer(&mut self, peer_idx: usize) -> usize {
        let mut messages = Vec::new();
        for (connection_peer, _gen, receiver) in &mut self.peers[peer_idx].receivers {
            while let Ok(msg) = receiver.try_recv() {
                messages.push((connection_peer.clone(), msg));
            }
        }

        let count = messages.len();
        let peer = &mut self.peers[peer_idx];

        for (connection_peer, msg) in messages {
            let generation = peer
                .manager
                .current_generation(&connection_peer)
                .expect("no generation for connected peer");

            if let PeerWireMessage::Data(ref data_msg) = msg {
                // Use origin_host (not connection_peer) to match production
                // semantics in server.rs — relay skips the original author.
                peer.manager.relay(&data_msg.origin_host, data_msg).await;
            }

            let env = InboundPeerEnvelope {
                msg,
                connection_generation: generation,
                connection_peer,
            };
            peer.manager.handle_inbound(env).await;
        }

        count
    }

    /// Process messages across all peers until quiescent (no more pending).
    /// Safety limit of 100 rounds to prevent infinite loops.
    pub async fn settle(&mut self) {
        for _ in 0..100 {
            let mut total = 0;
            for i in 0..self.peers.len() {
                total += self.process_peer(i).await;
            }
            if total == 0 {
                break;
            }
        }
    }

    pub fn manager(&self, peer_idx: usize) -> &PeerManager {
        &self.peers[peer_idx].manager
    }

    pub fn manager_mut(&mut self, peer_idx: usize) -> &mut PeerManager {
        &mut self.peers[peer_idx].manager
    }
}
```

Note: The existing imports in `test_support.rs` will need to be updated to include the new types. Merge the new `use` items with the existing ones at the top of the file.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-daemon peer::channel_tests::two_peer_snapshot_exchange`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test -p flotilla-daemon --locked`
Expected: All PASS — existing tests unaffected.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-daemon/src/peer/test_support.rs crates/flotilla-daemon/src/peer/channel_tests.rs crates/flotilla-daemon/src/peer/mod.rs
git commit -m "feat: add TestNetwork harness for in-process multi-peer testing"
```

### Task 4: Level A + Level B Integration Tests

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/channel_tests.rs`

- [ ] **Step 1: Write Level A tests — vector clock dedup and goodbye flow**

Add to `crates/flotilla-daemon/src/peer/channel_tests.rs`:

```rust
#[tokio::test]
async fn vector_clock_dedup_drops_duplicate() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let repo = test_repo();

    // Send snapshot with seq 1 (clock: {host-a: 1})
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "first snapshot should be stored");

    // Send same snapshot again with same clock — should be deduped
    // because relay stamps the clock identically and dominated_by returns true
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;
    net.settle().await;

    // Now send seq 2 (clock: {host-a: 2}) — this should NOT be deduped
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 2)).await;
    net.settle().await;

    // Verify the seq 2 snapshot was accepted by checking the stored seq
    let peer_data = net.manager(b).get_peer_data();
    let state = peer_data
        .get(&HostName::new("host-a"))
        .and_then(|repos| repos.get(&repo))
        .expect("host-b should have host-a's data");
    assert_eq!(state.seq, 2, "seq 2 snapshot should have been accepted (not deduped)");
}

#[tokio::test]
async fn bidirectional_snapshot_exchange() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let repo = test_repo();

    // A sends snapshot
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;
    // B sends snapshot
    net.inject_local_data(b, snapshot_msg("host-b", &repo, 1)).await;

    net.settle().await;

    // B has A's data
    assert!(has_peer_data(&net, b, "host-a", &repo), "B should have A's data");
    // A has B's data
    assert!(has_peer_data(&net, a, "host-b", &repo), "A should have B's data");
}

#[tokio::test]
async fn goodbye_flow_through_channel() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    // Get A's sender for B and retire it
    // resolve_sender returns Result<Arc<dyn PeerSender>, String>
    let sender = net.manager(a).resolve_sender(&HostName::new("host-b"))
        .expect("A should have a sender for B");
    sender.retire(GoodbyeReason::Superseded).await.expect("retire");

    // Process B — it should receive the Goodbye
    let processed = net.process_peer(b).await;
    assert!(processed > 0, "B should have received the Goodbye message");
}
```

Note: `get_peer_data()` takes no arguments and returns `&HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>>`. The `has_peer_data` test helper (defined above) wraps the two-level lookup. `resolve_sender` returns `Result<Arc<dyn PeerSender>, String>` (manager.rs line 772). The `PerRepoPeerState.seq` field stores the latest sequence number — verify the exact field name in `manager.rs`.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p flotilla-daemon peer::channel_tests`
Expected: All PASS

- [ ] **Step 3: Write Level B test — three-peer relay**

Add to `channel_tests.rs`:

```rust
#[tokio::test]
async fn three_peer_relay_propagation() {
    // Topology: A — B — C (linear chain)
    // A sends a snapshot. B receives and relays to C.
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    // Note: A and C are NOT directly connected
    net.start().await;

    let repo = test_repo();
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;

    // Settle propagates: A→B (relay), B→C (relay)
    net.settle().await;

    // B should have A's data
    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's snapshot");

    // C should also have A's data (relayed via B)
    assert!(
        has_peer_data(&net, c, "host-a", &repo),
        "host-c should have host-a's snapshot via relay through host-b"
    );
}

#[tokio::test]
async fn three_peer_mesh_dedup() {
    // Topology: full mesh A—B, B—C, A—C
    // A sends a snapshot. B and C each get it directly from A.
    // B also relays to C, but C should dedup it.
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.connect(a, c);
    net.start().await;

    let repo = test_repo();
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;

    net.settle().await;

    // Both B and C have A's data
    assert!(has_peer_data(&net, b, "host-a", &repo), "B should have A's data");
    assert!(has_peer_data(&net, c, "host-a", &repo), "C should have A's data");
}

#[tokio::test]
async fn reverse_direction_snapshot_in_chain() {
    // Topology: A — B — C
    // C sends a snapshot. Verify it propagates to A.
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.start().await;

    let repo = test_repo();
    net.inject_local_data(c, snapshot_msg("host-c", &repo, 1)).await;

    net.settle().await;

    assert!(
        has_peer_data(&net, a, "host-c", &repo),
        "host-a should have host-c's snapshot via relay through host-b"
    );
}
```

- [ ] **Step 4: Run all tests to verify they pass**

Run: `cargo test -p flotilla-daemon peer::channel_tests`
Expected: All PASS

- [ ] **Step 5: Run full test suite and lint**

Run: `cargo test -p flotilla-daemon --locked && cargo clippy -p flotilla-daemon --all-targets --locked -- -D warnings`
Expected: All PASS, no warnings

- [ ] **Step 6: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-daemon/src/peer/channel_tests.rs
git commit -m "test: multi-peer integration tests using ChannelTransport and TestNetwork"
```

---

## Implementation Notes

### PeerManager API (verified against source)

- `PeerManager::new(host: HostName)` — constructor (line 167)
- `add_peer(name, transport)` — register transport (line 189)
- `connect_all()` — connect all transports, activate connections, return `Vec<(HostName, u64, Receiver)>` (line 656)
- `current_generation(name) -> Option<u64>` — get active generation (line 255)
- `relay(origin, msg)` — flood to peers, stamps clock (line 600)
- `handle_inbound(env) -> HandleResult` — process inbound message (line 464)
- `get_peer_data() -> &HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>>` — no arguments, returns full nested map (line 647)
- `resolve_sender(name) -> Result<Arc<dyn PeerSender>, String>` — get sender for a peer (line 772)

### Verify `PerRepoPeerState.seq` field

The dedup test checks `state.seq` to verify the stored sequence number. Confirm the field name in `PerRepoPeerState` — it may be `seq`, `last_seq`, or similar. Adjust the assertion if needed.

### Vector clock behavior

The `snapshot_msg` helper pre-ticks the origin host's clock entry to `seq`, matching the existing test pattern in `manager.rs`. When `inject_local_data` calls `relay()`, relay further stamps the relaying host into the clock before forwarding. This means the clock grows as messages traverse the network — enabling `dominated_by` dedup at each hop.

### Both sides activate as Outbound

When `TestNetwork::start()` calls `connect_all()` on each peer, both sides activate their connection as `ConnectionDirection::Outbound`. This differs from production where one side is Inbound (socket acceptor) and the other is Outbound (SSH initiator). This works because each manager has only one transport per remote peer, so there's no conflict. If future tests need to exercise inbound-vs-outbound conflict resolution, `TestNetwork` would need adjustment.

### Why `try_recv()` in `process_peer()`

The settle loop uses `try_recv()` (non-blocking) rather than `recv()` (blocking) to drain available messages without waiting. This makes tests deterministic — if no messages are pending, the loop terminates immediately. Messages sent during relay within the same `process_peer` call will be picked up in the next `settle` round.
