use std::sync::Arc;

use flotilla_core::{config::ConfigStore, in_process::InProcessDaemon};
use flotilla_protocol::{ConfigLabel, DaemonEvent, HostName, PeerConnectionState};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use crate::peer::{InboundPeerEnvelope, PeerManager, SshTransport};

/// Manages peer networking lifecycle: SSH connections, inbound message
/// processing, and outbound snapshot broadcasting.
///
/// Created via `new()` which loads peer config and sets up transports.
/// Call `spawn()` to start the three background task groups. The returned
/// `Arc<Mutex<PeerManager>>` and `mpsc::Sender<InboundPeerEnvelope>` let
/// `DaemonServer` feed inbound socket-peer messages into the same pipeline.
pub struct PeerNetworkingTask {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
}

impl PeerNetworkingTask {
    /// Create a new peer networking task.
    ///
    /// Loads `hosts.toml` from config, creates a `PeerManager`, and registers
    /// SSH transports for each configured peer. Returns the task plus shared
    /// handles that `DaemonServer` needs for socket-peer integration.
    pub fn new(
        daemon: Arc<InProcessDaemon>,
        config: &ConfigStore,
    ) -> Result<(Self, Arc<Mutex<PeerManager>>, mpsc::Sender<InboundPeerEnvelope>), String> {
        let host_name = daemon.host_name().clone();
        let hosts_config = config.load_hosts()?;

        let peer_count = hosts_config.hosts.len();
        let mut peer_manager = PeerManager::new(host_name.clone());
        for (name, host_config) in hosts_config.hosts {
            let peer_host = HostName::new(&host_config.expected_host_name);
            if peer_host == host_name {
                warn!(
                    host = %host_name,
                    "peer config uses same name as local host — messages will be ignored"
                );
            }
            match SshTransport::new(host_name.clone(), ConfigLabel(name.clone()), host_config) {
                Ok(transport) => {
                    peer_manager.add_peer(peer_host, Box::new(transport));
                }
                Err(e) => {
                    warn!(host = %name, err = %e, "skipping peer with invalid host name");
                }
            }
        }

        info!(host = %host_name, %peer_count, "initialized PeerNetworkingTask");

        // Emit initial disconnected status for all configured peers
        for peer_host in peer_manager.configured_peer_names() {
            daemon.send_event(DaemonEvent::PeerStatusChanged { host: peer_host, status: PeerConnectionState::Disconnected });
        }

        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);
        let peer_manager = Arc::new(Mutex::new(peer_manager));

        Ok((
            Self {
                daemon,
                peer_manager: Arc::clone(&peer_manager),
                peer_data_tx: peer_data_tx.clone(),
                peer_data_rx: Some(peer_data_rx),
            },
            peer_manager,
            peer_data_tx,
        ))
    }
}
