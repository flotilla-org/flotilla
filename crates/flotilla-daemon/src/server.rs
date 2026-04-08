mod client_connection;
pub mod environment_sockets;
mod peer_connection;
mod peer_runtime;
mod remote_commands;
mod request_dispatch;
mod shared;
#[cfg(feature = "test-support")]
pub mod test_support;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use flotilla_core::{
    agents::SharedAgentStateStore, config::ConfigStore, in_process::InProcessDaemon, providers::discovery::DiscoveryRuntime,
};
use flotilla_protocol::{ConfigLabel, ConnectionRole, EnvironmentId, HostName, Message, NodeId, PROTOCOL_VERSION};
use flotilla_transport::message::{unix_message_session, MessageSession};
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
};
use tracing::{error, info, warn};

use self::{
    client_connection::ClientConnection,
    environment_sockets::EnvironmentSocketRegistry,
    peer_connection::PeerConnection,
    peer_runtime::PeerRuntime,
    remote_commands::{ForwardedCommandMap, PendingRemoteCancelMap, PendingRemoteCommandMap, RemoteCommandRouter},
    shared::{sync_peer_query_state, SocketPeerSender},
};
use crate::peer::{ConnectionDirection, ConnectionMeta, InboundPeerEnvelope, PeerManager, SshTransport};

/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
///
/// Visibility is promoted to `pub` with the `test-support` feature so
/// integration tests can construct notices to drive the outbound task.
#[cfg_attr(feature = "test-support", visibility::make(pub))]
pub(crate) struct PeerConnectedNotice {
    pub peer: NodeId,
    pub generation: u64,
}

fn build_remote_command_router(daemon: &Arc<InProcessDaemon>, peer_manager: &Arc<Mutex<PeerManager>>) -> RemoteCommandRouter {
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    RemoteCommandRouter::new(
        Arc::clone(daemon),
        Arc::clone(peer_manager),
        pending_remote_commands,
        forwarded_commands,
        pending_remote_cancels,
        Arc::new(AtomicU64::new(1 << 62)),
    )
}

fn build_peer_manager(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Result<Arc<Mutex<PeerManager>>, String> {
    let host_name = daemon.host_name().clone();
    let local_node_id = daemon.node_id().clone();
    let hosts_config = config.load_hosts()?;

    let peer_count = hosts_config.hosts.len();
    let mut peer_manager = PeerManager::new(local_node_id.clone());
    for (name, host_config) in hosts_config.hosts {
        let peer_node_id = NodeId::new(&host_config.expected_host_name);
        if peer_node_id.as_str() == host_name.as_str() {
            warn!(
                host = %host_name,
                "peer config uses same name as local host — messages will be ignored"
            );
        }
        match SshTransport::new(
            local_node_id.clone(),
            ConfigLabel(name.clone()),
            host_config,
            daemon.session_id(),
            config.state_dir().as_path(),
        ) {
            Ok(transport) => {
                peer_manager.add_peer(peer_node_id, Box::new(transport));
            }
            Err(e) => {
                warn!(host = %name, err = %e, "skipping peer with invalid host name");
            }
        }
    }

    info!(host = %host_name, %peer_count, "initialized PeerManager");

    Ok(Arc::new(Mutex::new(peer_manager)))
}

pub fn spawn_embedded_peer_networking(daemon: Arc<InProcessDaemon>, config: &ConfigStore) -> Result<tokio::task::JoinHandle<()>, String> {
    let peer_manager = build_peer_manager(&daemon, config)?;
    {
        let daemon = Arc::clone(&daemon);
        let peer_manager = Arc::clone(&peer_manager);
        tokio::spawn(async move {
            sync_peer_query_state(&peer_manager, &daemon).await;
        });
    }
    let (peer_data_tx, peer_data_rx) = mpsc::channel(256);
    let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
    let (handle, _peer_connected_tx) =
        spawn_peer_networking_runtime(daemon, peer_manager, Some(peer_data_rx), peer_data_tx, remote_command_router);
    Ok(handle)
}

/// Spawn the peer networking runtime with pre-built components.
///
/// Test-only entry point: callers provide a PeerManager with pre-configured
/// senders (e.g. CapturePeerSender). Passes `None` for `peer_data_rx` to skip
/// the inbound connection task — tests drive the outbound task via the returned
/// `PeerConnectedNotice` sender.
#[cfg(feature = "test-support")]
pub fn spawn_test_peer_networking(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
    // Receiver dropped intentionally — None is passed for the inbound task,
    // so no messages are forwarded; the sender satisfies the runtime signature.
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(256);
    let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
    spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        None, // No inbound task — test drives outbound via PeerConnectedNotice
        peer_data_tx,
        remote_command_router,
    )
}

/// The daemon server that listens on a Unix socket and dispatches requests
/// to an `InProcessDaemon`.
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    follower: bool,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Channel for inbound peer wire messages tagged with connection authority.
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    /// Manages connections to remote peer hosts and stores their provider data.
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    agent_state_store: SharedAgentStateStore,
    /// Registry of per-environment Unix sockets. Initialized on startup and
    /// populated when environments are created (wired in Phase D).
    pub environment_sockets: Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `config` — daemon configuration store, used for hostname and peer config.
    /// `discovery` — discovery runtime used to initialize tracked repos.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last active connection disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let daemon_config = config.load_daemon_config()?;
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
        let daemon = InProcessDaemon::new(repo_paths, Arc::clone(&config), discovery, host_name.clone()).await;
        let peer_manager = build_peer_manager(&daemon, &config)?;
        sync_peer_query_state(&peer_manager, &daemon).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);

        let agent_state_store = Arc::clone(daemon.agent_state_store());
        let remote_command_router = build_remote_command_router(&daemon, &peer_manager);

        Ok(Self {
            daemon,
            socket_path,
            idle_timeout,
            follower: daemon_config.follower,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_tx,
            shutdown_rx,
            peer_data_tx,
            peer_data_rx: Some(peer_data_rx),
            peer_manager,
            remote_command_router,
            agent_state_store,
            environment_sockets: Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        })
    }

    /// Take the receiver for inbound peer data messages.
    ///
    /// Returns `Some` on the first call, `None` thereafter. The PeerManager
    /// consumes this to process data arriving from peer daemons.
    pub fn take_peer_data_rx(&mut self) -> Option<mpsc::Receiver<InboundPeerEnvelope>> {
        self.peer_data_rx.take()
    }

    /// Run the server, accepting connections until idle timeout or shutdown signal.
    pub async fn run(mut self) -> Result<(), String> {
        // Clean up stale socket file before binding
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).map_err(|e| format!("failed to remove stale socket: {e}"))?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create socket directory: {e}"))?;
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| format!("failed to bind socket: {e}"))?;

        info!(path = %self.socket_path.display(), "daemon listening");

        // Tell the InProcessDaemon where the socket is so terminal sessions
        // can get FLOTILLA_DAEMON_SOCKET injected.
        self.daemon.set_daemon_socket_path(self.socket_path.clone()).await;

        // Take peer_data_rx before destructuring self
        let peer_data_rx = self.take_peer_data_rx();

        let daemon = self.daemon;
        let client_count = self.client_count;
        let shutdown_tx = self.shutdown_tx;
        let mut shutdown_rx = self.shutdown_rx;
        let idle_timeout = self.idle_timeout;
        let socket_path = self.socket_path.clone();
        let client_notify = self.client_notify;
        let peer_data_tx = self.peer_data_tx;
        let agent_state_store = self.agent_state_store;
        let remote_command_router = self.remote_command_router;

        // Spawn idle timeout watcher (disabled for follower-mode daemons
        // which serve peer connections and should stay up indefinitely)
        if !self.follower {
            let idle_client_count = Arc::clone(&client_count);
            let idle_shutdown_tx = shutdown_tx.clone();
            let idle_notify = Arc::clone(&client_notify);
            tokio::spawn(async move {
                loop {
                    // Wait until zero active connections.
                    loop {
                        if idle_client_count.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        idle_notify.notified().await;
                    }

                    info!(timeout_secs = idle_timeout.as_secs(), "no active connections, waiting before shutdown");

                    // Race: timeout vs client count change
                    tokio::select! {
                        () = tokio::time::sleep(idle_timeout) => {
                            if idle_client_count.load(Ordering::SeqCst) == 0 {
                                info!("idle timeout reached, shutting down");
                                let _ = idle_shutdown_tx.send(true);
                                return;
                            }
                            // Client connected during the sleep — loop back
                        }
                        () = idle_notify.notified() => {
                            // Client count changed — loop back to re-check
                        }
                    }
                }
            });
        } else {
            info!("follower mode: idle timeout disabled");
        }

        let peer_manager = self.peer_manager;
        let (_peer_runtime_handle, peer_connected_tx) = spawn_peer_networking_runtime(
            Arc::clone(&daemon),
            Arc::clone(&peer_manager),
            peer_data_rx,
            peer_data_tx.clone(),
            remote_command_router.clone(),
        );

        // SIGTERM handler
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to register SIGTERM handler");

        // Accept loop
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();
                            let peer_data_tx = peer_data_tx.clone();
                            let peer_manager = Arc::clone(&peer_manager);
                            let remote_command_router = remote_command_router.clone();
                            let peer_connected_tx = peer_connected_tx.clone();
                            let agent_state_store = Arc::clone(&agent_state_store);

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_manager,
                                    remote_command_router,
                                    client_count,
                                    client_notify,
                                    peer_connected_tx,
                                    agent_state_store,
                                    None,
                                )
                                .await;
                            });
                        }
                        Err(e) => {
                            error!(err = %e, "failed to accept connection");
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("shutdown signal received");
                        break;
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("received SIGINT — shutting down");
                    break;
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM — shutting down");
                    break;
                }
            }
        }

        // Clean up socket file on shutdown
        if let Err(e) = std::fs::remove_file(&socket_path) {
            warn!(err = %e, "failed to remove socket file on shutdown");
        }

        info!("daemon server stopped");
        Ok(())
    }
}

fn spawn_peer_networking_runtime(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    remote_command_router: RemoteCommandRouter,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
    PeerRuntime::new(daemon, peer_manager, peer_data_rx, peer_data_tx, remote_command_router).spawn()
}

/// Handle a single client connection.
///
/// `environment_context` — when `Some(id)`, this connection was accepted on a
/// per-environment socket; if the Hello message carries a mismatched
/// `environment_id` the connection is dropped.  `None` means the main socket
/// (forward-compatible with HTTP transport).
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    handle_client_session(
        unix_message_session(stream),
        daemon,
        shutdown_rx,
        peer_data_tx,
        peer_manager,
        remote_command_router,
        client_count,
        client_notify,
        peer_connected_tx,
        agent_state_store,
        environment_context,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_session(
    session: MessageSession,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    let session = Arc::new(session);
    let first_msg = tokio::select! {
        message_result = session.read() => {
            match message_result {
                Ok(msg) => msg,
                Err(e) => {
                    error!(err = %e, "error reading first message from client");
                    None
                }
            }
        }
        _ = shutdown_rx.changed() => None,
    };

    let Some(first_msg) = first_msg else {
        return;
    };

    match first_msg {
        Message::Request { id, request } => {
            ClientConnection::new(daemon, shutdown_rx, remote_command_router, client_count, client_notify, agent_state_store)
                .run(Arc::clone(&session), id, request)
                .await;
        }
        Message::Hello { protocol_version, node_id, display_name: _, session_id, connection_role } => {
            if environment_context.is_some() {
                warn!("peer/client hello on per-environment socket is unsupported");
                return;
            }

            if connection_role == Some(ConnectionRole::Client) {
                // Stateful client handshake: reply with server Hello, then enter
                // stateful client loop (session_id used for cursor ownership).
                if session
                    .write(Message::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        node_id: daemon.node_id().clone(),
                        display_name: daemon.host_name().to_string(),
                        session_id: daemon.session_id(),
                        connection_role: Some(ConnectionRole::Client),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                ClientConnection::new(daemon, shutdown_rx, remote_command_router, client_count, client_notify, agent_state_store)
                    .run_stateful(Arc::clone(&session), session_id)
                    .await;
            } else {
                // Peer path (ConnectionRole::Peer or None) — existing behavior.
                PeerConnection::new(daemon, shutdown_rx, peer_data_tx, peer_manager, peer_connected_tx, client_count, client_notify)
                    .run(session, protocol_version, node_id, session_id)
                    .await;
            }
        }
        other => {
            warn!(msg = ?other, "unexpected first message type from client");
        }
    }
}

#[cfg(test)]
mod tests;
