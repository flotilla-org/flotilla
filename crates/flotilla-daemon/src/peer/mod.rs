pub mod channel_transport;
pub mod manager;
pub mod merge;
pub mod ssh_transport;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod transport;

pub use channel_transport::{channel_transport_pair, ChannelTransport};
pub use manager::{
    synthetic_repo_path, ActivationResult, ConnectionDirection, ConnectionMeta, DisconnectPlan, HandleResult, InboundPeerEnvelope,
    OverlayUpdate, PeerManager, PendingResyncRequest, PerRepoPeerState, ReversePathHop, ReversePathKey, RouteHop, RouteState,
};
pub use merge::merge_provider_data;
pub use ssh_transport::SshTransport;
pub use transport::{PeerConnectionStatus, PeerSender, PeerTransport};

#[cfg(test)]
mod channel_tests;
