use async_trait::async_trait;
use std::sync::Arc;

use flotilla_protocol::{PeerDataMessage, PeerWireMessage};
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
}

#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn status(&self) -> PeerConnectionStatus;

    /// Subscribe to inbound peer data messages.
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String>;

    /// Return a sender for outbound peer messages when the transport is connected.
    fn sender(&self) -> Option<Arc<dyn PeerSender>>;
}
