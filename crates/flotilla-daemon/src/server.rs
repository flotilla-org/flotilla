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
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    Command, CommandPeerEvent, CommandResult, ConfigLabel, DaemonEvent, GoodbyeReason, HostName, Message, PeerConnectionState,
    PeerDataMessage, PeerWireMessage, RoutedPeerMessage, PROTOCOL_VERSION,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
};
use tracing::{debug, error, info, warn};

use crate::{
    peer::{
        ActivationResult, ConnectionDirection, ConnectionMeta, HandleResult, InboundPeerEnvelope, PeerManager, PeerSender, SshTransport,
    },
    peer_networking::PeerConnectedNotice,
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
    repo: Option<PathBuf>,
    finished_via_event: bool,
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
    pending_remote_commands: Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
    next_remote_command_id: Arc<AtomicU64>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last client disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let daemon_config = config.load_daemon_config();
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
        let discovery = flotilla_core::providers::discovery::DiscoveryRuntime::for_process(daemon_config.follower);
        let daemon = InProcessDaemon::new(repo_paths, Arc::clone(&config), discovery, host_name.clone()).await;
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

        info!(
            host = %host_name,
            %peer_count,
            "initialized PeerManager"
        );

        for peer_host in peer_manager.configured_peer_names() {
            daemon.send_event(DaemonEvent::PeerStatusChanged { host: peer_host, status: PeerConnectionState::Disconnected });
        }
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);

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
            peer_manager: Arc::new(Mutex::new(peer_manager)),
            pending_remote_commands: Arc::new(Mutex::new(HashMap::new())),
            next_remote_command_id: Arc::new(AtomicU64::new(1 << 62)),
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
        let next_remote_command_id = self.next_remote_command_id;
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

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

        // Spawn peer manager background task
        let peer_manager = self.peer_manager;
        let outbound_peer_manager = Arc::clone(&peer_manager);
        let peer_manager_task = Arc::clone(&peer_manager);
        let peer_data_tx_for_ssh = peer_data_tx.clone();
        let peer_connected_tx_for_ssh = peer_connected_tx.clone();
        let peer_daemon = Arc::clone(&daemon);
        let pending_remote_commands_task = Arc::clone(&pending_remote_commands);
        tokio::spawn(async move {
            if let Some(mut rx) = peer_data_rx {
                let mut resync_sweep = tokio::time::interval(Duration::from_secs(5));
                // Connect all peers and collect initial receivers into a map
                let mut initial_rx_map: HashMap<HostName, (u64, mpsc::Receiver<PeerWireMessage>)> = HashMap::new();
                let peer_names = {
                    let mut pm = peer_manager_task.lock().await;
                    let names = pm.configured_peer_names();
                    for (name, generation, rx) in pm.connect_all().await {
                        initial_rx_map.insert(name, (generation, rx));
                    }
                    names
                };

                // Spawn resilient per-peer forwarding tasks with reconnect loop.
                // On disconnect, stale peer data is cleared from the daemon overlay
                // so the UI doesn't show checkouts from unreachable hosts.
                // Emit initial connecting status for all peers
                for name in &peer_names {
                    peer_daemon.send_event(DaemonEvent::PeerStatusChanged { host: name.clone(), status: PeerConnectionState::Connecting });
                }

                // Emit connected/disconnected based on initial connect results
                for name in &peer_names {
                    let status =
                        if initial_rx_map.contains_key(name) { PeerConnectionState::Connected } else { PeerConnectionState::Disconnected };
                    peer_daemon.send_event(DaemonEvent::PeerStatusChanged { host: name.clone(), status });
                }

                for peer_name in peer_names {
                    let tx = peer_data_tx_for_ssh.clone();
                    let pm = Arc::clone(&peer_manager_task);
                    let daemon_for_cleanup = Arc::clone(&peer_daemon);
                    let initial_rx = initial_rx_map.remove(&peer_name);
                    let peer_connected_tx_clone = peer_connected_tx_for_ssh.clone();

                    tokio::spawn(async move {
                        // Forward from initial connection if available
                        if let Some((generation, mut inbound_rx)) = initial_rx {
                            let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });
                            if !forward_until_closed(&tx, &mut inbound_rx, &peer_name, generation).await {
                                return; // Main channel closed, stop entirely
                            }
                            info!(peer = %peer_name, "SSH connection dropped, will reconnect");
                            let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                            if plan.was_active {
                                daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                    host: peer_name.clone(),
                                    status: PeerConnectionState::Disconnected,
                                });
                            }
                        }

                        // Reconnect loop with exponential backoff
                        let mut attempt: u32 = 1;
                        loop {
                            if let Some(delay) = {
                                let mut pm = pm.lock().await;
                                pm.reconnect_suppressed_until(&peer_name).map(|deadline| deadline.saturating_duration_since(Instant::now()))
                            } {
                                info!(
                                    peer = %peer_name,
                                    delay_secs = delay.as_secs(),
                                    "reconnect suppressed after peer retirement"
                                );
                                tokio::time::sleep(delay).await;
                                attempt = 1;
                                continue;
                            }
                            daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                host: peer_name.clone(),
                                status: PeerConnectionState::Reconnecting,
                            });
                            let delay = SshTransport::backoff_delay(attempt);
                            info!(
                                peer = %peer_name,
                                %attempt,
                                delay_secs = delay.as_secs(),
                                "reconnecting after backoff"
                            );
                            tokio::time::sleep(delay).await;

                            let reconnect_result = {
                                let mut pm = pm.lock().await;
                                pm.reconnect_peer(&peer_name).await
                            };

                            match reconnect_result {
                                Ok((generation, mut inbound_rx)) => {
                                    info!(peer = %peer_name, "reconnected successfully");
                                    daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                        host: peer_name.clone(),
                                        status: PeerConnectionState::Connected,
                                    });
                                    let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });
                                    attempt = 1;
                                    if !forward_until_closed(&tx, &mut inbound_rx, &peer_name, generation).await {
                                        return;
                                    }
                                    info!(
                                        peer = %peer_name,
                                        "SSH connection dropped, will reconnect"
                                    );
                                    let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                                    if plan.was_active {
                                        daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                            host: peer_name.clone(),
                                            status: PeerConnectionState::Disconnected,
                                        });
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        peer = %peer_name,
                                        err = %e,
                                        %attempt,
                                        "reconnection failed"
                                    );
                                    attempt = attempt.saturating_add(1);
                                }
                            }
                        }
                    });
                }

                // Process inbound peer data.
                // Persistent clock for reply messages (resync responses).
                let mut reply_clock = flotilla_protocol::VectorClock::default();

                loop {
                    tokio::select! {
                        maybe_env = rx.recv() => {
                            let Some(env) = maybe_env else {
                                break;
                            };
                    let (origin, repo_path) = match &env.msg {
                        PeerWireMessage::Data(msg) => {
                            (msg.origin_host.clone(), msg.repo_path.clone())
                        }
                        // repo_path is unused for host summaries; handle_inbound always returns Ignored.
                        PeerWireMessage::HostSummary(_) => (env.connection_peer.clone(), PathBuf::new()),
                        PeerWireMessage::Routed(
                            flotilla_protocol::RoutedPeerMessage::ResyncSnapshot {
                                responder_host,
                                repo_path,
                                ..
                            },
                        ) => (responder_host.clone(), repo_path.clone()),
                        PeerWireMessage::Routed(_) => (env.connection_peer.clone(), PathBuf::new()),
                        PeerWireMessage::Goodbye { .. } | PeerWireMessage::Ping { .. } | PeerWireMessage::Pong { .. } => {
                            (env.connection_peer.clone(), PathBuf::new())
                        }
                    };

                    let mut pm = peer_manager_task.lock().await;

                    if let PeerWireMessage::Data(msg) = &env.msg {
                        pm.relay(&origin, msg).await;
                    }

                    // Then handle locally
                    let result = pm.handle_inbound(env).await;
                    match result {
                        HandleResult::Updated(ref updated_repo_id) => {
                            // Collect all peer data for this repo identity
                            let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = pm
                                .get_peer_data()
                                .iter()
                                .filter_map(|(host, repos)| {
                                    repos
                                        .get(updated_repo_id)
                                        .map(|state| (host.clone(), state.provider_data.clone()))
                                })
                                .collect();

                            // Drop the lock before async daemon calls
                            drop(pm);

                            // Find local repo or create virtual repo
                            if let Some(local_path) =
                                peer_daemon.find_repo_by_identity(updated_repo_id).await
                            {
                                debug!(
                                    repo = %updated_repo_id,
                                    path = %local_path.display(),
                                    peer_count = peers.len(),
                                    "updating local repo with peer data"
                                );
                                peer_daemon.set_peer_providers(&local_path, peers).await;
                            } else {
                                // Remote-only repo — create or update virtual repo
                                let synthetic =
                                    crate::peer::synthetic_repo_path(&origin, &repo_path);
                                debug!(
                                    repo = %updated_repo_id,
                                    path = %synthetic.display(),
                                    "creating/updating virtual repo for remote-only peer"
                                );

                                // Build merged provider data for virtual repo
                                let merged = crate::peer::merge_provider_data(
                                    &flotilla_protocol::ProviderData::default(),
                                    peer_daemon.host_name(),
                                    &peers
                                        .iter()
                                        .map(|(h, d)| (h.clone(), d))
                                        .collect::<Vec<_>>(),
                                );

                                if let Err(e) = peer_daemon
                                    .add_virtual_repo(synthetic.clone(), merged)
                                    .await
                                {
                                    warn!(
                                        repo = %updated_repo_id,
                                        err = %e,
                                        "failed to add virtual repo"
                                    );
                                } else {
                                    // Also set peer providers on the virtual repo
                                    // so future merges work correctly
                                    peer_daemon.set_peer_providers(&synthetic, peers).await;

                                    // Register in PeerManager
                                    let mut pm2 = peer_manager_task.lock().await;
                                    pm2.register_remote_repo(updated_repo_id.clone(), synthetic);
                                }
                            }
                        }
                        HandleResult::ResyncRequested {
                            request_id,
                            requester_host,
                            reply_via,
                            repo,
                            since_seq: _, // Phase 1: always send full snapshot
                        } => {
                            let local_host = pm.local_host().clone();

                            // Send local-only providers (not merged) back to requesting peer
                            if let Some(local_path) = peer_daemon.find_repo_by_identity(&repo).await
                            {
                                if let Some((local_providers, seq)) =
                                    peer_daemon.get_local_providers(&local_path).await
                                {
                                    reply_clock.tick(&local_host);
                                    let response_clock = reply_clock.clone();
                                    let response_repo = repo.clone();
                                    let response_repo_path = local_path.clone();
                                    let response_host = local_host.clone();
                                    let response_data = local_providers.clone();
                                    if let Err(e) = pm
                                        .send_to(
                                            &reply_via,
                                            PeerWireMessage::Routed(
                                                flotilla_protocol::RoutedPeerMessage::ResyncSnapshot {
                                                    request_id,
                                                    requester_host: requester_host.clone(),
                                                    responder_host: response_host,
                                                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                                                    repo_identity: response_repo,
                                                    repo_path: response_repo_path,
                                                    clock: response_clock,
                                                    seq,
                                                    data: Box::new(response_data),
                                                },
                                            ),
                                        )
                                        .await
                                    {
                                        warn!(
                                            peer = %reply_via,
                                            err = %e,
                                            "failed to send resync response"
                                        );
                                    }
                                }
                            }
                        }
                        HandleResult::NeedsResync { from, repo } => {
                            let local_host = pm.local_host().clone();
                            let request_id =
                                pm.note_pending_resync_request(from.clone(), repo.clone());
                            if let Err(e) = pm
                                .send_to(
                                    &from,
                                    PeerWireMessage::Routed(
                                        flotilla_protocol::RoutedPeerMessage::RequestResync {
                                            request_id,
                                            requester_host: local_host,
                                            target_host: from.clone(),
                                            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                                            repo_identity: repo,
                                            since_seq: 0,
                                        },
                                    ),
                                )
                                .await
                            {
                                warn!(
                                    peer = %from,
                                    err = %e,
                                    "failed to send resync request"
                                );
                            }
                        }
                        HandleResult::ReconnectSuppressed { peer } => {
                            info!(peer = %peer, "peer requested reconnect suppression");
                        }
                        HandleResult::CommandRequested { request_id, requester_host, reply_via, command } => {
                            drop(pm);
                            tokio::spawn(execute_forwarded_command(
                                Arc::clone(&peer_daemon),
                                Arc::clone(&peer_manager_task),
                                request_id,
                                requester_host,
                                reply_via,
                                command,
                            ));
                        }
                        HandleResult::CommandEventReceived { request_id, responder_host, event } => {
                            drop(pm);
                            emit_remote_command_event(
                                &peer_daemon,
                                &pending_remote_commands_task,
                                request_id,
                                responder_host,
                                event,
                            )
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
                        HandleResult::Ignored => {}
                    }
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

        // Spawn outbound task: forward local snapshots to peers as PeerDataMessages.
        // Uses local-only providers (no peer overlay) to avoid echoing peer data back.
        // Maintains a persistent vector clock so each message has a strictly increasing clock.
        // Sends to both configured SSH transports (PeerManager) and inbound peer clients
        // (peers that connected to us via socket).
        //
        // Also listens for PeerConnectedNotice to send current local state to
        // newly connected peers that would otherwise receive nothing until the
        // next local change.
        let outbound_daemon = Arc::clone(&daemon);
        let mut peer_connected_rx = peer_connected_rx;
        tokio::spawn(async move {
            let mut event_rx = outbound_daemon.subscribe();
            let mut outbound_clock = flotilla_protocol::VectorClock::default();
            let host_name = outbound_daemon.host_name().clone();
            let mut last_sent_versions: std::collections::HashMap<std::path::PathBuf, u64> = std::collections::HashMap::new();

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
                            Ok(DaemonEvent::SnapshotFull(snapshot)) => Some(snapshot.repo.clone()),
                            Ok(DaemonEvent::SnapshotDelta(delta)) => Some(delta.repo.clone()),
                            Ok(_) => None,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!(skipped = n, "outbound peer event subscriber lagged");
                                None
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        };
                        if let Some(repo_path) = repo_path {
                            // Only send to peers when local data has actually changed.
                            // get_local_providers returns a local_data_version that only
                            // increments on local changes (provider refreshes, issue
                            // updates, searches), not peer data merges. This prevents a
                            // feedback loop where peer data triggers re-sending unchanged
                            // local data back to peers endlessly.
                            let Some((local_providers, version)) = outbound_daemon.get_local_providers(&repo_path).await else {
                                continue;
                            };
                            let last = last_sent_versions.get(&repo_path).copied().unwrap_or(0);
                            if version <= last {
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
                            // Only record the version as sent if at least one peer
                            // received it. Otherwise a version produced while no peers
                            // are connected would be suppressed forever on reconnect.
                            if sent {
                                last_sent_versions.insert(repo_path, version);
                            }
                        }
                    }
                }
            }
        });

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
                            let next_remote_command_id = Arc::clone(&next_remote_command_id);
                            let peer_connected_tx = peer_connected_tx.clone();

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_manager,
                                    pending_remote_commands,
                                    next_remote_command_id,
                                    client_count,
                                    client_notify,
                                    peer_connected_tx,
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

/// Rebuild daemon overlays for repo identities affected by peer disconnect or failover.
async fn rebuild_peer_overlays(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    affected_repos: Vec<flotilla_protocol::RepoIdentity>,
) {
    for repo_id in affected_repos {
        if let Some(local_path) = daemon.find_repo_by_identity(&repo_id).await {
            // Local repo — rebuild its peer overlay from remaining peers
            let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = {
                let pm = peer_manager.lock().await;
                pm.get_peer_data()
                    .iter()
                    .filter_map(|(host, repos)| repos.get(&repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect()
            };
            daemon.set_peer_providers(&local_path, peers).await;
        } else {
            // Remote-only repo — rebuild or remove depending on remaining peers
            let mut pm = peer_manager.lock().await;
            if pm.has_peer_data_for(&repo_id) {
                // Still has peer data — re-merge from remaining peers
                let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = pm
                    .get_peer_data()
                    .iter()
                    .filter_map(|(host, repos)| repos.get(&repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect();

                if let Some(synthetic_path) = pm.known_remote_repos().get(&repo_id).cloned() {
                    drop(pm);
                    daemon.set_peer_providers(&synthetic_path, peers).await;
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
            flotilla_protocol::RoutedPeerMessage::CommandEvent { requester_host, .. } => requester_host.clone(),
            flotilla_protocol::RoutedPeerMessage::CommandResponse { requester_host, .. } => requester_host.clone(),
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

async fn execute_forwarded_command(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    request_id: u64,
    requester_host: HostName,
    reply_via: HostName,
    command: Command,
) {
    let mut event_rx = daemon.subscribe();
    let responder_host = daemon.host_name().clone();
    let command_id = match daemon.execute(command).await {
        Ok(command_id) => command_id,
        Err(message) => {
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

    loop {
        match event_rx.recv().await {
            Ok(DaemonEvent::CommandStarted { command_id: id, repo, description, .. }) if id == command_id => {
                let event = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::Started { repo, description }),
                };
                let pm = peer_manager.lock().await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(event)).await;
            }
            Ok(DaemonEvent::CommandStepUpdate { command_id: id, repo, step_index, step_count, description, status, .. })
                if id == command_id =>
            {
                let event = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::StepUpdate { repo, step_index, step_count, description, status }),
                };
                let pm = peer_manager.lock().await;
                let _ = pm.send_to(&reply_via, PeerWireMessage::Routed(event)).await;
            }
            Ok(DaemonEvent::CommandFinished { command_id: id, repo, result, .. }) if id == command_id => {
                let finished = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_host: requester_host.clone(),
                    responder_host: responder_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    event: Box::new(CommandPeerEvent::Finished { repo, result: result.clone() }),
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
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn emit_remote_command_event(
    daemon: &Arc<InProcessDaemon>,
    pending_remote_commands: &Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
    request_id: u64,
    responder_host: HostName,
    event: CommandPeerEvent,
) {
    let mut pending = pending_remote_commands.lock().await;
    let Some(entry) = pending.get_mut(&request_id) else {
        return;
    };

    match event {
        CommandPeerEvent::Started { repo, description } => {
            entry.repo = Some(repo.clone());
            daemon.send_event(DaemonEvent::CommandStarted { command_id: entry.command_id, host: responder_host, repo, description });
        }
        CommandPeerEvent::StepUpdate { repo, step_index, step_count, description, status } => {
            entry.repo = Some(repo.clone());
            daemon.send_event(DaemonEvent::CommandStepUpdate {
                command_id: entry.command_id,
                host: responder_host,
                repo,
                step_index,
                step_count,
                description,
                status,
            });
        }
        CommandPeerEvent::Finished { repo, result } => {
            entry.repo = Some(repo.clone());
            entry.finished_via_event = true;
            daemon.send_event(DaemonEvent::CommandFinished { command_id: entry.command_id, host: responder_host, repo, result });
        }
    }
}

async fn complete_remote_command(
    daemon: &Arc<InProcessDaemon>,
    pending_remote_commands: &Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
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

    daemon.send_event(DaemonEvent::CommandFinished {
        command_id: entry.command_id,
        host: responder_host,
        repo: entry.repo.unwrap_or_default(),
        result,
    });
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
    // Note: a residual apply-ordering race exists here. Between releasing
    // the PM lock above and calling set_peer_providers below, the central
    // processor can accept fresh inbound data and call set_peer_providers
    // via the HandleResult::Updated path. Because set_peer_providers is a
    // blind replace, this apply could overwrite that newer data. This is
    // the same read-then-apply pattern shared by ALL overlay write paths
    // (including the central processor itself). The effect is transient:
    // the next inbound message for the affected repo will re-apply the
    // correct state. Fully fixing this requires versioned/conditional
    // set_peer_providers, which is a broader change tracked separately.
    for update in &plan.overlay_updates {
        match update {
            crate::peer::OverlayUpdate::SetProviders { identity, peers } => {
                // Resolve identity to current local path. For remote-only repos,
                // the path comes from known_remote_repos (already resolved in the plan).
                // For local repos that were removed concurrently, find_repo_by_identity
                // returns None and we skip — the repo is gone, no overlay needed.
                if let Some(local_path) = daemon.find_repo_by_identity(identity).await {
                    daemon.set_peer_providers(&local_path, peers.clone()).await;
                } else if let Some(synthetic_path) = {
                    let pm = peer_manager.lock().await;
                    pm.known_remote_repos().get(identity).cloned()
                } {
                    daemon.set_peer_providers(&synthetic_path, peers.clone()).await;
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
    let Some(identity) = daemon.find_identity_for_path(repo_path).await else {
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
        let Some(identity) = daemon.find_identity_for_path(&repo_path).await else {
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

/// Forward messages from an inbound receiver to the shared peer_data channel.
///
/// Returns `true` if the inbound receiver was closed (connection dropped),
/// `false` if the outbound channel was closed (daemon shutting down).
async fn forward_until_closed(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
) -> bool {
    while let Some(msg) = inbound_rx.recv().await {
        if let Err(e) = tx.send(InboundPeerEnvelope { msg, connection_generation: generation, connection_peer: peer_name.clone() }).await {
            warn!(peer = %peer_name, err = %e, "forwarding channel closed");
            return false;
        }
    }
    true
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
    pending_remote_commands: Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
    next_remote_command_id: Arc<AtomicU64>,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
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
        Message::Request { id, method, params } => {
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

            let first_response =
                dispatch_request(&daemon, &peer_manager, &pending_remote_commands, &next_remote_command_id, id, &method, params).await;
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
                                        Message::Request { id, method, params } => {
                                            let response = dispatch_request(
                                                &daemon,
                                                &peer_manager,
                                                &pending_remote_commands,
                                                &next_remote_command_id,
                                                id,
                                                &method,
                                                params,
                                            )
                                            .await;
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
            daemon.send_event(DaemonEvent::PeerStatusChanged { host: host_name.clone(), status: PeerConnectionState::Connected });
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
                daemon.send_event(DaemonEvent::PeerStatusChanged { host: host_name, status: PeerConnectionState::Disconnected });
            }
            relay_task.abort();
        }
        other => {
            warn!(msg = ?other, "unexpected first message type from client");
        }
    }
}

/// Dispatch a request to the appropriate `DaemonHandle` method.
async fn dispatch_request(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    pending_remote_commands: &Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
    next_remote_command_id: &Arc<AtomicU64>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Message {
    match method {
        "list_repos" => match daemon.list_repos().await {
            Ok(repos) => Message::ok_response(id, &repos),
            Err(e) => Message::error_response(id, e),
        },

        "get_state" => {
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.get_state(&repo).await {
                Ok(snapshot) => Message::ok_response(id, &snapshot),
                Err(e) => Message::error_response(id, e),
            }
        }

        "execute" => {
            let command: Command = match params
                .get("command")
                .cloned()
                .ok_or_else(|| "missing 'command' field".to_string())
                .and_then(|v| serde_json::from_value(v).map_err(|e| format!("invalid command: {e}")))
            {
                Ok(cmd) => cmd,
                Err(e) => return Message::error_response(id, e),
            };

            let target_host = command.host.clone().unwrap_or_else(|| daemon.host_name().clone());
            if target_host != *daemon.host_name() {
                let request_id = {
                    let mut pm = peer_manager.lock().await;
                    pm.next_request_id()
                };
                let command_id = next_remote_command_id.fetch_add(1, Ordering::Relaxed);
                pending_remote_commands.lock().await.insert(request_id, PendingRemoteCommand {
                    command_id,
                    repo: None,
                    finished_via_event: false,
                });

                let routed = RoutedPeerMessage::CommandRequest {
                    request_id,
                    requester_host: daemon.host_name().clone(),
                    target_host: target_host.clone(),
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    command: Box::new(command),
                };
                let send_result = {
                    let pm = peer_manager.lock().await;
                    pm.send_to(&target_host, PeerWireMessage::Routed(routed)).await
                };

                match send_result {
                    Ok(()) => Message::ok_response(id, &command_id),
                    Err(e) => {
                        pending_remote_commands.lock().await.remove(&request_id);
                        Message::error_response(id, e)
                    }
                }
            } else {
                match daemon.execute(command).await {
                    Ok(command_id) => Message::ok_response(id, &command_id),
                    Err(e) => Message::error_response(id, e),
                }
            }
        }

        "cancel" => {
            let command_id: u64 = match params.get("command_id").and_then(|v| v.as_u64()) {
                Some(id) => id,
                None => return Message::error_response(id, "missing or invalid 'command_id'".to_string()),
            };
            match daemon.cancel(command_id).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "refresh" => {
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.refresh(&repo).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "add_repo" => {
            let path = match extract_path_param(&params, "path") {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.add_repo(&path).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "remove_repo" => {
            let path = match extract_path_param(&params, "path") {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.remove_repo(&path).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "replay_since" => {
            let last_seen: std::collections::HashMap<std::path::PathBuf, u64> =
                params.get("last_seen").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_else(|| {
                    warn!("replay_since: failed to parse last_seen, returning full snapshots");
                    std::collections::HashMap::new()
                });
            match daemon.replay_since(&last_seen).await {
                Ok(events) => Message::ok_response(id, &events),
                Err(e) => Message::error_response(id, e),
            }
        }

        "get_status" => match daemon.get_status().await {
            Ok(status) => Message::ok_response(id, &status),
            Err(e) => Message::error_response(id, e),
        },

        "get_repo_detail" => {
            let slug = match extract_str_param(&params, "slug") {
                Ok(s) => s,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.get_repo_detail(&slug).await {
                Ok(detail) => Message::ok_response(id, &detail),
                Err(e) => Message::error_response(id, e),
            }
        }

        "get_repo_providers" => {
            let slug = match extract_str_param(&params, "slug") {
                Ok(s) => s,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.get_repo_providers(&slug).await {
                Ok(providers) => Message::ok_response(id, &providers),
                Err(e) => Message::error_response(id, e),
            }
        }

        "get_repo_work" => {
            let slug = match extract_str_param(&params, "slug") {
                Ok(s) => s,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.get_repo_work(&slug).await {
                Ok(work) => Message::ok_response(id, &work),
                Err(e) => Message::error_response(id, e),
            }
        }

        unknown => Message::error_response(id, format!("unknown method: {unknown}")),
    }
}

/// Extract the "repo" field from params as a PathBuf.
fn extract_repo_path(params: &serde_json::Value) -> Result<PathBuf, String> {
    extract_path_param(params, "repo")
}

/// Extract a named path field from params as a PathBuf.
fn extract_path_param(params: &serde_json::Value, field: &str) -> Result<PathBuf, String> {
    params.get(field).and_then(|v| v.as_str()).map(PathBuf::from).ok_or_else(|| format!("missing '{field}' parameter"))
}

/// Extract a named string field from params.
fn extract_str_param(params: &serde_json::Value, field: &str) -> Result<String, String> {
    params.get(field).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| format!("missing '{field}' parameter"))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex as StdMutex};

    use async_trait::async_trait;
    use flotilla_core::providers::discovery::test_support::fake_discovery;
    use flotilla_protocol::{
        Checkout, Command, CommandAction, CommandPeerEvent, CommandResult, DaemonEvent, HostName, HostPath, PeerDataKind, PeerDataMessage,
        PeerWireMessage, ProviderData, RepoIdentity, RepoInfo, RoutedPeerMessage, VectorClock,
    };
    use indexmap::IndexMap;

    use super::*;
    use crate::peer::{
        test_support::{ensure_test_connection_generation, handle_test_peer_data},
        PeerSender,
    };

    struct CapturePeerSender {
        sent: Arc<StdMutex<Vec<PeerWireMessage>>>,
    }

    #[async_trait]
    impl PeerSender for CapturePeerSender {
        async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
            self.sent.lock().expect("lock").push(msg);
            Ok(())
        }

        async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
            self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
            Ok(())
        }
    }

    fn assert_ok_empty_response(msg: Message, expected_id: u64) {
        match msg {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, expected_id);
                assert!(ok);
                assert!(data.is_none());
                assert!(error.is_none());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    async fn empty_daemon() -> (tempfile::TempDir, Arc<InProcessDaemon>) {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::local()).await;
        (tmp, daemon)
    }

    fn empty_routing_state() -> (Arc<Mutex<PeerManager>>, Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>, Arc<AtomicU64>) {
        (
            Arc::new(Mutex::new(PeerManager::new(HostName::new("local")))),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(AtomicU64::new(1 << 62)),
        )
    }

    async fn dispatch_request_test(daemon: &Arc<InProcessDaemon>, id: u64, method: &str, params: serde_json::Value) -> Message {
        let (peer_manager, pending_remote_commands, next_remote_command_id) = empty_routing_state();
        dispatch_request(daemon, &peer_manager, &pending_remote_commands, &next_remote_command_id, id, method, params).await
    }

    fn checkout(branch: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn peer_snapshot(host: &str, repo_identity: &RepoIdentity, repo_path: &Path, checkout_path: &str, branch: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: repo_identity.clone(),
            repo_path: repo_path.to_path_buf(),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new(host), checkout_path), checkout(branch))]),
                    ..Default::default()
                }),
                seq: 1,
            },
        }
    }

    #[tokio::test]
    async fn write_message_writes_json_line() {
        let (a, b) = tokio::net::UnixStream::pair().expect("pair");
        let (_read_half, write_half) = a.into_split();
        let writer = tokio::sync::Mutex::new(BufWriter::new(write_half));

        let msg = Message::empty_ok_response(9);
        write_message(&writer, &msg).await.expect("write_message");

        let mut lines = BufReader::new(b).lines();
        let line = lines.next_line().await.expect("read line").expect("line");
        let parsed: Message = serde_json::from_str(&line).expect("parse line as message");
        match parsed {
            Message::Response { id, ok, .. } => {
                assert_eq!(id, 9);
                assert!(ok);
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn extract_path_param_requires_string_field() {
        let params = serde_json::json!({});
        let err = extract_path_param(&params, "repo").expect_err("missing field should error");
        assert!(err.contains("missing 'repo' parameter"));

        let params = serde_json::json!({ "repo": 42 });
        let err = extract_path_param(&params, "repo").expect_err("non-string field should error");
        assert!(err.contains("missing 'repo' parameter"));

        let params = serde_json::json!({ "repo": "/tmp/project" });
        let path = extract_path_param(&params, "repo").expect("valid path string");
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[tokio::test]
    async fn dispatch_request_handles_unknown_and_missing_params() {
        let (_tmp, daemon) = empty_daemon().await;

        let unknown = dispatch_request_test(&daemon, 1, "not_a_method", serde_json::json!({})).await;
        match unknown {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 1);
                assert!(!ok);
                assert!(data.is_none());
                assert!(error.unwrap_or_default().contains("unknown method"), "unexpected error payload");
            }
            other => panic!("expected response, got {other:?}"),
        }

        let missing_repo = dispatch_request_test(&daemon, 2, "get_state", serde_json::json!({})).await;
        match missing_repo {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 2);
                assert!(!ok);
                assert!(data.is_none());
                assert!(error.unwrap_or_default().contains("missing 'repo' parameter"), "unexpected error payload");
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_add_list_remove_repo_round_trip() {
        let (tmp, daemon) = empty_daemon().await;
        let repo_path = tmp.path().join("repo-a");
        std::fs::create_dir_all(&repo_path).unwrap();

        let add = dispatch_request_test(&daemon, 10, "add_repo", serde_json::json!({ "path": repo_path })).await;
        assert_ok_empty_response(add, 10);

        let list = dispatch_request_test(&daemon, 11, "list_repos", serde_json::json!({})).await;
        let listed: Vec<RepoInfo> = match list {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 11);
                assert!(ok, "list_repos should be ok: {error:?}");
                serde_json::from_value(data.expect("list data")).expect("parse repo list")
            }
            other => panic!("expected response, got {other:?}"),
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, repo_path);

        let remove = dispatch_request_test(&daemon, 12, "remove_repo", serde_json::json!({ "path": listed[0].path })).await;
        assert_ok_empty_response(remove, 12);
    }

    #[tokio::test]
    async fn dispatch_replay_since_with_bad_payload_degrades_to_empty_last_seen() {
        let (_tmp, daemon) = empty_daemon().await;

        let replay = dispatch_request_test(&daemon, 30, "replay_since", serde_json::json!({ "last_seen": "invalid-shape" })).await;
        match replay {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 30);
                assert!(ok, "replay_since should still succeed: {error:?}");
                let events: Vec<DaemonEvent> = serde_json::from_value(data.expect("replay events data")).expect("events");
                assert!(events.is_empty());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_request_execute_remote_routes_command_through_peer_manager() {
        let (_tmp, daemon) = empty_daemon().await;
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
        let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
        let sent = Arc::new(StdMutex::new(Vec::new()));
        peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(CapturePeerSender { sent: Arc::clone(&sent) }));

        let response = dispatch_request(
            &daemon,
            &peer_manager,
            &pending_remote_commands,
            &next_remote_command_id,
            40,
            "execute",
            serde_json::json!({
                "command": Command {
                    host: Some(HostName::new("feta")),
                    context_repo: None,
                    action: CommandAction::Refresh { repo: None },
                }
            }),
        )
        .await;

        let command_id = match response {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 40);
                assert!(ok, "remote execute should succeed: {error:?}");
                serde_json::from_value::<u64>(data.expect("command id")).expect("parse command id")
            }
            other => panic!("expected response, got {other:?}"),
        };

        assert!(command_id >= (1 << 62));
        assert_eq!(pending_remote_commands.lock().await.len(), 1);

        let sent = sent.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { requester_host, target_host, command, .. }) => {
                assert_eq!(requester_host, daemon.host_name());
                assert_eq!(target_host, &HostName::new("feta"));
                assert_eq!(command.as_ref(), &Command {
                    host: Some(HostName::new("feta")),
                    context_repo: None,
                    action: CommandAction::Refresh { repo: None }
                });
            }
            other => panic!("expected routed command request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_forwarded_command_proxies_lifecycle_and_response() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("create .git");
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::new("local")).await;
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let sent = Arc::new(StdMutex::new(Vec::new()));
        peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(CapturePeerSender { sent: Arc::clone(&sent) }));

        execute_forwarded_command(
            Arc::clone(&daemon),
            Arc::clone(&peer_manager),
            7,
            HostName::new("desktop"),
            HostName::new("relay"),
            Command { host: Some(daemon.host_name().clone()), context_repo: None, action: CommandAction::Refresh { repo: None } },
        )
        .await;

        let sent = sent.lock().expect("lock");
        assert!(sent.len() >= 3, "expected started event, finished event, and response");

        let mut saw_started = false;
        let mut saw_finished = false;
        let mut saw_response = false;

        for msg in sent.iter() {
            match msg {
                PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent { request_id, requester_host, responder_host, event, .. }) => {
                    assert_eq!(*request_id, 7);
                    assert_eq!(requester_host, &HostName::new("desktop"));
                    assert_eq!(responder_host, daemon.host_name());
                    match event.as_ref() {
                        CommandPeerEvent::Started { repo: event_repo, description } => {
                            assert_eq!(event_repo, &repo);
                            assert_eq!(description, "Refreshing...");
                            saw_started = true;
                        }
                        CommandPeerEvent::Finished { repo: event_repo, result } => {
                            assert_eq!(event_repo, &repo);
                            assert_eq!(result, &CommandResult::Refreshed { repos: vec![repo.clone()] });
                            saw_finished = true;
                        }
                        CommandPeerEvent::StepUpdate { .. } => {}
                    }
                }
                PeerWireMessage::Routed(RoutedPeerMessage::CommandResponse {
                    request_id, requester_host, responder_host, result, ..
                }) => {
                    assert_eq!(*request_id, 7);
                    assert_eq!(requester_host, &HostName::new("desktop"));
                    assert_eq!(responder_host, daemon.host_name());
                    assert_eq!(result.as_ref(), &CommandResult::Refreshed { repos: vec![repo.clone()] });
                    saw_response = true;
                }
                other => panic!("unexpected proxied message: {other:?}"),
            }
        }

        assert!(saw_started);
        assert!(saw_finished);
        assert!(saw_response);
    }

    #[tokio::test]
    async fn take_peer_data_rx_returns_some_once() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let mut server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        assert!(server.take_peer_data_rx().is_some(), "first call should return Some");
        assert!(server.take_peer_data_rx().is_none(), "second call should return None");
    }

    #[tokio::test]
    async fn daemon_server_replays_configured_hosts_as_disconnected() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.udder]\nhostname = \"udder\"\ndaemon_socket = \"/tmp/udder.sock\"\n\n[hosts.feta]\nhostname = \"feta\"\ndaemon_socket = \"/tmp/feta.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        let events = server.daemon.replay_since(&HashMap::new()).await.unwrap();
        let mut statuses: Vec<(HostName, PeerConnectionState)> = events
            .into_iter()
            .filter_map(|event| match event {
                DaemonEvent::PeerStatusChanged { host, status } => Some((host, status)),
                _ => None,
            })
            .collect();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(statuses, vec![
            (HostName::new("feta"), PeerConnectionState::Disconnected),
            (HostName::new("udder"), PeerConnectionState::Disconnected),
        ]);
    }

    fn test_peer_msg(host: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/tmp/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 0 },
        }
    }

    #[tokio::test]
    async fn handle_client_forwards_peer_data_and_registers_peer() {
        let (_tmp, daemon) = empty_daemon().await;
        let expected_local_host = daemon.host_name().clone();
        let daemon_events = daemon.subscribe();
        let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pm = Arc::clone(&peer_manager);
        let count_ref = Arc::clone(&client_count);
        let notify_ref = Arc::clone(&client_notify);
        let daemon_for_task = Arc::clone(&daemon);
        let handle = tokio::spawn(async move {
            let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
            let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
            handle_client(
                server_stream,
                daemon_for_task,
                shutdown_rx,
                peer_data_tx,
                pm,
                pending_remote_commands,
                next_remote_command_id,
                count_ref,
                notify_ref,
                peer_connected_tx,
            )
            .await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut reader = BufReader::new(read_half).lines();
        let mut writer = BufWriter::new(write_half);

        let hello =
            Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("remote-host"), session_id: uuid::Uuid::nil() };
        flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

        let line = reader.next_line().await.expect("read hello response").expect("hello line");
        let hello_back: Message = serde_json::from_str(&line).expect("parse hello");
        match hello_back {
            Message::Hello { protocol_version, host_name, .. } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(host_name, expected_local_host);
            }
            other => panic!("expected hello response, got {other:?}"),
        }

        let mut daemon_events = daemon_events;
        let connected_event = tokio::time::timeout(Duration::from_secs(2), daemon_events.recv())
            .await
            .expect("timeout waiting for peer status")
            .expect("peer status event");
        match connected_event {
            DaemonEvent::PeerStatusChanged { host, status } => {
                assert_eq!(host, HostName::new("remote-host"));
                assert_eq!(status, PeerConnectionState::Connected);
            }
            other => panic!("expected peer status event, got {other:?}"),
        }

        // Send a peer message from the client side
        let peer_msg = test_peer_msg("remote-host");
        let wire_msg = Message::Peer(Box::new(PeerWireMessage::Data(peer_msg.clone())));
        flotilla_protocol::framing::write_message_line(&mut writer, &wire_msg).await.expect("write peer message");

        // The server should forward the peer envelope
        let received = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
            .await
            .expect("timeout waiting for peer data")
            .expect("channel closed");
        assert_eq!(received.connection_peer, HostName::new("remote-host"));
        assert_eq!(received.connection_generation, 1);
        match received.msg {
            PeerWireMessage::Data(msg) => {
                assert_eq!(msg.origin_host, HostName::new("remote-host"));
            }
            other => panic!("expected data message, got {other:?}"),
        }

        {
            let pm = peer_manager.lock().await;
            assert_eq!(pm.current_generation(&HostName::new("remote-host")), Some(1));
        }
        assert_eq!(client_count.load(Ordering::SeqCst), 0);

        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let disconnected_event = tokio::time::timeout(Duration::from_secs(2), daemon_events.recv())
            .await
            .expect("timeout waiting for peer disconnect")
            .expect("peer disconnect event");
        match disconnected_event {
            DaemonEvent::PeerStatusChanged { host, status } => {
                assert_eq!(host, HostName::new("remote-host"));
                assert_eq!(status, PeerConnectionState::Disconnected);
            }
            other => panic!("expected peer disconnect event, got {other:?}"),
        }

        let pm = peer_manager.lock().await;
        assert!(pm.current_generation(&HostName::new("remote-host")).is_none(), "peer should be disconnected after socket close");
    }

    #[tokio::test]
    async fn send_local_to_peer_sends_host_summary_for_empty_daemon() {
        let (_tmp, daemon) = empty_daemon().await;
        let peer = HostName::new("remote-host");
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(CapturePeerSender { sent: Arc::clone(&sent) });
        let generation = {
            let mut pm = peer_manager.lock().await;
            ensure_test_connection_generation(&mut pm, &peer, || Arc::clone(&sender))
        };
        let mut clock = VectorClock::default();
        let host_name = daemon.host_name().clone();

        let sent_any = send_local_to_peer(&daemon, &peer_manager, &host_name, &mut clock, &peer, generation).await;

        assert!(sent_any, "host summary should count as initial peer sync");
        let sent = sent.lock().expect("lock");
        assert!(matches!(&sent[0], PeerWireMessage::HostSummary(summary) if summary.host_name == host_name));
    }

    #[tokio::test]
    async fn peer_manager_initialized_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();

        // Write daemon config with a custom host name
        std::fs::write(base.join("daemon.toml"), "host_name = \"test-host\"\n").unwrap();

        // Write hosts config with one peer
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = \"remote\"\ndaemon_socket = \"/tmp/daemon.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        // PeerManager should be initialized and accessible
        let pm = server.peer_manager.lock().await;
        // peer_data is empty since no data has been received yet
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn peer_manager_default_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        // Should still have a PeerManager with no peers
        let pm = server.peer_manager.lock().await;
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn daemon_server_new_returns_error_for_invalid_hosts_config() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = [\ndaemon_socket = \"/tmp/daemon.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let result = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await;

        match result {
            Ok(_) => panic!("invalid hosts config should return startup error"),
            Err(err) => assert!(err.contains("failed to parse")),
        }
    }

    #[tokio::test]
    async fn handle_client_relays_outbound_peer_messages() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pm = Arc::clone(&peer_manager);
        let count_ref = Arc::clone(&client_count);
        let notify_ref = Arc::clone(&client_notify);
        let handle = tokio::spawn(async move {
            let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
            let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
            handle_client(
                server_stream,
                daemon,
                shutdown_rx,
                peer_data_tx,
                pm,
                pending_remote_commands,
                next_remote_command_id,
                count_ref,
                notify_ref,
                peer_connected_tx,
            )
            .await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut reader = BufReader::new(read_half).lines();
        let mut writer = BufWriter::new(write_half);

        let hello =
            Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("relay-target"), session_id: uuid::Uuid::nil() };
        flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");
        let _ = reader.next_line().await.expect("read hello").expect("line");

        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let pm = peer_manager.lock().await;
            pm.send_to(&HostName::new("relay-target"), PeerWireMessage::Data(test_peer_msg("other-host"))).await.expect("send relay");
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut found_relay = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(1), reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let msg: Message = serde_json::from_str(&line).expect("parse");
                    if let Message::Peer(peer_msg) = msg {
                        if let PeerWireMessage::Data(peer_msg) = *peer_msg {
                            assert_eq!(peer_msg.origin_host, HostName::new("other-host"));
                            found_relay = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_relay, "should have received relayed peer message");

        // Clean up
        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn duplicate_inbound_peer_receives_goodbye_on_rejection() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream_a, server_stream_a) = tokio::net::UnixStream::pair().expect("pair a");
        let (client_stream_b, server_stream_b) = tokio::net::UnixStream::pair().expect("pair b");

        let expected_server_host = daemon.host_name().clone();
        let daemon_a = Arc::clone(&daemon);
        let daemon_b = Arc::clone(&daemon);
        let pm_a = Arc::clone(&peer_manager);
        let pm_b = Arc::clone(&peer_manager);
        let tx_a = peer_data_tx.clone();
        let tx_b = peer_data_tx.clone();
        let count_a = Arc::clone(&client_count);
        let count_b = Arc::clone(&client_count);
        let notify_a = Arc::clone(&client_notify);
        let notify_b = Arc::clone(&client_notify);
        let shutdown_rx_a = shutdown_rx.clone();
        let shutdown_rx_b = shutdown_rx.clone();
        let peer_connected_tx_a = peer_connected_tx.clone();
        let peer_connected_tx_b = peer_connected_tx.clone();

        let handle_a = tokio::spawn(async move {
            let pending_remote_commands_a = Arc::new(Mutex::new(HashMap::new()));
            let next_remote_command_id_a = Arc::new(AtomicU64::new(1 << 62));
            handle_client(
                server_stream_a,
                daemon_a,
                shutdown_rx_a,
                tx_a,
                pm_a,
                pending_remote_commands_a,
                next_remote_command_id_a,
                count_a,
                notify_a,
                peer_connected_tx_a,
            )
            .await;
        });
        let handle_b = tokio::spawn(async move {
            let pending_remote_commands_b = Arc::new(Mutex::new(HashMap::new()));
            let next_remote_command_id_b = Arc::new(AtomicU64::new(1 << 62));
            handle_client(
                server_stream_b,
                daemon_b,
                shutdown_rx_b,
                tx_b,
                pm_b,
                pending_remote_commands_b,
                next_remote_command_id_b,
                count_b,
                notify_b,
                peer_connected_tx_b,
            )
            .await;
        });

        async fn send_peer_hello(
            stream: tokio::net::UnixStream,
            expected_server_host: &HostName,
        ) -> (tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>, tokio::io::BufWriter<tokio::net::unix::OwnedWriteHalf>)
        {
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half).lines();
            let mut writer = BufWriter::new(write_half);
            let hello =
                Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("peer"), session_id: uuid::Uuid::nil() };
            flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

            let line = reader.next_line().await.expect("read hello response").expect("hello response line");
            let msg: Message = serde_json::from_str(&line).expect("parse hello response");
            match msg {
                Message::Hello { host_name, .. } => {
                    assert_eq!(host_name, expected_server_host.clone())
                }
                other => panic!("expected hello response, got {other:?}"),
            }

            (reader, writer)
        }

        let (_reader_a, writer_a) = send_peer_hello(client_stream_a, &expected_server_host).await;
        let (mut reader_b, writer_b) = send_peer_hello(client_stream_b, &expected_server_host).await;

        let goodbye = tokio::time::timeout(Duration::from_secs(2), reader_b.next_line())
            .await
            .expect("timeout waiting for goodbye")
            .expect("read goodbye line")
            .expect("goodbye line");
        let goodbye_msg: Message = serde_json::from_str(&goodbye).expect("parse goodbye");
        match goodbye_msg {
            Message::Peer(inner) => match *inner {
                PeerWireMessage::Goodbye { reason: flotilla_protocol::GoodbyeReason::Superseded } => {}
                other => panic!("expected superseded goodbye, got {other:?}"),
            },
            other => panic!("expected peer goodbye, got {other:?}"),
        }

        drop(writer_a);
        drop(writer_b);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle_a).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), handle_b).await;
    }

    #[tokio::test]
    async fn clear_peer_data_rebuilds_remote_only_repo_without_stale_first_event() {
        let (_tmp, daemon) = empty_daemon().await;
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let repo_identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
        let repo_path = PathBuf::from("/srv/remote-only");

        {
            let mut pm = peer_manager.lock().await;
            assert_eq!(
                handle_test_peer_data(
                    &mut pm,
                    peer_snapshot("peer-a", &repo_identity, &repo_path, "/srv/peer-a/remote-only", "feature-a",),
                    || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
                )
                .await,
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
            assert_eq!(
                handle_test_peer_data(
                    &mut pm,
                    peer_snapshot("peer-b", &repo_identity, &repo_path, "/srv/peer-b/remote-only", "feature-b",),
                    || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
                )
                .await,
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
        }

        let synthetic = crate::peer::synthetic_repo_path(&HostName::new("peer-a"), &repo_path);
        let merged = {
            let pm = peer_manager.lock().await;
            let peers: Vec<(HostName, ProviderData)> = pm
                .get_peer_data()
                .iter()
                .filter_map(|(host, repos)| repos.get(&repo_identity).map(|state| (host.clone(), state.provider_data.clone())))
                .collect();
            crate::peer::merge_provider_data(
                &ProviderData::default(),
                daemon.host_name(),
                &peers.iter().map(|(h, d)| (h.clone(), d)).collect::<Vec<_>>(),
            )
        };
        daemon.add_virtual_repo(synthetic.clone(), merged).await.expect("add virtual repo");
        daemon
            .set_peer_providers(&synthetic, vec![
                (HostName::new("peer-a"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only"), checkout("feature-a"))]),
                    ..Default::default()
                }),
                (HostName::new("peer-b"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only"), checkout("feature-b"))]),
                    ..Default::default()
                }),
            ])
            .await;
        {
            let mut pm = peer_manager.lock().await;
            pm.register_remote_repo(repo_identity.clone(), synthetic.clone());
        }

        let mut rx = daemon.subscribe();
        let gen_a = {
            let mut pm = peer_manager.lock().await;
            ensure_test_connection_generation(&mut pm, &HostName::new("peer-a"), || {
                Arc::new(super::SocketPeerSender { tx: tokio::sync::Mutex::new(Some(mpsc::channel(1).0)) }) as Arc<dyn PeerSender>
            })
        };

        disconnect_peer_and_rebuild(&peer_manager, &daemon, &HostName::new("peer-a"), gen_a).await;

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for first event")
            .expect("broadcast channel should stay open");

        let stale_key = HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only");
        let remaining_key = HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only");
        match event {
            DaemonEvent::SnapshotFull(snapshot) => {
                assert_eq!(snapshot.repo, synthetic);
                assert!(
                    !snapshot.providers.checkouts.contains_key(&stale_key),
                    "first snapshot after disconnect should not include stale peer-a checkout"
                );
                assert_eq!(snapshot.providers.checkouts[&remaining_key].branch, "feature-b");
            }
            DaemonEvent::SnapshotDelta(delta) => {
                assert_eq!(delta.repo, synthetic);
                assert!(
                    delta.changes.iter().any(|change| matches!(
                        change,
                        flotilla_protocol::Change::Checkout {
                            key,
                            op: flotilla_protocol::EntryOp::Removed
                        } if key == &stale_key
                    )),
                    "first delta after disconnect should remove stale peer-a checkout"
                );
            }
            other => panic!("expected snapshot event, got {other:?}"),
        }
    }
}
