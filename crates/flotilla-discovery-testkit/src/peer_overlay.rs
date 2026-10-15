//! In-memory peer-layer adapter for capability and composition tests.
use flotilla_projections::host_registry::PeerOverlay;
use flotilla_protocol::{
    EnvironmentId, HostIdentity, HostSummary, NodeId, NodeInfo, PeerConnectionState, SystemInfo, ToolInventory, TopologyRoute,
};
use std::collections::HashMap;
use tokio::sync::RwLock;
#[derive(Default)]
pub struct HostPeerOverlay {
    node_connectivity: RwLock<HashMap<NodeId, PeerConnectionState>>,
    configured_peers: RwLock<HashMap<NodeId, String>>,
    topology_routes: RwLock<Vec<TopologyRoute>>,
    peer_identities: RwLock<HashMap<EnvironmentId, HostIdentity>>,
}
impl PeerOverlay for HostPeerOverlay {
    fn node_connectivity(&self) -> &RwLock<HashMap<NodeId, PeerConnectionState>> {
        &self.node_connectivity
    }
    fn configured_peers(&self) -> &RwLock<HashMap<NodeId, String>> {
        &self.configured_peers
    }
    fn topology_routes(&self) -> &RwLock<Vec<TopologyRoute>> {
        &self.topology_routes
    }
    fn peer_identities(&self) -> &RwLock<HashMap<EnvironmentId, HostIdentity>> {
        &self.peer_identities
    }
    fn is_configured_peer_environment_id(&self, environment_id: &EnvironmentId) -> bool {
        environment_id.provisioned_id().is_some_and(|id| id.starts_with("configured-peer:"))
    }
    fn summary_is_overlay_placeholder(&self, summary: &HostSummary) -> bool {
        summary.system == SystemInfo::default()
            && summary.inventory == ToolInventory::default()
            && summary.providers.is_empty()
            && summary.environments.is_empty()
    }
    fn connection_status_for_node(
        &self,
        local_node: &NodeInfo,
        node_connectivity: &HashMap<NodeId, PeerConnectionState>,
        node_id: &NodeId,
    ) -> PeerConnectionState {
        if *node_id == local_node.node_id {
            PeerConnectionState::Connected
        } else {
            node_connectivity.get(node_id).cloned().unwrap_or(PeerConnectionState::Disconnected)
        }
    }
}
