//! Store-to-store resource replication (ADR 0060).
//!
//! The engine replicates each registered resource kind from a connected peer
//! into the host's local stores: it lists, watches and resumes the peer's
//! origin log, relays third-origin replicas the peer holds, and repairs drift
//! with a periodic digest safety net. Records whose origin root is the local
//! node or the peer are never relayed.
//!
//! Hosts supply the narrow ports in [`ports`]: local target stores and node
//! identity ([`ReplicationHost`]), health reporting ([`ReplicationHealth`]),
//! and peer sources ([`PeerSources`]). A direct peer is any
//! [`flotilla_store::Store`]; a routed peer is a resource watch carried by the
//! host's command transport.
pub mod ports;
mod replicator;

pub use ports::{
    DirectPeerConnector, LocalStores, PeerSources, ReplicationHealth, ReplicationHost, RoutedDigestRequest, RoutedPeer, RoutedWatch,
    RoutedWatchItem, RoutedWatchRequest,
};
pub use replicator::{
    replicate_kind_direct, replicate_relay_direct, DigestControl, DigestScheduler, PeerReplicatorSupervisors, ReplicationOptions,
    ReplicationStore,
};
