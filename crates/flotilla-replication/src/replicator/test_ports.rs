//! In-memory port implementations for engine tests.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_protocol::NodeId;
use flotilla_resources::PartitionDigest;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

use super::{PeerReplicatorSupervisors, ReplicationOptions, ReplicationStore};
use crate::ports::{
    DirectPeerConnector, LocalStores, PeerSources, ReplicationHealth, ReplicationHost, RoutedDigestRequest, RoutedPeer, RoutedWatch,
    RoutedWatchRequest,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HealthReport {
    Begin(NodeId),
    Healthy(NodeId, String),
    Failure(NodeId, String, String),
}

#[derive(Default)]
pub(super) struct RecordingHealth {
    pub(super) reports: Mutex<Vec<HealthReport>>,
}

impl RecordingHealth {
    pub(super) fn reports(&self) -> Vec<HealthReport> {
        self.reports.lock().expect("health reports").clone()
    }
}

#[async_trait::async_trait]
impl ReplicationHealth for RecordingHealth {
    async fn begin(&self, peer: &NodeId) {
        self.reports.lock().expect("health reports").push(HealthReport::Begin(peer.clone()));
    }
    async fn healthy(&self, peer: &NodeId, kind: &str) {
        self.reports.lock().expect("health reports").push(HealthReport::Healthy(peer.clone(), kind.to_string()));
    }
    async fn failure(&self, peer: &NodeId, kind: &str, message: &str) {
        self.reports.lock().expect("health reports").push(HealthReport::Failure(peer.clone(), kind.to_string(), message.to_string()));
    }
}

/// Direct peers keyed by socket path, each serving durable and observed stores.
#[derive(Default)]
pub(super) struct InMemoryPeers {
    peers: Mutex<Vec<(PathBuf, LocalStores)>>,
    pub(super) connections: Mutex<Vec<(PathBuf, ReplicationStore)>>,
}

impl InMemoryPeers {
    pub(super) fn serve(&self, socket: impl Into<PathBuf>, stores: LocalStores) {
        self.peers.lock().expect("peers").push((socket.into(), stores));
    }
}

impl DirectPeerConnector for InMemoryPeers {
    fn connect(&self, socket: &Path, store: ReplicationStore) -> Result<ResourceBackend, String> {
        self.connections.lock().expect("connections").push((socket.to_path_buf(), store));
        let peers = self.peers.lock().expect("peers");
        let (_, stores) = peers.iter().find(|(path, _)| path == socket).ok_or_else(|| format!("no peer at {}", socket.display()))?;
        Ok(stores.backend(store).clone())
    }
}

/// A routed peer for tests that must not reach the routed transport.
pub(super) struct NoRoutedPeer;

#[async_trait::async_trait]
impl RoutedPeer for NoRoutedPeer {
    async fn watch(&self, _peer: &NodeId, _request: RoutedWatchRequest) -> Result<Box<dyn RoutedWatch>, String> {
        Err("routed transport unavailable in this test".to_string())
    }
    async fn digest(&self, _peer: &NodeId, _request: RoutedDigestRequest) -> Result<PartitionDigest, String> {
        Err("routed transport unavailable in this test".to_string())
    }
}

pub(super) fn stores(root: &NodeId) -> LocalStores {
    LocalStores {
        durable: ResourceBackend::new(InMemoryBackend::default()).with_local_root(root.clone()),
        observed: ResourceBackend::new(InMemoryBackend::default()).with_local_root(root.clone()),
    }
}

pub(super) fn host(node: &str, health: Arc<RecordingHealth>) -> ReplicationHost {
    let node_id = NodeId::new(node);
    ReplicationHost::new(node_id.clone(), stores(&node_id), health)
}

/// Supervisors whose replicators are never started by the calling test.
pub(super) fn supervisors() -> PeerReplicatorSupervisors {
    PeerReplicatorSupervisors::new(
        host("holder", Arc::new(RecordingHealth::default())),
        PeerSources { direct: Arc::new(InMemoryPeers::default()), routed: Arc::new(NoRoutedPeer) },
        ReplicationOptions::default(),
    )
}
