//! Narrow ports the replication engine needs from its host.
//!
//! The engine writes replicas into the host's local stores, filters relayed
//! records by the host's node identity, reports per-kind health, and reads
//! peers through one of two peer sources: a direct peer store, or a routed
//! resource watch supplied by the host's command transport.

use std::{path::Path, sync::Arc};

use flotilla_protocol::{CommandValue, NodeId, ResourceReadEnvelope};
use flotilla_resources::{DigestQuery, PartitionDigest, ReplicaCursor};
use flotilla_store::{load_metrics::ReplicationLoadMetrics, ResourceBackend};

use crate::ReplicationStore;

/// The local side of replication: node identity, target stores and health.
#[derive(Clone)]
pub struct ReplicationHost {
    node_id: NodeId,
    stores: LocalStores,
    health: Arc<dyn ReplicationHealth>,
    metrics: Arc<ReplicationLoadMetrics>,
}

impl ReplicationHost {
    pub fn new(node_id: NodeId, stores: LocalStores, health: Arc<dyn ReplicationHealth>) -> Self {
        Self { node_id, stores, health, metrics: Arc::default() }
    }

    /// Record snapshots, lag resets, stream failures and applied events into
    /// `metrics`, shared with the host for load measurement (#3206).
    pub fn with_metrics(mut self, metrics: Arc<ReplicationLoadMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn metrics(&self) -> &ReplicationLoadMetrics {
        &self.metrics
    }

    /// Records whose origin root is this node are never written back locally.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub fn stores(&self) -> &LocalStores {
        &self.stores
    }

    pub fn health(&self) -> &dyn ReplicationHealth {
        &*self.health
    }
}

/// Local target stores that receive replicas.
///
/// Durable desired state and the observed read view replicate separately:
/// see [`ReplicationStore`].
#[derive(Clone, Debug)]
pub struct LocalStores {
    pub durable: ResourceBackend,
    pub observed: ResourceBackend,
}

impl LocalStores {
    pub fn backend(&self, store: ReplicationStore) -> &ResourceBackend {
        match store {
            ReplicationStore::Durable => &self.durable,
            ReplicationStore::Observed => &self.observed,
        }
    }
}

/// Per-peer, per-kind replication health reported to the host.
///
/// `kind` is the health key: the resource kind, prefixed with `observed/` for
/// observed-store replication.
#[async_trait::async_trait]
pub trait ReplicationHealth: Send + Sync {
    /// A new peer generation started; earlier failures for the peer are stale.
    async fn begin(&self, peer: &NodeId);
    /// A kind's origin watch is established.
    async fn healthy(&self, peer: &NodeId, kind: &str);
    /// A kind's replication attempt failed and will be retried.
    async fn failure(&self, peer: &NodeId, kind: &str, message: &str);
}

/// Peer sources available to the supervisors.
#[derive(Clone)]
pub struct PeerSources {
    pub direct: Arc<dyn DirectPeerConnector>,
    pub routed: Arc<dyn RoutedPeer>,
}

/// Opens a direct peer: a [`flotilla_store::Store`] served at a forwarded
/// resource socket.
///
/// Opening is called after each retry backoff with the latest socket path, so
/// it should be cheap and must not cache a dead connection.
pub trait DirectPeerConnector: Send + Sync {
    fn connect(&self, socket: &Path, store: ReplicationStore) -> Result<ResourceBackend, String>;
}

/// A routed peer reached through the host's command transport, used when no
/// direct peer store is available.
#[async_trait::async_trait]
pub trait RoutedPeer: Send + Sync {
    /// Start a resource watch at `peer`. Implementations must subscribe to the
    /// watch's responses before dispatching it, so no response is missed.
    async fn watch(&self, peer: &NodeId, request: RoutedWatchRequest) -> Result<Box<dyn RoutedWatch>, String>;

    /// Query the peer's authoritative partition digest.
    async fn digest(&self, peer: &NodeId, request: RoutedDigestRequest) -> Result<PartitionDigest, String>;
}

/// A routed `ResourceWatch` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedWatchRequest {
    pub namespace: String,
    /// Store-qualified kind path, for example `convoys` or `observed/checkouts`.
    pub kind: String,
    /// Stream the peer's replica sources (relay) instead of its local log.
    pub replica_sources: bool,
    /// Resume after this replica prefix; `None` starts with a snapshot.
    pub cursor: Option<ReplicaCursor>,
}

/// A routed `QueryResourceDigest` request.
#[derive(Clone, Debug)]
pub struct RoutedDigestRequest {
    pub namespace: String,
    /// Store-qualified kind path, as in [`RoutedWatchRequest::kind`].
    pub kind: String,
    pub query: DigestQuery,
}

/// One item from a routed watch.
#[derive(Debug)]
pub enum RoutedWatchItem {
    /// A resource watch response for this watch.
    Response(Box<ResourceReadEnvelope>),
    /// The remote command finished with this result.
    Finished(Box<CommandValue>),
    /// The transport dropped this many events; the watch cannot continue.
    Lagged(u64),
    /// The transport's event stream closed.
    Closed,
}

/// A live routed watch. [`RoutedWatch::next`] must be cancel-safe: the engine
/// polls it inside `select!` beside digest rounds.
#[async_trait::async_trait]
pub trait RoutedWatch: Send {
    async fn next(&mut self) -> RoutedWatchItem;
    /// Ask the peer to stop the watch. Errors are ignored by the engine.
    async fn cancel(&self);
}
