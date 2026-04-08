use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::peer::transport::{PeerConnectionStatus, PeerSender, PeerTransport};

const CHANNEL_BUFFER: usize = 256;

/// Control envelopes carried on the persistent backbone channel.
/// Data is delivered directly via the session channel; the backbone carries
/// only connection lifecycle signals.
enum ChannelEnvelope {
    Connected,
    Disconnected,
}

/// In-process peer transport using persistent backbone channels with session
/// envelopes. Supports full connect/disconnect/reconnect lifecycle.
///
/// Created in pairs via [`channel_transport_pair`]. A persistent backbone
/// `mpsc<ChannelEnvelope>` carries `Connected` and `Disconnected` control
/// signals. Data flows directly via session channels. Each `connect()` creates
/// a fresh session channel; the forwarding task is spawned lazily in `subscribe()`.
///
/// When the remote side disconnects, the local forwarding task detects the
/// `Disconnected` envelope, closes the session (subscriber gets `None`), and
/// transitions the local status to `Disconnected` — matching TCP/SSH semantics.
pub struct ChannelTransport {
    local_name: HostName,
    remote_name: HostName,
    status: Arc<std::sync::Mutex<PeerConnectionStatus>>,
    // Backbone — persistent for the lifetime of the pair (control + fallback data)
    backbone_tx: mpsc::Sender<ChannelEnvelope>,
    backbone_rx: Arc<std::sync::Mutex<Option<mpsc::Receiver<ChannelEnvelope>>>>,
    // Session — created fresh per connect() cycle
    // The tx is stored in a shared slot so the remote's sender() can clone it
    // for direct (non-backbone) data delivery.
    local_session_tx_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>>,
    session_rx: Option<mpsc::Receiver<PeerWireMessage>>,
    // Reference to the remote transport's session_tx_slot — used by sender()
    // to deliver data directly without going through the backbone/forwarding task.
    remote_session_tx_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>>,
    // Forwarding task state — only set after subscribe()
    cancel_tx: Option<oneshot::Sender<()>>,
    task_handle: Option<JoinHandle<()>>,
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
    /// Taken on retire to prevent further sends (acts as a "retired" gate).
    retired: tokio::sync::Mutex<bool>,
    /// Session sender resolved lazily from the remote's slot, then cached.
    /// Once resolved, this sender is bound to that specific session — after
    /// disconnect/reconnect, sends fail (closed channel) rather than leaking
    /// into the new session.
    session_tx: std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>,
    /// Reference to the remote's session slot for lazy resolution.
    remote_session_tx_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>>,
}

impl ChannelSender {
    /// Resolve the session sender: use cached value if available, otherwise
    /// clone from the remote's slot and cache it. Once resolved, the sender
    /// is pinned to that session.
    fn resolve_session_tx(&self) -> Option<mpsc::Sender<PeerWireMessage>> {
        let mut cached = self.session_tx.lock().expect("session_tx lock");
        if cached.is_some() {
            return cached.clone();
        }
        let from_slot = self.remote_session_tx_slot.lock().expect("session slot lock").clone();
        if from_slot.is_some() {
            *cached = from_slot.clone();
        }
        from_slot
    }
}

#[async_trait]
impl PeerSender for ChannelSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        if *self.retired.lock().await {
            return Err("channel sender retired".to_string());
        }

        let tx = self.resolve_session_tx().ok_or_else(|| "channel closed".to_string())?;
        tx.send(msg).await.map_err(|_| "channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        *self.retired.lock().await = true;
        let msg = PeerWireMessage::Goodbye { reason };

        if let Some(tx) = self.resolve_session_tx() {
            tx.send(msg).await.map_err(|_| "channel closed".to_string())?;
        }
        Ok(())
    }
}

/// Monitoring/forwarding task. Watches the backbone for control envelopes
/// (`Connected`, `Disconnected`) and forwards any fallback `Packet` envelopes
/// to the session channel.
async fn forwarding_task(
    mut backbone_rx: mpsc::Receiver<ChannelEnvelope>,
    mut cancel_rx: oneshot::Receiver<()>,
    status: Arc<std::sync::Mutex<PeerConnectionStatus>>,
    local_session_tx_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>>,
    backbone_rx_slot: Arc<std::sync::Mutex<Option<mpsc::Receiver<ChannelEnvelope>>>>,
) {
    loop {
        tokio::select! {
            envelope = backbone_rx.recv() => {
                match envelope {
                    Some(ChannelEnvelope::Disconnected) => {
                        // Remote disconnected — update status BEFORE closing session,
                        // so subscribers see Disconnected as soon as recv() returns None
                        *status.lock().expect("status lock") = PeerConnectionStatus::Disconnected;
                        // Drop session_tx from slot to close the subscriber's session_rx
                        local_session_tx_slot.lock().expect("session slot lock").take();
                        backbone_rx_slot.lock().expect("backbone lock").replace(backbone_rx);
                        return;
                    }
                    Some(ChannelEnvelope::Connected) => {
                        // Remote reconnected — no-op
                    }
                    None => {
                        // Backbone closed (peer transport dropped) — update status,
                        // close session, return backbone, exit.
                        *status.lock().expect("status lock") = PeerConnectionStatus::Disconnected;
                        local_session_tx_slot.lock().expect("session slot lock").take();
                        backbone_rx_slot.lock().expect("backbone lock").replace(backbone_rx);
                        return;
                    }
                }
            }
            _ = &mut cancel_rx => {
                // Local disconnect — return backbone receiver and exit
                backbone_rx_slot.lock().expect("backbone lock").replace(backbone_rx);
                return;
            }
        }
    }
}

#[async_trait]
impl PeerTransport for ChannelTransport {
    async fn connect(&mut self) -> Result<(), String> {
        // Check status first — return error immediately if already connected
        // (before awaiting task_handle, which would block on a live task)
        {
            let status = self.status.lock().expect("status lock");
            if *status != PeerConnectionStatus::Disconnected {
                return Err(format!("cannot connect: status is {:?}", *status));
            }
        }

        // Await any previous forwarding task to ensure backbone_rx is returned
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }
        self.cancel_tx.take();

        // Take backbone_rx, drain stale envelopes, put it back
        let mut backbone_rx =
            self.backbone_rx.lock().expect("backbone lock").take().ok_or("cannot connect: backbone receiver unavailable")?;
        while backbone_rx.try_recv().is_ok() {}
        self.backbone_rx.lock().expect("backbone lock").replace(backbone_rx);

        // Create fresh session channel
        let (session_tx, session_rx) = mpsc::channel(CHANNEL_BUFFER);
        self.local_session_tx_slot.lock().expect("session slot lock").replace(session_tx);
        self.session_rx = Some(session_rx);

        // Notify remote side
        let _ = self.backbone_tx.send(ChannelEnvelope::Connected).await;

        // Transition through Connecting to match SshTransport's status lifecycle
        let mut status = self.status.lock().expect("status lock");
        *status = PeerConnectionStatus::Connecting;
        *status = PeerConnectionStatus::Connected;

        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        // No-op if already fully disconnected
        let is_active = self.cancel_tx.is_some()
            || self.task_handle.is_some()
            || self.local_session_tx_slot.lock().expect("session slot lock").is_some()
            || self.session_rx.is_some();
        if !is_active && self.status() == PeerConnectionStatus::Disconnected {
            return Ok(());
        }

        // Notify remote side (best-effort, non-blocking to avoid deadlock
        // if the backbone buffer is full)
        let _ = self.backbone_tx.try_send(ChannelEnvelope::Disconnected);

        // Signal forwarding task to exit (if running)
        self.cancel_tx.take();

        // Await task completion — ensures backbone_rx is returned
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }

        // Drop session
        self.local_session_tx_slot.lock().expect("session slot lock").take();
        self.session_rx.take();

        // Update status
        *self.status.lock().expect("status lock") = PeerConnectionStatus::Disconnected;

        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.lock().expect("status lock").clone()
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        {
            let status = self.status.lock().expect("status lock");
            if *status != PeerConnectionStatus::Connected {
                return Err(format!("cannot subscribe: status is {:?}", *status));
            }
        }

        let session_rx = self.session_rx.take().ok_or_else(|| "already subscribed (receiver already taken)".to_string())?;

        // Take backbone_rx for the forwarding/monitoring task
        let backbone_rx =
            self.backbone_rx.lock().expect("backbone lock").take().ok_or("cannot subscribe: backbone receiver unavailable")?;

        // Create cancellation channel
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancel_tx = Some(cancel_tx);

        // Spawn forwarding/monitoring task
        let status = Arc::clone(&self.status);
        let local_session_tx_slot = Arc::clone(&self.local_session_tx_slot);
        let backbone_rx_slot = Arc::clone(&self.backbone_rx);
        self.task_handle = Some(tokio::spawn(forwarding_task(backbone_rx, cancel_rx, status, local_session_tx_slot, backbone_rx_slot)));

        Ok(session_rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        let status = self.status.lock().expect("status lock");
        if *status != PeerConnectionStatus::Connected {
            return None;
        }
        drop(status);
        // Eagerly try to capture the remote's current session sender.
        // If the remote hasn't connected yet, resolve_session_tx() will
        // lazily resolve it on first send. Once resolved, the sender is
        // pinned to that session — after disconnect/reconnect, sends fail.
        let session_tx = self.remote_session_tx_slot.lock().expect("session slot lock").clone();
        Some(Arc::new(ChannelSender {
            retired: tokio::sync::Mutex::new(false),
            session_tx: std::sync::Mutex::new(session_tx),
            remote_session_tx_slot: Arc::clone(&self.remote_session_tx_slot),
        }) as Arc<dyn PeerSender>)
    }
}

/// Create a paired set of in-process transports. A's outbound backbone is B's
/// inbound backbone and vice versa. Both start in `Disconnected` state.
pub fn channel_transport_pair(local_name: HostName, remote_name: HostName) -> (ChannelTransport, ChannelTransport) {
    let (a_to_b_tx, a_to_b_rx) = mpsc::channel(CHANNEL_BUFFER);
    let (b_to_a_tx, b_to_a_rx) = mpsc::channel(CHANNEL_BUFFER);

    // Shared session_tx slots — each transport's slot holds the tx end of its
    // session channel. The remote's sender() clones from here for direct delivery.
    let a_session_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>> = Arc::new(std::sync::Mutex::new(None));
    let b_session_slot: Arc<std::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>> = Arc::new(std::sync::Mutex::new(None));

    let transport_a = ChannelTransport {
        local_name: local_name.clone(),
        remote_name: remote_name.clone(),
        status: Arc::new(std::sync::Mutex::new(PeerConnectionStatus::Disconnected)),
        backbone_tx: a_to_b_tx,
        backbone_rx: Arc::new(std::sync::Mutex::new(Some(b_to_a_rx))),
        local_session_tx_slot: Arc::clone(&a_session_slot),
        session_rx: None,
        remote_session_tx_slot: Arc::clone(&b_session_slot),
        cancel_tx: None,
        task_handle: None,
    };

    let transport_b = ChannelTransport {
        local_name: remote_name,
        remote_name: local_name,
        status: Arc::new(std::sync::Mutex::new(PeerConnectionStatus::Disconnected)),
        backbone_tx: b_to_a_tx,
        backbone_rx: Arc::new(std::sync::Mutex::new(Some(a_to_b_rx))),
        local_session_tx_slot: Arc::clone(&b_session_slot),
        session_rx: None,
        remote_session_tx_slot: Arc::clone(&a_session_slot),
        cancel_tx: None,
        task_handle: None,
    };

    (transport_a, transport_b)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{NodeId, PeerDataKind, PeerDataMessage, ProviderData, RepoIdentity, VectorClock};

    use super::*;

    fn test_snapshot_msg(origin: &str, seq: u64) -> PeerWireMessage {
        PeerWireMessage::Data(PeerDataMessage {
            origin_node_id: NodeId::new(origin),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            host_repo_root: Some(PathBuf::from("/repo")),
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
    async fn reconnect_after_disconnect_succeeds() {
        let (mut a, _b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect should succeed");
        a.disconnect().await.expect("disconnect should succeed");
        a.connect().await.expect("reconnect should succeed");
        assert_eq!(a.status(), PeerConnectionStatus::Connected);
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
            PeerWireMessage::Data(PeerDataMessage { origin_node_id, kind: PeerDataKind::Snapshot { seq, .. }, .. }) => {
                assert_eq!(origin_node_id, NodeId::new("alpha"));
                assert_eq!(seq, 1);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // B sends to A
        sender_b.send(test_snapshot_msg("beta", 2)).await.expect("B send");
        let msg = rx_a.recv().await.expect("A should receive message from B");
        match msg {
            PeerWireMessage::Data(PeerDataMessage { origin_node_id, kind: PeerDataKind::Snapshot { seq, .. }, .. }) => {
                assert_eq!(origin_node_id, NodeId::new("beta"));
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

    // --- Reconnection tests ---

    #[tokio::test]
    async fn reconnect_sends_and_receives() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));

        // First session
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");
        let sender_a1 = a.sender().expect("sender A");
        let mut rx_b1 = b.subscribe().await.expect("subscribe B");
        sender_a1.send(test_snapshot_msg("alpha", 1)).await.expect("send");
        let msg = rx_b1.recv().await.expect("recv");
        assert!(matches!(msg, PeerWireMessage::Data(PeerDataMessage { kind: PeerDataKind::Snapshot { seq: 1, .. }, .. })));

        // Disconnect both sides
        a.disconnect().await.expect("disconnect A");
        b.disconnect().await.expect("disconnect B");

        // Second session — reconnect and verify messaging works
        a.connect().await.expect("reconnect A");
        b.connect().await.expect("reconnect B");
        let sender_a2 = a.sender().expect("sender A after reconnect");
        let mut rx_b2 = b.subscribe().await.expect("subscribe B after reconnect");
        sender_a2.send(test_snapshot_msg("alpha", 2)).await.expect("send after reconnect");
        let msg = rx_b2.recv().await.expect("recv after reconnect");
        assert!(matches!(msg, PeerWireMessage::Data(PeerDataMessage { kind: PeerDataKind::Snapshot { seq: 2, .. }, .. })));
    }

    #[tokio::test]
    async fn remote_disconnect_closes_local_receiver() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let _rx_a = a.subscribe().await.expect("subscribe A");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // A disconnects — B's forwarding task should see Disconnected and close the session
        a.disconnect().await.expect("disconnect A");

        // B's receiver should yield None
        let msg = rx_b.recv().await;
        assert!(msg.is_none(), "B's receiver should close after A disconnects");
    }

    #[tokio::test]
    async fn remote_disconnect_transitions_status() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let _rx_a = a.subscribe().await.expect("subscribe A");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // A disconnects
        a.disconnect().await.expect("disconnect A");

        // Drain B's receiver to ensure the forwarding task has processed the Disconnected envelope
        let _ = rx_b.recv().await;

        // B's status should now be Disconnected
        assert_eq!(b.status(), PeerConnectionStatus::Disconnected, "B should transition to Disconnected after A disconnects");
    }

    #[tokio::test]
    async fn reconnect_after_remote_disconnect() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));
        a.connect().await.expect("connect A");
        b.connect().await.expect("connect B");

        let _rx_a = a.subscribe().await.expect("subscribe A");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        // A disconnects — B detects
        a.disconnect().await.expect("disconnect A");
        let _ = rx_b.recv().await; // drain to trigger status transition
        assert_eq!(b.status(), PeerConnectionStatus::Disconnected);

        // Both sides reconnect
        a.connect().await.expect("reconnect A");
        b.connect().await.expect("reconnect B");

        // Bidirectional messaging works in the new session
        let sender_a = a.sender().expect("sender A");
        let sender_b = b.sender().expect("sender B");
        let mut rx_a = a.subscribe().await.expect("subscribe A");
        let mut rx_b = b.subscribe().await.expect("subscribe B");

        sender_a.send(test_snapshot_msg("alpha", 10)).await.expect("send A→B");
        sender_b.send(test_snapshot_msg("beta", 20)).await.expect("send B→A");

        let msg_at_b = rx_b.recv().await.expect("B recv");
        assert!(matches!(msg_at_b, PeerWireMessage::Data(PeerDataMessage { kind: PeerDataKind::Snapshot { seq: 10, .. }, .. })));

        let msg_at_a = rx_a.recv().await.expect("A recv");
        assert!(matches!(msg_at_a, PeerWireMessage::Data(PeerDataMessage { kind: PeerDataKind::Snapshot { seq: 20, .. }, .. })));
    }

    #[tokio::test]
    async fn multiple_reconnect_cycles() {
        let (mut a, mut b) = channel_transport_pair(HostName::new("alpha"), HostName::new("beta"));

        for cycle in 0..3u64 {
            a.connect().await.unwrap_or_else(|e| panic!("connect A cycle {cycle}: {e}"));
            b.connect().await.unwrap_or_else(|e| panic!("connect B cycle {cycle}: {e}"));

            let sender = a.sender().expect("sender A");
            let mut rx = b.subscribe().await.expect("subscribe B");

            let seq = cycle * 10 + 1;
            sender.send(test_snapshot_msg("alpha", seq)).await.expect("send");
            let msg = rx.recv().await.expect("recv");
            assert!(matches!(msg, PeerWireMessage::Data(PeerDataMessage { kind: PeerDataKind::Snapshot { seq: s, .. }, .. }) if s == seq));

            a.disconnect().await.unwrap_or_else(|e| panic!("disconnect A cycle {cycle}: {e}"));
            b.disconnect().await.unwrap_or_else(|e| panic!("disconnect B cycle {cycle}: {e}"));
        }
    }
}
