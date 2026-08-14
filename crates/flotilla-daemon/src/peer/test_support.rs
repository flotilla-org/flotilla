use std::sync::{Arc, Mutex};

use flotilla_protocol::{GoodbyeReason, NodeId, NodeInfo, PeerWireMessage};
use tokio::sync::Notify;

use crate::peer::{
    transport::{PeerConnectionStatus, PeerTransport},
    ActivationResult, ConnectionDirection, ConnectionMeta, PeerManager, PeerSender,
};

#[doc(hidden)]
pub fn ensure_test_connection_generation<F>(mgr: &mut PeerManager, origin: &NodeId, mut make_sender: F) -> u64
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
    remote_node: Option<NodeInfo>,
    subscribe_error: Option<String>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    pub fn new() -> Self {
        Self { status: PeerConnectionStatus::Connected, sender: None, remote_node: None, subscribe_error: None }
    }

    pub fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let (mock_sender, sent) = MockPeerSender::new();
        let sender: Arc<dyn PeerSender> = Arc::new(mock_sender);
        (Self { status: PeerConnectionStatus::Connected, sender: Some(sender), remote_node: None, subscribe_error: None }, sent)
    }

    pub fn with_remote_node(mut self, node: NodeInfo) -> Self {
        self.remote_node = Some(node);
        self
    }

    pub fn with_subscribe_error(mut self, error: impl Into<String>) -> Self {
        self.subscribe_error = Some(error.into());
        self
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

    fn connection_address(&self) -> String {
        self.remote_node.as_ref().map(|node| format!("mock://{}", node.node_id)).unwrap_or_else(|| "mock://unknown".to_string())
    }

    async fn subscribe(&mut self) -> Result<tokio::sync::mpsc::Receiver<PeerWireMessage>, String> {
        if let Some(error) = self.subscribe_error.take() {
            return Err(error);
        }
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.sender.clone()
    }

    fn remote_node_info(&self) -> Option<NodeInfo> {
        self.remote_node.clone()
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
