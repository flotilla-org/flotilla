use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::{ConfigLabel, HostName, NodeId, NodeInfo, PeerWireMessage};
use tokio::sync::mpsc;

use super::{ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope, PeerManager};
use crate::peer::{test_support::MockPeerSender, PeerConnectionStatus, PeerSender, PeerTransport};

fn activate(mgr: &mut PeerManager, peer: &str, sender: Arc<MockPeerSender>) -> u64 {
    match mgr.activate_connection(NodeId::new(peer), sender, ConnectionMeta {
        direction: ConnectionDirection::Outbound,
        config_label: None,
        expected_peer: Some(NodeId::new(peer)),
        config_backed: false,
    }) {
        ActivationResult::Accepted { generation, .. } => generation,
        ActivationResult::Rejected { reason } => panic!("connection unexpectedly rejected: {reason:?}"),
    }
}

struct HangingOnceTransport {
    attempts: Arc<AtomicUsize>,
    sender: Arc<dyn PeerSender>,
    remote: NodeInfo,
    status: PeerConnectionStatus,
}

#[async_trait]
impl PeerTransport for HangingOnceTransport {
    async fn connect(&mut self) -> Result<(), String> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            std::future::pending().await
        } else {
            self.status = PeerConnectionStatus::Connected;
            Ok(())
        }
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    fn connection_address(&self) -> String {
        format!("mock://{}", self.remote.node_id)
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        Some(Arc::clone(&self.sender))
    }

    fn remote_node_info(&self) -> Option<NodeInfo> {
        Some(self.remote.clone())
    }
}

#[tokio::test(start_paused = true)]
async fn timed_out_reconnect_can_recover_on_the_next_attempt() {
    let label = ConfigLabel("remote".into());
    let remote = NodeInfo::new(NodeId::new("remote-node"), "Remote");
    let attempts = Arc::new(AtomicUsize::new(0));
    let (sender, _) = MockPeerSender::new();
    let mut manager = PeerManager::new(NodeId::new("local"));
    manager.add_configured_target(
        label.clone(),
        HostName::new("Remote"),
        Some(remote.node_id.clone()),
        Box::new(HangingOnceTransport {
            attempts: Arc::clone(&attempts),
            sender: Arc::new(sender),
            remote: remote.clone(),
            status: PeerConnectionStatus::Disconnected,
        }),
    );

    let error = manager.reconnect_target(&label, Duration::from_secs(1)).await.expect_err("first transport dial should time out");
    assert!(error.contains("timed out"));

    let connection =
        manager.reconnect_target(&label, Duration::from_secs(1)).await.expect("next dial should recover without restarting the manager");
    assert_eq!(connection.node, remote);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn route_advertisement_learns_and_relays_ephemeral_route() {
    let mut mgr = PeerManager::new(NodeId::new("feta"));
    let (kiwi_sender, _) = MockPeerSender::new();
    let (udder_sender, udder_sent) = MockPeerSender::new();
    let kiwi_generation = activate(&mut mgr, "kiwi", Arc::new(kiwi_sender));
    activate(&mut mgr, "udder", Arc::new(udder_sender));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::RouteAdvertisement {
                origin_node_id: NodeId::new("mango"),
                origin_display_name: "Mango".into(),
                remaining_hops: 8,
                visited: vec![NodeId::new("kiwi")],
            },
            connection_generation: kiwi_generation,
            connection_peer: NodeId::new("kiwi"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.resolve_sender(&NodeId::new("mango")).is_ok(), "learned route should resolve through kiwi");

    super::dispatch_pending_sends(mgr.take_pending_sends()).await;
    assert!(matches!(
        udder_sent.lock().expect("lock").as_slice(),
        [PeerWireMessage::RouteAdvertisement { origin_node_id, visited, .. }]
            if origin_node_id == &NodeId::new("mango") && visited.contains(&NodeId::new("feta"))
    ));
}

#[tokio::test]
async fn stale_generation_route_advertisement_is_ignored() {
    let mut mgr = PeerManager::new(NodeId::new("local"));
    let (sender, _) = MockPeerSender::new();
    let generation = activate(&mut mgr, "peer", Arc::new(sender));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::RouteAdvertisement {
                origin_node_id: NodeId::new("remote"),
                origin_display_name: "Remote".into(),
                remaining_hops: 8,
                visited: vec![NodeId::new("peer")],
            },
            connection_generation: generation + 1,
            connection_peer: NodeId::new("peer"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.resolve_sender(&NodeId::new("remote")).is_err());
}

#[tokio::test]
async fn ping_queues_matching_pong() {
    let mut mgr = PeerManager::new(NodeId::new("local"));
    let (sender, sent) = MockPeerSender::new();
    let generation = activate(&mut mgr, "peer", Arc::new(sender));

    mgr.handle_inbound(InboundPeerEnvelope {
        msg: PeerWireMessage::Ping { timestamp: 42 },
        connection_generation: generation,
        connection_peer: NodeId::new("peer"),
    })
    .await;
    super::dispatch_pending_sends(mgr.take_pending_sends()).await;

    assert!(matches!(sent.lock().expect("lock").as_slice(), [PeerWireMessage::Pong { timestamp: 42 }]));
}

#[test]
fn route_advertisements_include_local_and_known_reachable_nodes() {
    let mut mgr = PeerManager::new(NodeId::new("feta"));
    let (kiwi_sender, _) = MockPeerSender::new();
    let (udder_sender, _) = MockPeerSender::new();
    activate(&mut mgr, "kiwi", Arc::new(kiwi_sender));
    activate(&mut mgr, "udder", Arc::new(udder_sender));

    let advertised: Vec<NodeId> = mgr
        .route_advertisements_for(&NodeId::new("udder"))
        .into_iter()
        .filter_map(|message| match message {
            PeerWireMessage::RouteAdvertisement { origin_node_id, .. } => Some(origin_node_id),
            _ => None,
        })
        .collect();

    assert!(advertised.contains(&NodeId::new("feta")));
    assert!(advertised.contains(&NodeId::new("kiwi")));
    assert!(!advertised.contains(&NodeId::new("udder")));
}

#[test]
fn node_info_for_advertisement_preserves_display_name() {
    let mut mgr = PeerManager::new(NodeId::new("local"));
    mgr.learned_node_names.insert(NodeId::new("remote"), "Remote host".into());
    let info: NodeInfo = mgr.node_info_for(&NodeId::new("remote"));
    assert_eq!(info.display_name, "Remote host");
}
