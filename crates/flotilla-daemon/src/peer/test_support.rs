use std::sync::{Arc, Mutex};

use flotilla_protocol::{GoodbyeReason, HostName, PeerDataMessage, PeerWireMessage};
use tokio::sync::Notify;

use crate::peer::{
    channel_transport::channel_transport_pair,
    transport::{PeerConnectionStatus, PeerTransport},
    ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope, PeerManager, PeerSender,
};

#[doc(hidden)]
pub fn ensure_test_connection_generation<F>(mgr: &mut PeerManager, origin: &HostName, mut make_sender: F) -> u64
where
    F: FnMut() -> Arc<dyn PeerSender>,
{
    if let Some(generation) = mgr.current_generation(origin) {
        return generation;
    }

    for direction in [ConnectionDirection::Inbound, ConnectionDirection::Outbound] {
        match mgr.activate_connection_with_session(
            origin.clone(),
            make_sender(),
            ConnectionMeta { direction, config_label: None, expected_peer: Some(origin.clone()), config_backed: false },
            None,
        ) {
            ActivationResult::Accepted { generation, .. } => return generation,
            ActivationResult::Rejected { .. } => continue,
        }
    }

    panic!("expected test activation for {origin} to succeed");
}

#[doc(hidden)]
pub async fn handle_test_peer_data<F>(mgr: &mut PeerManager, msg: PeerDataMessage, make_sender: F) -> HandleResult
where
    F: FnMut() -> Arc<dyn PeerSender>,
{
    let origin = msg.origin_host.clone();
    let generation = ensure_test_connection_generation(mgr, &origin, make_sender);
    mgr.handle_inbound(InboundPeerEnvelope { msg: PeerWireMessage::Data(msg), connection_generation: generation, connection_peer: origin })
        .await
}

pub struct TestPeer {
    pub name: HostName,
    pub manager: PeerManager,
    receivers: Vec<(HostName, u64, tokio::sync::mpsc::Receiver<PeerWireMessage>)>,
}

pub struct TestNetwork {
    peers: Vec<TestPeer>,
}

impl Default for TestNetwork {
    fn default() -> Self {
        Self::new()
    }
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
    /// Calls relay() to forward to connected peers via their senders.
    /// The msg.origin_host should match the peer's name.
    pub async fn inject_local_data(&mut self, peer_idx: usize, msg: PeerDataMessage) {
        let peer = &self.peers[peer_idx];
        peer.manager.relay(&peer.name, &msg).await;
    }

    /// Process all pending inbound messages for a single peer.
    /// Replicates the relay-then-handle pattern from server.rs.
    pub async fn process_peer(&mut self, peer_idx: usize) -> usize {
        self.process_peer_with_results(peer_idx).await.len()
    }

    /// Process all pending inbound messages for a single peer and return the
    /// handle results in arrival order.
    pub async fn process_peer_with_results(&mut self, peer_idx: usize) -> Vec<HandleResult> {
        let mut messages = Vec::new();
        for (connection_peer, gen, receiver) in &mut self.peers[peer_idx].receivers {
            while let Ok(msg) = receiver.try_recv() {
                messages.push((connection_peer.clone(), *gen, msg));
            }
        }

        let peer = &mut self.peers[peer_idx];
        let mut results = Vec::new();

        for (connection_peer, generation, msg) in messages {
            if let PeerWireMessage::Data(ref data_msg) = msg {
                // Use origin_host (not connection_peer) to match production
                // semantics in server.rs — relay skips the original author.
                peer.manager.relay(&data_msg.origin_host, data_msg).await;
            }

            let env = InboundPeerEnvelope { msg, connection_generation: generation, connection_peer };
            results.push(peer.manager.handle_inbound(env).await);
        }

        results
    }

    /// Process messages across all peers until quiescent.
    /// Safety limit of 100 rounds to prevent infinite loops.
    pub async fn settle(&mut self) {
        for round in 0..100 {
            let mut total = 0;
            for i in 0..self.peers.len() {
                total += self.process_peer(i).await;
            }
            if total == 0 {
                return;
            }
            if round == 99 {
                panic!("settle did not quiesce after 100 rounds — possible relay loop");
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

// ---------------------------------------------------------------------------
// Mock implementations
// ---------------------------------------------------------------------------

pub struct MockPeerSender {
    pub sent: Arc<Mutex<Vec<PeerWireMessage>>>,
}

impl MockPeerSender {
    pub fn new() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (Self { sent: Arc::clone(&sent) }, sent)
    }

    /// Create a throw-away sender whose messages are discarded.
    /// Use when a `PeerSender` is required but the test doesn't inspect what was sent.
    pub fn discard() -> Arc<dyn PeerSender> {
        Arc::new(Self { sent: Arc::new(Mutex::new(Vec::new())) })
    }
}

#[async_trait::async_trait]
impl PeerSender for MockPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}

pub struct BlockingPeerSender {
    pub started: Arc<Notify>,
    pub release: Arc<Notify>,
    pub sent: Arc<Mutex<Vec<PeerWireMessage>>>,
}

#[async_trait::async_trait]
impl PeerSender for BlockingPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.started.notify_waiters();
        self.release.notified().await;
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.started.notify_waiters();
        self.release.notified().await;
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}

pub struct MockTransport {
    pub status: PeerConnectionStatus,
    sender: Option<Arc<dyn PeerSender>>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    pub fn new() -> Self {
        Self { status: PeerConnectionStatus::Connected, sender: None }
    }

    pub fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let (mock_sender, sent) = MockPeerSender::new();
        let sender: Arc<dyn PeerSender> = Arc::new(mock_sender);
        (Self { status: PeerConnectionStatus::Connected, sender: Some(sender) }, sent)
    }
}

#[async_trait::async_trait]
impl PeerTransport for MockTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Connected;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<tokio::sync::mpsc::Receiver<PeerWireMessage>, String> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.sender.clone()
    }
}

pub async fn wait_for_command_result(
    rx: &mut tokio::sync::broadcast::Receiver<flotilla_protocol::DaemonEvent>,
    command_id: u64,
    timeout: std::time::Duration,
) -> flotilla_protocol::commands::CommandValue {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(flotilla_protocol::DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => return result,
                Ok(_) => continue,
                Err(e) => panic!("recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result")
}
