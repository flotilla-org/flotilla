use std::sync::Arc;

use flotilla_protocol::{HostName, PeerDataMessage, PeerWireMessage};

use crate::peer::{
    ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope,
    PeerManager, PeerSender,
};

#[doc(hidden)]
pub fn ensure_test_connection_generation<F>(
    mgr: &mut PeerManager,
    origin: &HostName,
    mut make_sender: F,
) -> u64
where
    F: FnMut() -> Arc<dyn PeerSender>,
{
    if let Some(generation) = mgr.current_generation(origin) {
        return generation;
    }

    for direction in [ConnectionDirection::Inbound, ConnectionDirection::Outbound] {
        match mgr.activate_connection(
            origin.clone(),
            make_sender(),
            ConnectionMeta {
                direction,
                config_label: None,
                expected_peer: Some(origin.clone()),
                config_backed: false,
            },
        ) {
            ActivationResult::Accepted { generation, .. } => return generation,
            ActivationResult::Rejected { .. } => continue,
        }
    }

    panic!("expected test activation for {origin} to succeed");
}

#[doc(hidden)]
pub async fn handle_test_peer_data<F>(
    mgr: &mut PeerManager,
    msg: PeerDataMessage,
    make_sender: F,
) -> HandleResult
where
    F: FnMut() -> Arc<dyn PeerSender>,
{
    let origin = msg.origin_host.clone();
    let generation = ensure_test_connection_generation(mgr, &origin, make_sender);
    mgr.handle_inbound(InboundPeerEnvelope {
        msg: PeerWireMessage::Data(msg),
        connection_generation: generation,
        connection_peer: origin,
    })
    .await
}
