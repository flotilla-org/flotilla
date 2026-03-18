use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use flotilla_core::{
    agents::{AgentEntry, SharedAgentStateStore},
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::DiscoveryRuntime,
};
use flotilla_protocol::{
    Command, CommandAction, CommandPeerEvent, CommandResult, ConfigLabel, DaemonEvent, GoodbyeReason, HostName, Message,
    PeerConnectionState, PeerDataMessage, PeerWireMessage, RepoIdentity, RepoSelector, Request, Response, RoutedPeerMessage,
    PROTOCOL_VERSION,
};
use futures::future::join_all;
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    net::UnixListener,
    sync::{mpsc, oneshot, watch, Mutex, Notify},
};
use tracing::{debug, error, info, warn};

use crate::peer::{
    ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope, PeerManager, PeerSender, SshTransport,
};

struct SocketPeerSender {
    tx: tokio::sync::Mutex<Option<mpsc::Sender<Message>>>,
}

#[async_trait]
impl PeerSender for SocketPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        let tx = self.tx.lock().await.as_ref().cloned().ok_or_else(|| "socket peer outbound channel closed".to_string())?;
        tx.send(Message::Peer(Box::new(msg))).await.map_err(|_| "socket peer outbound channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        let tx = self.tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(Message::Peer(Box::new(PeerWireMessage::Goodbye { reason })))
                .await
                .map_err(|_| "socket peer outbound channel closed".to_string())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PendingRemoteCommand {
    command_id: u64,
    target_host: HostName,
    repo_identity: Option<RepoIdentity>,
    repo: Option<PathBuf>,
    finished_via_event: bool,
}

#[derive(Debug, Clone)]
struct ForwardedCommand {
    state: ForwardedCommandState,
}

#[derive(Debug, Clone)]
enum ForwardedCommandState {
    Launching { ready: Arc<Notify> },
    Running { command_id: u64 },
}

type PendingRemoteCommandMap = Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>;
type ForwardedCommandMap = Arc<Mutex<HashMap<u64, ForwardedCommand>>>;
type PendingRemoteCancelMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>;

/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
///
/// Visibility is promoted to `pub` with the `test-support` feature so
/// integration tests can construct notices to drive the outbound task.
#[cfg_attr(feature = "test-support", visibility::make(pub))]
pub(crate) struct PeerConnectedNotice {
    pub peer: HostName,
    pub generation: u64,
}

fn build_peer_manager(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Result<Arc<Mutex<PeerManager>>, String> {
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
        match SshTransport::new(host_name.clone(), ConfigLabel(name.clone()), host_config, daemon.session_id()) {
            Ok(transport) => {
                peer_manager.add_peer(peer_host, Box::new(transport));
            }
            Err(e) => {
                warn!(host = %name, err = %e, "skipping peer with invalid host name");
            }
        }
    }

    info!(host = %host_name, %peer_count, "initialized PeerManager");

    Ok(Arc::new(Mutex::new(peer_manager)))
}

async fn sync_peer_query_state(peer_manager: &Arc<Mutex<PeerManager>>, daemon: &Arc<InProcessDaemon>) {
    // Keep the PeerManager lock scoped to this snapshot read. Several call sites
    // invoke this immediately after mutating PeerManager state; holding the lock
    // across the daemon writes would deadlock if a future refactor re-entered
    // PeerManager while mirroring query state.
    let (configured, summaries, routes) = {
        let pm = peer_manager.lock().await;
        (pm.configured_peer_names(), pm.get_peer_host_summaries().clone(), pm.topology_routes())
    };

    daemon.set_configured_peer_names(configured).await;
    daemon.set_peer_host_summaries(summaries).await;
    daemon.set_topology_routes(routes).await;
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
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    let (handle, _peer_connected_tx) = spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        Some(peer_data_rx),
        peer_data_tx,
        pending_remote_commands,
        forwarded_commands,
        pending_remote_cancels,
    );
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
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        None, // No inbound task — test drives outbound via PeerConnectedNotice
        peer_data_tx,
        pending_remote_commands,
        forwarded_commands,
        pending_remote_cancels,
    )
}

struct DispatchContext<'a> {
    daemon: &'a Arc<InProcessDaemon>,
    peer_manager: &'a Arc<Mutex<PeerManager>>,
    pending_remote_commands: &'a PendingRemoteCommandMap,
    pending_remote_cancels: &'a PendingRemoteCancelMap,
    next_remote_command_id: &'a Arc<AtomicU64>,
    agent_state_store: &'a SharedAgentStateStore,
}

fn extract_command_repo_identity(command: &Command) -> Option<RepoIdentity> {
    if let Some(RepoSelector::Identity(identity)) = command.context_repo.as_ref() {
        return Some(identity.clone());
    }
    match &command.action {
        CommandAction::Checkout { repo: RepoSelector::Identity(identity), .. } => Some(identity.clone()),
        CommandAction::PrepareTerminalForCheckout { .. } => None,
        CommandAction::UntrackRepo { repo: RepoSelector::Identity(identity) } => Some(identity.clone()),
        CommandAction::Refresh { repo: Some(RepoSelector::Identity(identity)) } => Some(identity.clone()),
        _ => None,
    }
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
    pending_remote_commands: PendingRemoteCommandMap,
    forwarded_commands: ForwardedCommandMap,
    pending_remote_cancels: PendingRemoteCancelMap,
    next_remote_command_id: Arc<AtomicU64>,
    agent_state_store: SharedAgentStateStore,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `config` — daemon configuration store, used for hostname and peer config.
    /// `discovery` — discovery runtime used to initialize tracked repos.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last client disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let daemon_config = config.load_daemon_config();
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
        let daemon = InProcessDaemon::new(repo_paths, Arc::clone(&config), discovery, host_name.clone()).await;
        let peer_manager = build_peer_manager(&daemon, &config)?;
        sync_peer_query_state(&peer_manager, &daemon).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);

        let agent_state_store = Arc::clone(daemon.agent_state_store());

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
            pending_remote_commands: Arc::new(Mutex::new(HashMap::new())),
            forwarded_commands: Arc::new(Mutex::new(HashMap::new())),
            pending_remote_cancels: Arc::new(Mutex::new(HashMap::new())),
            next_remote_command_id: Arc::new(AtomicU64::new(1 << 62)),
            agent_state_store,
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
        let pending_remote_commands = self.pending_remote_commands;
        let forwarded_commands = self.forwarded_commands;
        let pending_remote_cancels = self.pending_remote_cancels;
        let next_remote_command_id = self.next_remote_command_id;
        let agent_state_store = self.agent_state_store;

        // Spawn idle timeout watcher (disabled for follower-mode daemons
        // which serve peer connections and should stay up indefinitely)
        if !self.follower {
            let idle_client_count = Arc::clone(&client_count);
            let idle_shutdown_tx = shutdown_tx.clone();
            let idle_notify = Arc::clone(&client_notify);
            tokio::spawn(async move {
                loop {
                    // Wait until zero clients
                    loop {
                        if idle_client_count.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        idle_notify.notified().await;
                    }

                    info!(timeout_secs = idle_timeout.as_secs(), "no clients connected, waiting before shutdown");

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
            Arc::clone(&pending_remote_commands),
            Arc::clone(&forwarded_commands),
            Arc::clone(&pending_remote_cancels),
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
                            let pending_remote_commands = Arc::clone(&pending_remote_commands);
                            let pending_remote_cancels = Arc::clone(&pending_remote_cancels);
                            let next_remote_command_id = Arc::clone(&next_remote_command_id);
                            let peer_connected_tx = peer_connected_tx.clone();
                            let agent_state_store = Arc::clone(&agent_state_store);

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_manager,
                                    pending_remote_commands,
                                    pending_remote_cancels,
                                    next_remote_command_id,
                                    client_count,
                                    client_notify,
                                    peer_connected_tx,
                                    agent_state_store,
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
    pending_remote_commands: PendingRemoteCommandMap,
    forwarded_commands: ForwardedCommandMap,
    pending_remote_cancels: PendingRemoteCancelMap,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
    let outbound_peer_manager = Arc::clone(&peer_manager);
    let peer_manager_task = Arc::clone(&peer_manager);
    let peer_data_tx_for_ssh = peer_data_tx.clone();
    let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();
    let peer_connected_tx_for_ssh = peer_connected_tx.clone();
    let peer_daemon = Arc::clone(&daemon);
    let pending_remote_commands_task = Arc::clone(&pending_remote_commands);
    let forwarded_commands_task = Arc::clone(&forwarded_commands);
    let pending_remote_cancels_task = Arc::clone(&pending_remote_cancels);

    let inbound_handle = tokio::spawn(async move {
        if let Some(mut rx) = peer_data_rx {
            let mut resync_sweep = tokio::time::interval(Duration::from_secs(5));
            let mut initial_rx_map: HashMap<HostName, (u64, mpsc::Receiver<PeerWireMessage>)> = HashMap::new();
            let peer_names = {
                let mut pm = peer_manager_task.lock().await;
                let names = pm.configured_peer_names();
                for (name, generation, rx) in pm.connect_all().await {
                    initial_rx_map.insert(name, (generation, rx));
                }
                names
            };

            for name in &peer_names {
                let _ = peer_daemon.publish_peer_connection_status(name, PeerConnectionState::Connecting).await;
            }
            for name in &peer_names {
                let status =
                    if initial_rx_map.contains_key(name) { PeerConnectionState::Connected } else { PeerConnectionState::Disconnected };
                let _ = peer_daemon.publish_peer_connection_status(name, status).await;
            }
            sync_peer_query_state(&peer_manager_task, &peer_daemon).await;

            for peer_name in peer_names {
                let tx = peer_data_tx_for_ssh.clone();
                let pm = Arc::clone(&peer_manager_task);
                let daemon_for_cleanup = Arc::clone(&peer_daemon);
                let initial_rx = initial_rx_map.remove(&peer_name);
                let peer_connected_tx_clone = peer_connected_tx_for_ssh.clone();

                tokio::spawn(async move {
                    let mut last_known_session_id: Option<uuid::Uuid> = None;

                    if let Some((generation, mut inbound_rx)) = initial_rx {
                        let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });
                        last_known_session_id = {
                            let pm_lock = pm.lock().await;
                            pm_lock.peer_session_id(&peer_name)
                        };
                        let sender = {
                            let pm_lock = pm.lock().await;
                            pm_lock.get_sender_if_current(&peer_name, generation)
                        };
                        let forward_result = if let Some(sender) = sender {
                            forward_with_keepalive(&tx, &mut inbound_rx, &peer_name, generation, sender).await
                        } else {
                            ForwardResult::Disconnected
                        };
                        match forward_result {
                            ForwardResult::Shutdown => return,
                            ForwardResult::Disconnected => {
                                info!(peer = %peer_name, "SSH connection dropped, will reconnect");
                            }
                            ForwardResult::KeepaliveTimeout => {
                                info!(peer = %peer_name, "keepalive timeout, forcing reconnect");
                            }
                        }
                        let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                        if plan.was_active {
                            let _ = daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Disconnected).await;
                        }
                    }

                    let mut attempt: u32 = 1;
                    loop {
                        if let Some(delay) = {
                            let mut pm = pm.lock().await;
                            pm.reconnect_suppressed_until(&peer_name).map(|deadline| deadline.saturating_duration_since(Instant::now()))
                        } {
                            info!(peer = %peer_name, delay_secs = delay.as_secs(), "reconnect suppressed after peer retirement");
                            tokio::time::sleep(delay).await;
                            attempt = 1;
                            continue;
                        }
                        let _ = daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Reconnecting).await;
                        let delay = SshTransport::backoff_delay(attempt);
                        info!(peer = %peer_name, %attempt, delay_secs = delay.as_secs(), "reconnecting after backoff");
                        tokio::time::sleep(delay).await;

                        let reconnect_result = {
                            let mut pm = pm.lock().await;
                            pm.reconnect_peer(&peer_name).await
                        };

                        match reconnect_result {
                            Ok((generation, mut inbound_rx)) => {
                                info!(peer = %peer_name, "reconnected successfully");
                                last_known_session_id =
                                    handle_remote_restart_if_needed(&pm, &daemon_for_cleanup, &peer_name, last_known_session_id).await;
                                sync_peer_query_state(&pm, &daemon_for_cleanup).await;
                                let _ = daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Connected).await;
                                let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });
                                attempt = 1;
                                let sender = {
                                    let pm_lock = pm.lock().await;
                                    pm_lock.get_sender_if_current(&peer_name, generation)
                                };
                                let forward_result = if let Some(sender) = sender {
                                    forward_with_keepalive(&tx, &mut inbound_rx, &peer_name, generation, sender).await
                                } else {
                                    ForwardResult::Disconnected
                                };
                                match forward_result {
                                    ForwardResult::Shutdown => return,
                                    ForwardResult::Disconnected => {
                                        info!(peer = %peer_name, "SSH connection dropped, will reconnect");
                                    }
                                    ForwardResult::KeepaliveTimeout => {
                                        info!(peer = %peer_name, "keepalive timeout, forcing reconnect");
                                    }
                                }
                                let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                                if plan.was_active {
                                    let _ = daemon_for_cleanup
                                        .publish_peer_connection_status(&peer_name, PeerConnectionState::Disconnected)
                                        .await;
                                }
                            }
                            Err(e) => {
                                warn!(peer = %peer_name, err = %e, %attempt, "reconnection failed");
                                attempt = attempt.saturating_add(1);
                            }
                        }
                    }
                });
            }

            let mut reply_clock = flotilla_protocol::VectorClock::default();
            loop {
                tokio::select! {
                    maybe_env = rx.recv() => {
                        let Some(env) = maybe_env else { break };
                        let (origin, repo_path) = match &env.msg {
                            PeerWireMessage::Data(msg) => (msg.origin_host.clone(), msg.repo_path.clone()),
                            PeerWireMessage::HostSummary(_) => (env.connection_peer.clone(), PathBuf::new()),
                            PeerWireMessage::Routed(
                                flotilla_protocol::RoutedPeerMessage::ResyncSnapshot { responder_host, repo_path, .. },
                            ) => (responder_host.clone(), repo_path.clone()),
                            PeerWireMessage::Routed(_) => (env.connection_peer.clone(), PathBuf::new()),
                            PeerWireMessage::Goodbye { .. } | PeerWireMessage::Ping { .. } | PeerWireMessage::Pong { .. } => {
                                (env.connection_peer.clone(), PathBuf::new())
                            }
                        };

                        if let PeerWireMessage::Data(msg) = &env.msg {
                            relay_peer_data(&peer_manager_task, &origin, msg).await;
                        }

                        if let PeerWireMessage::HostSummary(summary) = &env.msg {
                            let _ = peer_daemon.publish_peer_summary(&origin, summary.clone()).await;
                        }

                        {
                            let mut pm = peer_manager_task.lock().await;
                            match pm.handle_inbound(env).await {
                            HandleResult::Updated(ref updated_repo_id) => {
                                let overlay_version = pm.overlay_version();
                                let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = pm
                                    .get_peer_data()
                                    .iter()
                                    .filter_map(|(host, repos)| {
                                        repos.get(updated_repo_id).map(|state| (host.clone(), state.provider_data.clone()))
                                    })
                                    .collect();
                                drop(pm);

                                if let Some(local_path) = peer_daemon.preferred_local_path_for_identity(updated_repo_id).await {
                                    peer_daemon.set_peer_providers(&local_path, peers, overlay_version).await;
                                } else {
                                    let synthetic = crate::peer::synthetic_repo_path(&origin, &repo_path);
                                    let merged = crate::peer::merge_provider_data(
                                        &flotilla_protocol::ProviderData::default(),
                                        peer_daemon.host_name(),
                                        &peers.iter().map(|(h, d)| (h.clone(), d)).collect::<Vec<_>>(),
                                    );
                                    if let Err(e) = peer_daemon.add_virtual_repo(updated_repo_id.clone(), synthetic.clone(), merged).await {
                                        warn!(repo = %updated_repo_id, err = %e, "failed to add virtual repo");
                                    } else {
                                        peer_daemon.set_peer_providers(&synthetic, peers, overlay_version).await;
                                        let mut pm2 = peer_manager_task.lock().await;
                                        pm2.register_remote_repo(updated_repo_id.clone(), synthetic);
                                    }
                                }
                            }
                            HandleResult::ResyncRequested { request_id, requester_host, reply_via, repo, since_seq: _ } => {
                                let local_host = pm.local_host().clone();
                                if let Some(local_path) = peer_daemon.preferred_local_path_for_identity(&repo).await {
                                    if let Some((local_providers, seq)) = peer_daemon.get_local_providers(&local_path).await {
                                        reply_clock.tick(&local_host);
                                        let response_clock = reply_clock.clone();
                                        if let Err(e) = pm
                                            .send_to(
                                                &reply_via,
                                                PeerWireMessage::Routed(flotilla_protocol::RoutedPeerMessage::ResyncSnapshot {
                                                    request_id,
                                                    requester_host: requester_host.clone(),
                                                    responder_host: local_host.clone(),
                                                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                                                    repo_identity: repo.clone(),
                                                    repo_path: local_path.clone(),
                                                    clock: response_clock,
                                                    seq,
                                                    data: Box::new(local_providers.clone()),
                                                }),
                                            )
                                            .await
                                        {
                                            warn!(peer = %reply_via, err = %e, "failed to send resync response");
                                        }
                                    }
                                }
                            }
                            HandleResult::NeedsResync { from, repo } => {
                                let local_host = pm.local_host().clone();
                                let request_id = pm.note_pending_resync_request(from.clone(), repo.clone());
                                if let Err(e) = pm
                                    .send_to(
                                        &from,
                                        PeerWireMessage::Routed(flotilla_protocol::RoutedPeerMessage::RequestResync {
                                            request_id,
                                            requester_host: local_host,
                                            target_host: from.clone(),
                                            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                                            repo_identity: repo,
                                            since_seq: 0,
                                        }),
                                    )
                                    .await
                                {
                                    warn!(peer = %from, err = %e, "failed to send resync request");
                                }
                            }
                            HandleResult::ReconnectSuppressed { peer } => {
                                info!(peer = %peer, "peer requested reconnect suppression");
                            }
                            HandleResult::CommandRequested { request_id, requester_host, reply_via, command } => {
                                drop(pm);
                                let ready = Arc::new(Notify::new());
                                forwarded_commands_task.lock().await.insert(
                                    request_id,
                                    ForwardedCommand {
                                        state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) },
                                    },
                                );
                                tokio::spawn(execute_forwarded_command(
                                    Arc::clone(&peer_daemon),
                                    Arc::clone(&peer_manager_task),
                                    Arc::clone(&forwarded_commands_task),
                                    request_id,
                                    requester_host,
                                    reply_via,
                                    command,
                                    ready,
                                ));
                            }
                            HandleResult::CommandCancelRequested { cancel_id, requester_host, reply_via, command_request_id } => {
                                drop(pm);
                                tokio::spawn(cancel_forwarded_command(
                                    Arc::clone(&peer_daemon),
                                    Arc::clone(&peer_manager_task),
                                    Arc::clone(&forwarded_commands_task),
                                    cancel_id,
                                    requester_host,
                                    reply_via,
                                    command_request_id,
                                ));
                            }
                            HandleResult::CommandEventReceived { request_id, responder_host, event } => {
                                drop(pm);
                                emit_remote_command_event(&peer_daemon, &pending_remote_commands_task, request_id, responder_host, event)
                                    .await;
                            }
                            HandleResult::CommandResponseReceived { request_id, responder_host, result } => {
                                drop(pm);
                                complete_remote_command(
                                    &peer_daemon,
                                    &pending_remote_commands_task,
                                    request_id,
                                    responder_host,
                                    result,
                                )
                                .await;
                            }
                            HandleResult::CommandCancelResponseReceived { cancel_id, responder_host: _, error } => {
                                drop(pm);
                                complete_remote_cancel(&pending_remote_cancels_task, cancel_id, error).await;
                            }
                            HandleResult::Ignored => {}
                        }
                        }
                        sync_peer_query_state(&peer_manager_task, &peer_daemon).await;
                    }
                    _ = resync_sweep.tick() => {
                        let expired_repos = {
                            let mut pm = peer_manager_task.lock().await;
                            pm.sweep_expired_resyncs(Instant::now())
                        };
                        if !expired_repos.is_empty() {
                            rebuild_peer_overlays(&peer_manager_task, &peer_daemon, expired_repos).await;
                        }
                    }
                }
            }
        }
    });

    let outbound_daemon = Arc::clone(&daemon);
    let mut peer_connected_rx = peer_connected_rx;
    tokio::spawn(async move {
        let mut event_rx = outbound_daemon.subscribe();
        let mut outbound_clock = flotilla_protocol::VectorClock::default();
        let host_name = outbound_daemon.host_name().clone();
        let mut last_sent_versions: std::collections::HashMap<RepoIdentity, u64> = std::collections::HashMap::new();

        loop {
            tokio::select! {
                notice = peer_connected_rx.recv() => {
                    let Some(notice) = notice else { break };
                    debug!(peer = %notice.peer, generation = notice.generation, "sending local state to newly connected peer");
                    send_local_to_peer(
                        &outbound_daemon,
                        &outbound_peer_manager,
                        &host_name,
                        &mut outbound_clock,
                        &notice.peer,
                        notice.generation,
                    )
                    .await;
                }
                event = event_rx.recv() => {
                    let repo_path = match event {
                        Ok(DaemonEvent::RepoSnapshot(snapshot)) => Some(snapshot.repo.clone()),
                        Ok(DaemonEvent::RepoDelta(delta)) => Some(delta.repo.clone()),
                        Ok(_) => None,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "outbound peer event subscriber lagged");
                            None
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    if let Some(repo_path) = repo_path {
                        let Some(repo_identity) = outbound_daemon.tracked_repo_identity_for_path(&repo_path).await else {
                            continue;
                        };
                        let Some((local_providers, version)) = outbound_daemon.get_local_providers(&repo_path).await else {
                            continue;
                        };
                        if !should_send_local_version(&last_sent_versions, &repo_identity, version) {
                            continue;
                        }
                        let sent = send_local_to_peers(
                            &outbound_daemon,
                            &outbound_peer_manager,
                            &host_name,
                            &mut outbound_clock,
                            &repo_path,
                            local_providers,
                            version,
                        )
                        .await;
                        if sent {
                            last_sent_versions.insert(repo_identity, version);
                        }
                    }
                }
            }
        }
    });

    (inbound_handle, peer_connected_tx)
}

async fn handle_remote_restart_if_needed(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &HostName,
    last_known_session_id: Option<uuid::Uuid>,
) -> Option<uuid::Uuid> {
    let current_session_id = {
        let pm_lock = peer_manager.lock().await;
        pm_lock.peer_session_id(peer_name)
    };

    if let (Some(prev), Some(curr)) = (last_known_session_id, current_session_id) {
        if prev != curr {
            info!(peer = %peer_name, "remote daemon restarted (session_id changed), clearing stale data");
            let affected_repos = {
                let mut pm_lock = peer_manager.lock().await;
                pm_lock.clear_peer_data_for_restart(peer_name)
            };
            if !affected_repos.is_empty() {
                rebuild_peer_overlays(peer_manager, daemon, affected_repos).await;
            }
            sync_peer_query_state(peer_manager, daemon).await;
        }
    }

    current_session_id
}

async fn relay_peer_data(peer_manager: &Arc<Mutex<PeerManager>>, origin: &HostName, msg: &PeerDataMessage) {
    let relay_targets = {
        let pm = peer_manager.lock().await;
        pm.prepare_relay(origin, msg)
    };

    if relay_targets.is_empty() {
        return;
    }

    let sends = relay_targets.into_iter().map(|(name, sender, relayed_msg)| async move {
        match tokio::time::timeout(Duration::from_secs(5), sender.send(PeerWireMessage::Data(relayed_msg))).await {
            Ok(Ok(())) => {
                debug!(to = %name, "relayed peer data");
            }
            Ok(Err(e)) => {
                warn!(to = %name, err = %e, "relay send failed");
            }
            Err(_) => {
                warn!(to = %name, "relay send timed out (5s)");
            }
        }
    });
    join_all(sends).await;
}

/// Rebuild daemon overlays for repo identities affected by peer disconnect or failover.
async fn rebuild_peer_overlays(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    affected_repos: Vec<flotilla_protocol::RepoIdentity>,
) {
    for repo_id in affected_repos {
        if let Some(local_path) = daemon.preferred_local_path_for_identity(&repo_id).await {
            // Local repo — rebuild its peer overlay from remaining peers
            let (peers, overlay_version) = {
                let pm = peer_manager.lock().await;
                let v = pm.overlay_version();
                let peers = pm
                    .get_peer_data()
                    .iter()
                    .filter_map(|(host, repos)| repos.get(&repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect();
                (peers, v)
            };
            daemon.set_peer_providers(&local_path, peers, overlay_version).await;
        } else {
            // Remote-only repo — rebuild or remove depending on remaining peers
            let mut pm = peer_manager.lock().await;
            if pm.has_peer_data_for(&repo_id) {
                // Still has peer data — re-merge from remaining peers
                let overlay_version = pm.overlay_version();
                let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = pm
                    .get_peer_data()
                    .iter()
                    .filter_map(|(host, repos)| repos.get(&repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect();

                if let Some(synthetic_path) = pm.known_remote_repos().get(&repo_id).cloned() {
                    drop(pm);
                    daemon.set_peer_providers(&synthetic_path, peers, overlay_version).await;
                }
            } else if let Some(synthetic_path) = pm.unregister_remote_repo(&repo_id) {
                // No peers remain — remove the virtual tab
                drop(pm);
                info!(
                    repo = %repo_id,
                    path = %synthetic_path.display(),
                    "removing virtual repo — no peers remaining"
                );
                if let Err(e) = daemon.remove_repo(&synthetic_path).await {
                    warn!(
                        repo = %repo_id,
                        err = %e,
                        "failed to remove virtual repo"
                    );
                }
            }
        }
    }
}

async fn dispatch_resync_requests(peer_manager: &Arc<Mutex<PeerManager>>, requests: Vec<flotilla_protocol::RoutedPeerMessage>) {
    for request in requests {
        let target = match &request {
            flotilla_protocol::RoutedPeerMessage::RequestResync { target_host, .. } => target_host.clone(),
            flotilla_protocol::RoutedPeerMessage::ResyncSnapshot { requester_host, .. } => requester_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandRequest { target_host, .. } => target_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandCancelRequest { target_host, .. } => target_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandEvent { requester_host, .. } => requester_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandResponse { requester_host, .. } => requester_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandCancelResponse { requester_host, .. } => requester_host.clone(),
        };
        let sender = {
            let pm = peer_manager.lock().await;
            pm.resolve_sender(&target)
        };
        let sender = match sender {
            Ok(sender) => sender,
            Err(e) => {
                warn!(peer = %target, err = %e, "failed to resolve routed resync sender");
                continue;
            }
        };
        if let Err(e) = sender.send(PeerWireMessage::Routed(request)).await {
            warn!(peer = %target, err = %e, "failed to dispatch routed resync request");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_forwarded_command(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    forwarded_commands: ForwardedCommandMap,
    request_id: u64,
    requester_host: HostName,
    reply_via: HostName,
    command: Command,
    ready: Arc<Notify>,
) {
    let mut event_rx = daemon.subscribe();
    let responder_host = daemon.host_name().clone();
    let command_id = match daemon.execute(command).await {
        Ok(command_id) => command_id,
        Err(message) => {
            forwarded_commands.lock().await.remove(&request_id);
            ready.notify_waiters();
            let response = RoutedPeerMessage::CommandResponse {
                request_id,
                requester_host,
                responder_host,
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                result: Box::new(CommandResult::Error { message }),
            };
            let pm = peer_manager.lock().await;
            let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(response)).await;
            return;
        }
    };
    if let Some(entry) = forwarded_commands.lock().await.get_mut(&request_id) {
        entry.state = ForwardedCommandState::Running { command_id };
    }
    ready.notify_waiters();

    loop {
        match event_rx.recv().await {
            Ok(DaemonEvent::CommandStarted { command_id: id, repo_identity, repo, description, .. }) if id == command_id => {
                let event = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::Started { repo_identity, repo, description }),
                };
                let pm = peer_manager.lock().await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(event)).await;
            }
            Ok(DaemonEvent::CommandStepUpdate {
                command_id: id, repo_identity, repo, step_index, step_count, description, status, ..
            }) if id == command_id => {
                let event = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::StepUpdate { repo_identity, repo, step_index, step_count, description, status }),
                };
                let pm = peer_manager.lock().await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(event)).await;
            }
            Ok(DaemonEvent::CommandFinished { command_id: id, repo_identity, repo, result, .. }) if id == command_id => {
                let finished = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::Finished { repo_identity, repo, result: result.clone() }),
                };
                let response = RoutedPeerMessage::CommandResponse {
                    request_id,
                    requester_host,
                    responder_host,
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    result: Box::new(result),
                };
                let pm = peer_manager.lock().await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(finished)).await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(response)).await;
                forwarded_commands.lock().await.remove(&request_id);
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                forwarded_commands.lock().await.remove(&request_id);
                break;
            }
        }
    }
}

async fn await_forwarded_command_id(forwarded_commands: &ForwardedCommandMap, command_request_id: u64) -> Result<u64, String> {
    loop {
        let ready = {
            let forwarded = forwarded_commands.lock().await;
            match forwarded.get(&command_request_id) {
                Some(ForwardedCommand { state: ForwardedCommandState::Running { command_id } }) => return Ok(*command_id),
                Some(ForwardedCommand { state: ForwardedCommandState::Launching { ready } }) => Arc::clone(ready),
                None => return Err(format!("remote command not found: {command_request_id}")),
            }
        };
        ready.notified().await;
    }
}

async fn cancel_forwarded_command(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    forwarded_commands: ForwardedCommandMap,
    cancel_id: u64,
    requester_host: HostName,
    reply_via: HostName,
    command_request_id: u64,
) {
    let responder_host = daemon.host_name().clone();
    let error =
        match tokio::time::timeout(Duration::from_secs(5), await_forwarded_command_id(&forwarded_commands, command_request_id)).await {
            Ok(Ok(command_id)) => daemon.cancel(command_id).await.err(),
            Ok(Err(message)) => Some(message),
            Err(_) => Some(format!("timed out waiting for remote command registration: {command_request_id}")),
        };

    let response = RoutedPeerMessage::CommandCancelResponse {
        cancel_id,
        requester_host,
        responder_host,
        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
        error,
    };
    let pm = peer_manager.lock().await;
    let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(response)).await;
}

async fn emit_remote_command_event(
    daemon: &Arc<InProcessDaemon>,
    pending_remote_commands: &PendingRemoteCommandMap,
    request_id: u64,
    responder_host: HostName,
    event: CommandPeerEvent,
) {
    let mut pending = pending_remote_commands.lock().await;
    let Some(entry) = pending.get_mut(&request_id) else {
        return;
    };

    match event {
        CommandPeerEvent::Started { repo_identity, repo, description } => {
            entry.repo_identity = Some(repo_identity.clone());
            entry.repo = Some(repo.clone());
            daemon.send_event(DaemonEvent::CommandStarted {
                command_id: entry.command_id,
                host: responder_host,
                repo_identity,
                repo,
                description,
            });
        }
        CommandPeerEvent::StepUpdate { repo_identity, repo, step_index, step_count, description, status } => {
            entry.repo_identity = Some(repo_identity.clone());
            entry.repo = Some(repo.clone());
            daemon.send_event(DaemonEvent::CommandStepUpdate {
                command_id: entry.command_id,
                host: responder_host,
                repo_identity,
                repo,
                step_index,
                step_count,
                description,
                status,
            });
        }
        CommandPeerEvent::Finished { repo_identity, repo, result } => {
            entry.repo_identity = Some(repo_identity.clone());
            entry.repo = Some(repo.clone());
            entry.finished_via_event = true;
            daemon.send_event(DaemonEvent::CommandFinished {
                command_id: entry.command_id,
                host: responder_host,
                repo_identity,
                repo,
                result,
            });
        }
    }
}

async fn complete_remote_command(
    daemon: &Arc<InProcessDaemon>,
    pending_remote_commands: &PendingRemoteCommandMap,
    request_id: u64,
    responder_host: HostName,
    result: CommandResult,
) {
    let mut pending = pending_remote_commands.lock().await;
    let Some(entry) = pending.remove(&request_id) else {
        return;
    };

    if entry.finished_via_event {
        return;
    }

    let fallback_repo_identity = || RepoIdentity {
        // Routed command completions can arrive without any repo-bearing event.
        // In that case there is no reliable filesystem path from the responder,
        // so keep a local sentinel keyed to the last repo path we saw, if any.
        authority: "local".into(),
        path: entry.repo.clone().unwrap_or_default().display().to_string(),
    };

    daemon.send_event(DaemonEvent::CommandFinished {
        command_id: entry.command_id,
        host: responder_host,
        repo_identity: entry
            .repo_identity
            .or_else(|| match &result {
                CommandResult::TerminalPrepared { repo_identity, .. } => Some(repo_identity.clone()),
                _ => None,
            })
            .unwrap_or_else(fallback_repo_identity),
        repo: entry.repo.unwrap_or_default(),
        result,
    });
}

async fn complete_remote_cancel(pending_remote_cancels: &PendingRemoteCancelMap, cancel_id: u64, error: Option<String>) {
    let tx = pending_remote_cancels.lock().await.remove(&cancel_id);
    if let Some(tx) = tx {
        let _ = tx.send(match error {
            Some(message) => Err(message),
            None => Ok(()),
        });
    }
}

async fn disconnect_peer_and_rebuild(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &HostName,
    generation: u64,
) -> crate::peer::DisconnectPlan {
    let mut plan = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(peer_name, generation)
    };

    // Apply pre-computed overlay updates outside the PeerManager lock.
    // Identity → path is resolved here at apply time (not at computation time)
    // to avoid TOCTOU with concurrent add_repo/remove_repo.
    //
    // Overlay updates carry a version from the PeerManager, so
    // set_peer_providers will reject stale applies that lost the race
    // against fresher inbound data.
    for update in &plan.overlay_updates {
        match update {
            crate::peer::OverlayUpdate::SetProviders { identity, peers, overlay_version } => {
                // Resolve identity to current local path. For remote-only repos,
                // the path comes from known_remote_repos (already resolved in the plan).
                // For local repos that were removed concurrently, preferred_local_path_for_identity
                // returns None and we skip — the repo is gone, no overlay needed.
                if let Some(local_path) = daemon.preferred_local_path_for_identity(identity).await {
                    daemon.set_peer_providers(&local_path, peers.clone(), *overlay_version).await;
                } else if let Some(synthetic_path) = {
                    let pm = peer_manager.lock().await;
                    pm.known_remote_repos().get(identity).cloned()
                } {
                    daemon.set_peer_providers(&synthetic_path, peers.clone(), *overlay_version).await;
                }
            }
            crate::peer::OverlayUpdate::RemoveRepo { identity, path } => {
                info!(
                    repo = %identity,
                    path = %path.display(),
                    "removing virtual repo — no peers remaining"
                );
                if let Err(e) = daemon.remove_repo(path).await {
                    warn!(
                        repo = %identity,
                        err = %e,
                        "failed to remove virtual repo"
                    );
                }
            }
        }
    }

    sync_peer_query_state(peer_manager, daemon).await;

    let resync_requests = std::mem::take(&mut plan.resync_requests);
    dispatch_resync_requests(peer_manager, resync_requests).await;
    plan
}

/// Send local-only provider data to all peers for a given repo.
///
/// Called by the outbound task whenever any snapshot event (full or delta)
/// indicates local data changed. Always sends a full snapshot to peers —
/// peer replication doesn't use deltas.
///
/// Sends to both configured SSH transports (outbound peers we connected to)
/// and inbound peer clients (peers that connected to our socket).
/// Send local provider data to all connected peers.
/// Returns `true` if at least one peer was successfully sent to.
async fn send_local_to_peers(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    repo_path: &std::path::Path,
    local_providers: flotilla_protocol::ProviderData,
    local_data_version: u64,
) -> bool {
    let Some(identity) = daemon.tracked_repo_identity_for_path(repo_path).await else {
        return false;
    };

    clock.tick(host_name);
    let msg = PeerDataMessage {
        origin_host: host_name.clone(),
        repo_identity: identity,
        repo_path: repo_path.to_path_buf(),
        clock: clock.clone(),
        kind: flotilla_protocol::PeerDataKind::Snapshot { data: Box::new(local_providers), seq: local_data_version },
    };

    // Send to all active peers, including direct socket peers.
    let peer_senders = {
        let pm = peer_manager.lock().await;
        pm.active_peer_senders()
    };
    let mut any_sent = false;
    for (peer_name, sender) in peer_senders {
        if let Err(e) = sender.send(PeerWireMessage::Data(msg.clone())).await {
            debug!(peer = %peer_name, err = %e, "failed to send snapshot to peer");
        } else {
            any_sent = true;
        }
    }
    any_sent
}

fn should_send_local_version(
    last_sent_versions: &std::collections::HashMap<RepoIdentity, u64>,
    repo_identity: &RepoIdentity,
    local_data_version: u64,
) -> bool {
    local_data_version > last_sent_versions.get(repo_identity).copied().unwrap_or(0)
}

/// Send current local state for all repos to a specific newly-connected peer.
///
/// Unlike `send_local_to_peers` (which broadcasts), this targets a single peer
/// that has just connected and has no state. Bypasses `last_sent_versions` since
/// the peer needs everything regardless of what was previously sent to others.
///
/// The generation guard ensures this is a no-op if the connection has already
/// been superseded between the notice being sent and this function running.
async fn send_local_to_peer(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    peer: &HostName,
    generation: u64,
) -> bool {
    let repo_paths = daemon.tracked_repo_paths().await;

    // Resolve the sender once before iterating repos. If the connection
    // has already been superseded, skip the entire loop.
    let sender = {
        let pm = peer_manager.lock().await;
        pm.get_sender_if_current(peer, generation)
    };
    let Some(sender) = sender else {
        debug!(peer = %peer, "peer connection superseded, skipping local state send");
        return false;
    };

    let mut any_sent = false;
    if let Err(e) = sender.send(PeerWireMessage::HostSummary(daemon.local_host_summary().clone())).await {
        debug!(peer = %peer, err = %e, "failed to send host summary to peer");
    } else {
        any_sent = true;
    }

    for repo_path in repo_paths {
        let Some((local_providers, version)) = daemon.get_local_providers(&repo_path).await else {
            continue;
        };
        // Skip uninitialized repos (version 0 = not yet refreshed). Sending
        // empty data with a fresh vector clock would overwrite the peer's
        // existing state. The peer will receive data on first local refresh.
        if version == 0 {
            continue;
        }
        let Some(identity) = daemon.tracked_repo_identity_for_path(&repo_path).await else {
            continue;
        };

        clock.tick(host_name);
        let msg = PeerDataMessage {
            origin_host: host_name.clone(),
            repo_identity: identity,
            repo_path: repo_path.clone(),
            clock: clock.clone(),
            kind: flotilla_protocol::PeerDataKind::Snapshot { data: Box::new(local_providers), seq: version },
        };

        if let Err(e) = sender.send(PeerWireMessage::Data(msg)).await {
            debug!(peer = %peer, err = %e, "failed to send local state to peer");
        } else {
            any_sent = true;
        }
    }
    any_sent
}

enum ForwardResult {
    Disconnected,
    Shutdown,
    KeepaliveTimeout,
}

const PING_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(90);

/// Forward messages from an inbound receiver to the shared peer_data channel,
/// with periodic keepalive pings and liveness timeout detection.
async fn forward_with_keepalive(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
    sender: Arc<dyn PeerSender>,
) -> ForwardResult {
    forward_with_keepalive_for_test(tx, inbound_rx, peer_name, generation, sender, PING_INTERVAL, KEEPALIVE_TIMEOUT).await
}

async fn forward_with_keepalive_for_test(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
    sender: Arc<dyn PeerSender>,
    ping_interval_duration: Duration,
    keepalive_timeout: Duration,
) -> ForwardResult {
    let mut ping_interval = tokio::time::interval_at(tokio::time::Instant::now() + ping_interval_duration, ping_interval_duration);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_message_at = Instant::now();

    loop {
        tokio::select! {
            msg = inbound_rx.recv() => {
                match msg {
                    Some(peer_msg) => {
                        last_message_at = Instant::now();
                        if matches!(&peer_msg, PeerWireMessage::Pong { .. }) {
                            continue;
                        }
                        if let Err(e) = tx.send(InboundPeerEnvelope {
                            msg: peer_msg,
                            connection_generation: generation,
                            connection_peer: peer_name.clone(),
                        }).await {
                            warn!(peer = %peer_name, err = %e, "forwarding channel closed");
                            return ForwardResult::Shutdown;
                        }
                    }
                    None => return ForwardResult::Disconnected,
                }
            }
            _ = ping_interval.tick() => {
                if last_message_at.elapsed() > keepalive_timeout {
                    warn!(
                        peer = %peer_name,
                        elapsed_secs = last_message_at.elapsed().as_secs(),
                        "keepalive timeout — no messages received in 90s"
                    );
                    return ForwardResult::KeepaliveTimeout;
                }

                let timestamp =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                if let Err(e) = sender.send(PeerWireMessage::Ping { timestamp }).await {
                    debug!(peer = %peer_name, err = %e, "failed to send keepalive ping");
                }
            }
        }
    }
}

/// Write a JSON message followed by a newline to the writer.
async fn write_message(writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>, msg: &Message) -> Result<(), ()> {
    let mut w = writer.lock().await;
    flotilla_protocol::framing::write_message_line(&mut *w, msg).await.map_err(|_| ())
}

/// Handle a single client connection.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    pending_remote_commands: PendingRemoteCommandMap,
    pending_remote_cancels: PendingRemoteCancelMap,
    next_remote_command_id: Arc<AtomicU64>,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
    agent_state_store: SharedAgentStateStore,
) {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write_half)));
    let mut lines = reader.lines();
    let first_msg = tokio::select! {
        line_result = lines.next_line() => {
            match line_result {
                Ok(Some(line)) => match serde_json::from_str::<Message>(&line) {
                    Ok(msg) => Some(msg),
                    Err(e) => {
                        warn!(err = %e, "failed to parse first message");
                        None
                    }
                },
                Ok(None) => None,
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
            let count = client_count.fetch_add(1, Ordering::SeqCst) + 1;
            info!(%count, "client connected");
            client_notify.notify_one();

            let event_writer = Arc::clone(&writer);
            let mut event_rx = daemon.subscribe();
            let event_task = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            let msg = Message::Event { event: Box::new(event) };
                            if write_message(&event_writer, &msg).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "event subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let dispatch_ctx = DispatchContext {
                daemon: &daemon,
                peer_manager: &peer_manager,
                pending_remote_commands: &pending_remote_commands,
                pending_remote_cancels: &pending_remote_cancels,
                next_remote_command_id: &next_remote_command_id,
                agent_state_store: &agent_state_store,
            };
            let first_response = dispatch_request(&dispatch_ctx, id, request).await;
            if write_message(&writer, &first_response).await.is_ok() {
                loop {
                    tokio::select! {
                        line_result = lines.next_line() => {
                            match line_result {
                                Ok(Some(line)) => {
                                    let msg: Message = match serde_json::from_str(&line) {
                                        Ok(m) => m,
                                        Err(e) => {
                                            warn!(err = %e, "failed to parse message");
                                            continue;
                                        }
                                    };
                                    match msg {
                                        Message::Request { id, request } => {
                                            let response = dispatch_request(&dispatch_ctx, id, request).await;
                                            if write_message(&writer, &response).await.is_err() {
                                                break;
                                            }
                                        }
                                        other => {
                                            warn!(msg = ?other, "unexpected message type from client");
                                            break;
                                        }
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    error!(err = %e, "error reading from client");
                                    break;
                                }
                            }
                        }
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            }

            event_task.abort();
            let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
            info!(%count, "client disconnected");
            client_notify.notify_one();
        }
        Message::Hello { protocol_version, host_name, session_id } => {
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
                host_name: daemon.host_name().clone(),
                session_id: daemon.session_id(),
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
                let mut pm = peer_manager.lock().await;
                match pm.activate_connection_with_session(
                    host_name.clone(),
                    Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(Some(outbound_tx.clone())) }),
                    ConnectionMeta {
                        direction: ConnectionDirection::Inbound,
                        config_label: None,
                        expected_peer: None,
                        config_backed: false,
                    },
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
                    let mut pm = peer_manager.lock().await;
                    pm.take_displaced_sender(&host_name, displaced_generation)
                };
                if let Some(displaced) = displaced {
                    let _ = displaced.retire(GoodbyeReason::Superseded).await;
                }
            }
            sync_peer_query_state(&peer_manager, &daemon).await;
            let _ = daemon.publish_peer_connection_status(&host_name, PeerConnectionState::Connected).await;
            let _ = peer_connected_tx.send(PeerConnectedNotice { peer: host_name.clone(), generation });

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
                                        if let Err(e) = peer_data_tx.send(InboundPeerEnvelope {
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
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }

            let plan = disconnect_peer_and_rebuild(&peer_manager, &daemon, &host_name, generation).await;
            if plan.was_active {
                let _ = daemon.publish_peer_connection_status(&host_name, PeerConnectionState::Disconnected).await;
            }
            relay_task.abort();
        }
        other => {
            warn!(msg = ?other, "unexpected first message type from client");
        }
    }
}

/// Dispatch a request to the appropriate `DaemonHandle` method.
async fn dispatch_request(ctx: &DispatchContext<'_>, id: u64, request: Request) -> Message {
    match request {
        Request::ListRepos => match ctx.daemon.list_repos().await {
            Ok(repos) => Message::ok_response(id, Response::ListRepos(repos)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetState { repo } => match ctx.daemon.get_state(&flotilla_protocol::RepoSelector::Path(repo)).await {
            Ok(snapshot) => Message::ok_response(id, Response::GetState(Box::new(snapshot))),
            Err(e) => Message::error_response(id, e),
        },

        Request::Execute { command } => {
            let target_host = command.host.clone().unwrap_or_else(|| ctx.daemon.host_name().clone());
            if target_host != *ctx.daemon.host_name() {
                let request_id = {
                    let mut pm = ctx.peer_manager.lock().await;
                    pm.next_request_id()
                };
                let command_id = ctx.next_remote_command_id.fetch_add(1, Ordering::Relaxed);
                ctx.pending_remote_commands.lock().await.insert(request_id, PendingRemoteCommand {
                    command_id,
                    target_host: target_host.clone(),
                    repo_identity: extract_command_repo_identity(&command),
                    repo: None,
                    finished_via_event: false,
                });

                let routed = RoutedPeerMessage::CommandRequest {
                    request_id,
                    requester_host: ctx.daemon.host_name().clone(),
                    target_host: target_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    command: Box::new(command),
                };
                let send_result = {
                    let pm = ctx.peer_manager.lock().await;
                    pm.send_to(&target_host, PeerWireMessage::Routed(routed)).await
                };

                match send_result {
                    Ok(()) => Message::ok_response(id, Response::Execute { command_id }),
                    Err(e) => {
                        ctx.pending_remote_commands.lock().await.remove(&request_id);
                        Message::error_response(id, e)
                    }
                }
            } else {
                match ctx.daemon.execute(command).await {
                    Ok(command_id) => Message::ok_response(id, Response::Execute { command_id }),
                    Err(e) => Message::error_response(id, e),
                }
            }
        }

        Request::Cancel { command_id } => {
            let remote = {
                let pending = ctx.pending_remote_commands.lock().await;
                pending
                    .iter()
                    .find(|(_, entry)| entry.command_id == command_id)
                    .map(|(request_id, entry)| (*request_id, entry.target_host.clone()))
            };
            if let Some((command_request_id, target_host)) = remote {
                let cancel_id = {
                    let mut pm = ctx.peer_manager.lock().await;
                    pm.next_request_id()
                };
                let (tx, rx) = oneshot::channel();
                ctx.pending_remote_cancels.lock().await.insert(cancel_id, tx);
                let routed = RoutedPeerMessage::CommandCancelRequest {
                    cancel_id,
                    requester_host: ctx.daemon.host_name().clone(),
                    target_host: target_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    command_request_id,
                };
                let send_result = {
                    let pm = ctx.peer_manager.lock().await;
                    pm.send_to(&target_host, PeerWireMessage::Routed(routed)).await
                };
                if let Err(e) = send_result {
                    ctx.pending_remote_cancels.lock().await.remove(&cancel_id);
                    return Message::error_response(id, e);
                }
                match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    Ok(Ok(Ok(()))) => Message::ok_response(id, Response::Cancel),
                    Ok(Ok(Err(message))) => Message::error_response(id, message),
                    Ok(Err(_)) => Message::error_response(id, "remote cancel response channel closed".to_string()),
                    Err(_) => {
                        ctx.pending_remote_cancels.lock().await.remove(&cancel_id);
                        Message::error_response(id, "timed out waiting for remote cancel response".to_string())
                    }
                }
            } else {
                match ctx.daemon.cancel(command_id).await {
                    Ok(()) => Message::ok_response(id, Response::Cancel),
                    Err(e) => Message::error_response(id, e),
                }
            }
        }

        Request::Refresh { repo } => {
            let command =
                Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: Some(RepoSelector::Path(repo)) } };
            match ctx.daemon.execute(command).await {
                Ok(_) => Message::ok_response(id, Response::Refresh),
                Err(e) => Message::error_response(id, e),
            }
        }

        Request::AddRepo { path } => {
            let command = Command { host: None, context_repo: None, action: CommandAction::TrackRepoPath { path } };
            match ctx.daemon.execute(command).await {
                Ok(_) => Message::ok_response(id, Response::AddRepo),
                Err(e) => Message::error_response(id, e),
            }
        }

        Request::RemoveRepo { path } => {
            let command = Command { host: None, context_repo: None, action: CommandAction::UntrackRepo { repo: RepoSelector::Path(path) } };
            match ctx.daemon.execute(command).await {
                Ok(_) => Message::ok_response(id, Response::RemoveRepo),
                Err(e) => Message::error_response(id, e),
            }
        }

        Request::ReplaySince { last_seen } => {
            let last_seen = last_seen.into_iter().map(|entry| (entry.stream, entry.seq)).collect();
            match ctx.daemon.replay_since(&last_seen).await {
                Ok(events) => Message::ok_response(id, Response::ReplaySince(events)),
                Err(e) => Message::error_response(id, e),
            }
        }

        Request::GetStatus => match ctx.daemon.get_status().await {
            Ok(status) => Message::ok_response(id, Response::GetStatus(status)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetRepoDetail { slug } => match ctx.daemon.get_repo_detail(&flotilla_protocol::RepoSelector::Query(slug)).await {
            Ok(detail) => Message::ok_response(id, Response::GetRepoDetail(detail)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetRepoProviders { slug } => match ctx.daemon.get_repo_providers(&flotilla_protocol::RepoSelector::Query(slug)).await {
            Ok(providers) => Message::ok_response(id, Response::GetRepoProviders(providers)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetRepoWork { slug } => match ctx.daemon.get_repo_work(&flotilla_protocol::RepoSelector::Query(slug)).await {
            Ok(work) => Message::ok_response(id, Response::GetRepoWork(work)),
            Err(e) => Message::error_response(id, e),
        },

        Request::ListHosts => match ctx.daemon.list_hosts().await {
            Ok(hosts) => Message::ok_response(id, Response::ListHosts(hosts)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetHostStatus { host } => match ctx.daemon.get_host_status(&host).await {
            Ok(status) => Message::ok_response(id, Response::GetHostStatus(status)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetHostProviders { host } => match ctx.daemon.get_host_providers(&host).await {
            Ok(providers) => Message::ok_response(id, Response::GetHostProviders(providers)),
            Err(e) => Message::error_response(id, e),
        },

        Request::GetTopology => match ctx.daemon.get_topology().await {
            Ok(topology) => Message::ok_response(id, Response::GetTopology(topology)),
            Err(e) => Message::error_response(id, e),
        },

        Request::AgentHook { event } => {
            use flotilla_protocol::AgentEventType;

            tracing::info!(
                harness = ?event.harness,
                event_type = ?event.event_type,
                attachable_id = %event.attachable_id,
                session_id = ?event.session_id,
                "received agent hook event"
            );

            let result = (|| {
                let mut store = ctx.agent_state_store.lock().map_err(|_| "agent state store lock poisoned".to_string())?;

                // Resolve attachable_id: if the hook didn't have one from env, check
                // session_id index for a previously allocated one.
                let attachable_id = if let Some(ref sid) = event.session_id {
                    if let Some(existing) = store.lookup_by_session_id(sid) {
                        existing.clone()
                    } else {
                        event.attachable_id.clone()
                    }
                } else {
                    event.attachable_id.clone()
                };

                let changed = if event.event_type == AgentEventType::Ended {
                    store.remove(&attachable_id);
                    true
                } else if let Some(status) = event.event_type.to_status() {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let existing = store.get(&attachable_id);
                    // TODO(#393): persist event.cwd for CliAgentProvider correlation
                    let entry = AgentEntry {
                        harness: event.harness.clone(),
                        status,
                        model: event.model.clone().or_else(|| existing.and_then(|e| e.model.clone())),
                        session_title: existing.and_then(|e| e.session_title.clone()),
                        session_id: event.session_id.clone(),
                        last_event_epoch_secs: now,
                    };
                    store.upsert(attachable_id, entry);
                    true
                } else {
                    false // NoChange events skip the store
                };

                if changed {
                    store.save()
                } else {
                    Ok(())
                }
                // NOTE: agent state changes are not pushed to the TUI immediately.
                // They become visible on the next refresh cycle (~10s). A proper fix
                // requires the log-based architecture (#256) where push events can
                // trigger targeted view re-materialization without a full provider refresh.
            })();

            match result {
                Ok(()) => Message::ok_response(id, Response::AgentHook),
                Err(e) => {
                    warn!(err = %e, "failed to process agent hook event");
                    Message::error_response(id, e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
