use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{GoodbyeReason, HostName, Message, PeerConnectionState, PeerWireMessage, PROTOCOL_VERSION};
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{error, info, warn};

use super::{
    peer_runtime::disconnect_peer_and_rebuild,
    shared::{write_message, ConnectionLines, ConnectionWriter},
    sync_peer_query_state, ConnectionDirection, ConnectionMeta, PeerConnectedNotice, SocketPeerSender,
};
use crate::peer::{ActivationResult, InboundPeerEnvelope, PeerManager};

pub(super) struct PeerConnection {
    daemon: Arc<InProcessDaemon>,
    shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<tokio::sync::Notify>,
}

impl PeerConnection {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        shutdown_rx: watch::Receiver<bool>,
        peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
        peer_manager: Arc<Mutex<PeerManager>>,
        peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
        client_count: Arc<AtomicUsize>,
        client_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self { daemon, shutdown_rx, peer_data_tx, peer_manager, peer_connected_tx, client_count, client_notify }
    }

    pub(super) async fn run(
        mut self,
        mut lines: ConnectionLines,
        writer: ConnectionWriter,
        protocol_version: u32,
        host_name: HostName,
        session_id: uuid::Uuid,
    ) {
        if protocol_version != PROTOCOL_VERSION {
            warn!(
                peer = %host_name,
                expected = PROTOCOL_VERSION,
                got = protocol_version,
                "peer protocol version mismatch"
            );
            return;
        }

        if write_message(&writer, &Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            host_name: self.daemon.host_name().clone(),
            session_id: self.daemon.session_id(),
        })
        .await
        .is_err()
        {
            return;
        }

        let remote_session_id = Some(session_id);

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);
        let relay_writer = Arc::clone(&writer);
        let relay_task = tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if write_message(&relay_writer, &msg).await.is_err() {
                    break;
                }
            }
        });

        let (generation, displaced_generation) = {
            let mut pm = self.peer_manager.lock().await;
            match pm.activate_connection_with_session(
                host_name.clone(),
                Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(Some(outbound_tx.clone())) }),
                ConnectionMeta { direction: ConnectionDirection::Inbound, config_label: None, expected_peer: None, config_backed: false },
                remote_session_id,
            ) {
                ActivationResult::Accepted { generation, displaced } => (generation, displaced),
                ActivationResult::Rejected { reason } => {
                    let _ = write_message(&writer, &Message::Peer(Box::new(PeerWireMessage::Goodbye { reason }))).await;
                    relay_task.abort();
                    return;
                }
            }
        };
        if let Some(displaced_generation) = displaced_generation {
            let displaced = {
                let mut pm = self.peer_manager.lock().await;
                pm.take_displaced_sender(&host_name, displaced_generation)
            };
            if let Some(displaced) = displaced {
                let _ = displaced.retire(GoodbyeReason::Superseded).await;
            }
        }
        let count = self.client_count.fetch_add(1, Ordering::SeqCst) + 1;
        info!(peer = %host_name, %count, "peer connected");
        self.client_notify.notify_one();

        sync_peer_query_state(&self.peer_manager, &self.daemon).await;
        self.daemon.publish_peer_connection_status(&host_name, PeerConnectionState::Connected).await;
        let _ = self.peer_connected_tx.send(PeerConnectedNotice { peer: host_name.clone(), generation });

        loop {
            tokio::select! {
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            let msg: Message = match serde_json::from_str(&line) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(peer = %host_name, err = %e, "failed to parse peer message");
                                    break;
                                }
                            };
                            match msg {
                                Message::Peer(peer_msg) => {
                                    if let Err(e) = self.peer_data_tx.send(InboundPeerEnvelope {
                                        msg: *peer_msg,
                                        connection_generation: generation,
                                        connection_peer: host_name.clone(),
                                    }).await {
                                        warn!(peer = %host_name, err = %e, "failed to forward inbound peer message");
                                        break;
                                    }
                                }
                                other => {
                                    warn!(peer = %host_name, msg = ?other, "unexpected message type from peer");
                                    break;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            error!(peer = %host_name, err = %e, "error reading from peer");
                            break;
                        }
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }

        let plan = disconnect_peer_and_rebuild(&self.peer_manager, &self.daemon, &host_name, generation).await;
        if plan.was_active {
            self.daemon.publish_peer_connection_status(&host_name, PeerConnectionState::Disconnected).await;
        }
        relay_task.abort();
        let count = self.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        info!(peer = %host_name, %count, "peer disconnected");
        self.client_notify.notify_one();
    }
}
