use std::sync::Arc;

use flotilla_protocol::{NodeId, NodeInfo, PeerWireMessage};

use super::{ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope, PeerManager};
use crate::peer::test_support::MockPeerSender;

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
