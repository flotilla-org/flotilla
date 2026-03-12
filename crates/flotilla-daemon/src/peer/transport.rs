use async_trait::async_trait;
use flotilla_protocol::PeerDataMessage;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn status(&self) -> PeerConnectionStatus;

    /// Subscribe to inbound peer data messages.
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String>;

    /// Send a peer data message to the remote daemon.
    /// Uses `&self` (not `&mut self`) — implementations use interior mutability
    /// (e.g. `Mutex<mpsc::Sender>`) so the PeerManager can iterate peers and send.
    async fn send(&self, msg: PeerDataMessage) -> Result<(), String>;
}
