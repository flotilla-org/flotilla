use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::sync::mpsc;

use crate::peer::transport::{PeerConnectionStatus, PeerSender, PeerTransport};

const CHANNEL_BUFFER: usize = 256;

/// In-process peer transport using `tokio::mpsc` channels.
///
/// Created in pairs via [`channel_transport_pair`] — what one side sends, the
/// other receives. Unlike [`SshTransport`], this transport is **single-lifecycle**:
/// once disconnected, it cannot be reconnected (the channels are consumed).
/// This is inherent to the paired design — there is no persistent listener to
/// reconnect to. For scenarios requiring reconnection (e.g. `PeerManager::reconnect_peer`),
/// create a new pair and re-register.
///
/// Used for in-process multi-peer testing via [`TestNetwork`](super::test_support::TestNetwork),
/// and potentially for future in-process daemon topologies.
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
            tx.send(PeerWireMessage::Goodbye { reason }).await.map_err(|_| "channel closed".to_string())?;
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
        // Transition through Connecting to match SshTransport's status lifecycle.
        // For channels the connection is instant — no async work needed.
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
        self.inbound_rx.take().ok_or_else(|| "already subscribed (receiver already taken)".to_string())
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        if self.status != PeerConnectionStatus::Connected {
            return None;
        }
        self.outbound_tx.as_ref().map(|tx| Arc::new(ChannelSender { tx: tokio::sync::Mutex::new(Some(tx.clone())) }) as Arc<dyn PeerSender>)
    }
}

/// Create a paired set of in-process transports. A's outbound is B's inbound
/// and vice versa. Both start in `Disconnected` state.
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{PeerDataKind, PeerDataMessage, ProviderData, RepoIdentity, VectorClock};

    use super::*;

    fn test_snapshot_msg(origin: &str, seq: u64) -> PeerWireMessage {
        PeerWireMessage::Data(PeerDataMessage {
            origin_host: HostName::new(origin),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
        })
    }

    #[test]
    fn pair_starts_disconnected() {
        let (a, b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        assert_eq!(a.status(), PeerConnectionStatus::Disconnected);
        assert_eq!(b.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn pair_has_correct_names() {
        let (a, b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        assert_eq!(a.local_name(), &HostName::new("alpha"));
        assert_eq!(a.remote_name(), &HostName::new("beta"));
        assert_eq!(b.local_name(), &HostName::new("beta"));
        assert_eq!(b.remote_name(), &HostName::new("alpha"));
    }

    #[tokio::test]
    async fn connect_transitions_to_connected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        assert_eq!(a.status(), PeerConnectionStatus::Connected);
    }

    #[tokio::test]
    async fn connect_when_already_connected_fails() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("first connect should succeed");
        let err = a.connect().await.expect_err("second connect should fail");
        assert!(err.contains("cannot connect"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn disconnect_transitions_to_disconnected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        a.disconnect().await.expect("disconnect should succeed");
        assert_eq!(a.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn sender_returns_none_before_connect() {
        let (a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        assert!(a.sender().is_none(), "sender should be None before connect");
    }

    #[tokio::test]
    async fn sender_returns_some_after_connect() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        assert!(a.sender().is_some(), "sender should be Some after connect");
    }

    #[tokio::test]
    async fn sender_returns_none_after_disconnect() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        a.disconnect().await.expect("disconnect should succeed");
        assert!(a.sender().is_none(), "sender should be None after disconnect");
    }

    #[tokio::test]
    async fn reconnect_after_disconnect_fails() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        a.disconnect().await.expect("disconnect should succeed");
        let err = a.connect().await.expect_err("reconnect should fail");
        assert!(err.contains("already used"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn bidirectional_message_exchange() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let sender_a = a.sender().expect("A should have a sender");
        let sender_b = b.sender().expect("B should have a sender");
        let mut rx_a = a.subscribe().await.expect("subscribe A");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // A sends to B
        sender_a.send(test_snapshot_msg("alpha", 1)).await.expect("A send");
        let msg = rx_b.recv().await.expect("B should receive message from A");
        match msg {
            PeerWireMessage::Data(PeerDataMessage { origin_host, kind: PeerDataKind::Snapshot { seq, .. }, .. }) => {
                assert_eq!(origin_host, HostName::new("alpha"));
                assert_eq!(seq, 1);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // B sends to A
        sender_b.send(test_snapshot_msg("beta", 2)).await.expect("B send");
        let msg = rx_a.recv().await.expect("A should receive message from B");
        match msg {
            PeerWireMessage::Data(PeerDataMessage { origin_host, kind: PeerDataKind::Snapshot { seq, .. }, .. }) => {
                assert_eq!(origin_host, HostName::new("beta"));
                assert_eq!(seq, 2);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_is_one_shot() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        let _rx = a.subscribe().await.expect("first subscribe should succeed");
        let err = a.subscribe().await.expect_err("second subscribe should fail");
        assert!(err.contains("already subscribed"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        let err = a.subscribe().await.expect_err("subscribe before connect should fail");
        assert!(err.contains("cannot subscribe"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn retire_sends_goodbye_and_closes() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let sender_a = a.sender().expect("A should have a sender");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // Retire A's sender — should send Goodbye and close
        sender_a.retire(GoodbyeReason::Superseded).await.expect("retire should succeed");

        let msg = rx_b.recv().await.expect("B should receive Goodbye");
        match msg {
            PeerWireMessage::Goodbye { reason } => {
                assert_eq!(reason, GoodbyeReason::Superseded);
            }
            other => panic!("expected Goodbye, got: {other:?}"),
        }

        // Subsequent sends through the same sender should fail
        let err = sender_a.send(test_snapshot_msg("alpha", 99)).await.expect_err("send after retire should fail");
        assert!(err.contains("retired"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn disconnect_closes_receiver() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // Disconnecting A drops its outbound_tx, which closes B's inbound_rx
        a.disconnect().await.expect("disconnect A");

        // B's receiver should yield None (channel closed)
        let msg = rx_b.recv().await;
        assert!(msg.is_none(), "B's receiver should close after A disconnects");
    }
}
