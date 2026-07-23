use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{GoodbyeReason, NodeInfo, PeerWireMessage};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

#[async_trait]
pub trait PeerSender: Send + Sync {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String>;
    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String>;
}

#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn status(&self) -> PeerConnectionStatus;

    /// Human-readable address used for connection attempts and diagnostics.
    fn connection_address(&self) -> String;

    /// A local Unix socket forwarded to the remote daemon, when this
    /// transport provides one.
    fn resource_socket_path(&self) -> Option<PathBuf> {
        None
    }

    /// Subscribe to inbound peer wire messages.
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String>;

    /// Return a sender for outbound peer messages when the transport is connected.
    fn sender(&self) -> Option<Arc<dyn PeerSender>>;

    /// Return the session ID received during the last handshake, if any.
    fn remote_session_id(&self) -> Option<uuid::Uuid> {
        None
    }

    /// Return the remote node identity learned during the last handshake, if any.
    fn remote_node_info(&self) -> Option<NodeInfo> {
        None
    }
}
