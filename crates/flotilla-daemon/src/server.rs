use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tracing::{debug, error, info, warn};

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{
    Command, DaemonEvent, HostName, Message, PeerConnectionState, PeerDataMessage,
};

use crate::peer::{HandleResult, PeerManager, SshTransport};

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
    /// Channel for inbound peer data messages forwarded from connected peer clients.
    peer_data_tx: mpsc::Sender<PeerDataMessage>,
    peer_data_rx: Option<mpsc::Receiver<PeerDataMessage>>,
    /// Map of connected peer clients, keyed by their host name.
    /// Each entry holds a sender that can push messages back to that peer's socket.
    peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>>,
    /// Manages connections to remote peer hosts and stores their provider data.
    peer_manager: Arc<Mutex<PeerManager>>,
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
    ) -> Self {
        let daemon_config = config.load_daemon_config();
        let host_name = daemon_config
            .host_name
            .map(HostName::new)
            .unwrap_or_else(HostName::local);
        let hosts_config = config.load_hosts();

        let peer_count = hosts_config.hosts.len();
        let mut peer_manager = PeerManager::new(host_name.clone());
        for (name, host_config) in hosts_config.hosts {
            let peer_host = HostName::new(&name);
            if peer_host == host_name {
                warn!(
                    host = %host_name,
                    "peer config uses same name as local host — messages will be ignored"
                );
            }
            let transport = SshTransport::new(peer_host.clone(), host_config);
            peer_manager.add_peer(peer_host, Box::new(transport));
        }

        info!(
            host = %host_name,
            %peer_count,
            "initialized PeerManager"
        );

        let daemon = InProcessDaemon::new_with_options(
            repo_paths,
            config,
            daemon_config.follower,
            host_name.clone(),
        )
        .await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);

        Self {
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
            peer_clients: Arc::new(Mutex::new(HashMap::new())),
            peer_manager: Arc::new(Mutex::new(peer_manager)),
        }
    }

    /// Take the receiver for inbound peer data messages.
    ///
    /// Returns `Some` on the first call, `None` thereafter. The PeerManager
    /// consumes this to process data arriving from peer daemons.
    pub fn take_peer_data_rx(&mut self) -> Option<mpsc::Receiver<PeerDataMessage>> {
        self.peer_data_rx.take()
    }

    /// Get a handle to the peer clients map.
    ///
    /// The PeerManager uses this to send `Message::PeerData` back to specific
    /// connected peer daemons.
    pub fn peer_clients(&self) -> Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> {
        Arc::clone(&self.peer_clients)
    }

    /// Run the server, accepting connections until idle timeout or shutdown signal.
    pub async fn run(mut self) -> Result<(), String> {
        // Clean up stale socket file before binding
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| format!("failed to remove stale socket: {e}"))?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create socket directory: {e}"))?;
        }

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| format!("failed to bind socket: {e}"))?;

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
        let peer_clients = self.peer_clients;

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

                    info!(
                        timeout_secs = idle_timeout.as_secs(),
                        "no clients connected, waiting before shutdown"
                    );

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
        let peer_data_tx_for_ssh = peer_data_tx.clone();
        let peer_daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            if let Some(mut rx) = peer_data_rx {
                // Connect all peers and collect initial receivers into a map
                let mut initial_rx_map: HashMap<HostName, mpsc::Receiver<PeerDataMessage>> =
                    HashMap::new();
                let peer_names = {
                    let mut pm = peer_manager.lock().await;
                    let names: Vec<HostName> = pm.peers().keys().cloned().collect();
                    for (name, rx) in pm.connect_all().await {
                        initial_rx_map.insert(name, rx);
                    }
                    names
                };

                // Spawn resilient per-peer forwarding tasks with reconnect loop.
                // On disconnect, stale peer data is cleared from the daemon overlay
                // so the UI doesn't show checkouts from unreachable hosts.
                // Emit initial connecting status for all peers
                for name in &peer_names {
                    peer_daemon.send_event(DaemonEvent::PeerStatusChanged {
                        host: name.clone(),
                        status: PeerConnectionState::Connecting,
                    });
                }

                // Emit connected/disconnected based on initial connect results
                for name in &peer_names {
                    let status = if initial_rx_map.contains_key(name) {
                        PeerConnectionState::Connected
                    } else {
                        PeerConnectionState::Disconnected
                    };
                    peer_daemon.send_event(DaemonEvent::PeerStatusChanged {
                        host: name.clone(),
                        status,
                    });
                }

                for peer_name in peer_names {
                    let tx = peer_data_tx_for_ssh.clone();
                    let pm = Arc::clone(&peer_manager);
                    let daemon_for_cleanup = Arc::clone(&peer_daemon);
                    let initial_rx = initial_rx_map.remove(&peer_name);

                    tokio::spawn(async move {
                        // Forward from initial connection if available
                        if let Some(mut inbound_rx) = initial_rx {
                            if !forward_until_closed(&tx, &mut inbound_rx, &peer_name).await {
                                return; // Main channel closed, stop entirely
                            }
                            info!(peer = %peer_name, "SSH connection dropped, will reconnect");
                            daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                host: peer_name.clone(),
                                status: PeerConnectionState::Disconnected,
                            });
                            clear_peer_data(&pm, &daemon_for_cleanup, &peer_name).await;
                        }

                        // Reconnect loop with exponential backoff
                        let mut attempt: u32 = 1;
                        loop {
                            daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                host: peer_name.clone(),
                                status: PeerConnectionState::Reconnecting,
                            });
                            let delay = crate::peer::SshTransport::backoff_delay(attempt);
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
                                Ok(mut inbound_rx) => {
                                    info!(peer = %peer_name, "reconnected successfully");
                                    daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                        host: peer_name.clone(),
                                        status: PeerConnectionState::Connected,
                                    });
                                    attempt = 1;
                                    if !forward_until_closed(&tx, &mut inbound_rx, &peer_name).await
                                    {
                                        return;
                                    }
                                    info!(
                                        peer = %peer_name,
                                        "SSH connection dropped, will reconnect"
                                    );
                                    daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                        host: peer_name.clone(),
                                        status: PeerConnectionState::Disconnected,
                                    });
                                    clear_peer_data(&pm, &daemon_for_cleanup, &peer_name).await;
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

                while let Some(msg) = rx.recv().await {
                    let origin = msg.origin_host.clone();
                    let repo_path = msg.repo_path.clone();

                    let mut pm = peer_manager.lock().await;

                    // Relay to other peers before consuming the message
                    pm.relay(&origin, &msg).await;

                    // Then handle locally
                    let result = pm.handle_peer_data(msg);
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
                                    let mut pm2 = peer_manager.lock().await;
                                    pm2.register_remote_repo(updated_repo_id.clone(), synthetic);
                                }
                            }
                        }
                        HandleResult::ResyncRequested {
                            from,
                            repo,
                            since_seq: _,
                        } => {
                            let local_host = pm.local_host().clone();

                            // Send local-only providers (not merged) back to requesting peer
                            if let Some(local_path) = peer_daemon.find_repo_by_identity(&repo).await
                            {
                                if let Some((local_providers, seq)) =
                                    peer_daemon.get_local_providers(&local_path).await
                                {
                                    reply_clock.tick(&local_host);
                                    let response = PeerDataMessage {
                                        origin_host: local_host,
                                        repo_identity: repo,
                                        repo_path,
                                        clock: reply_clock.clone(),
                                        kind: flotilla_protocol::PeerDataKind::Snapshot {
                                            data: Box::new(local_providers),
                                            seq,
                                        },
                                    };
                                    // Send via PeerManager transport (works for SSH peers)
                                    if let Err(e) = pm.send_to(&from, response).await {
                                        warn!(
                                            peer = %from,
                                            err = %e,
                                            "failed to send resync response"
                                        );
                                    }
                                }
                            }
                            drop(pm);
                        }
                        HandleResult::NeedsResync { from, repo } => {
                            let local_host = pm.local_host().clone();

                            reply_clock.tick(&local_host);
                            let request = PeerDataMessage {
                                origin_host: local_host,
                                repo_identity: repo,
                                repo_path,
                                clock: reply_clock.clone(),
                                kind: flotilla_protocol::PeerDataKind::RequestResync {
                                    since_seq: 0,
                                },
                            };
                            // Send via PeerManager transport (works for SSH peers)
                            if let Err(e) = pm.send_to(&from, request).await {
                                warn!(
                                    peer = %from,
                                    err = %e,
                                    "failed to send resync request"
                                );
                            }
                            drop(pm);
                        }
                        HandleResult::Ignored => {}
                    }
                }
            }
        });

        // Spawn outbound task: forward local snapshots to peers as PeerDataMessages.
        // Uses local-only providers (no peer overlay) to avoid echoing peer data back.
        // Maintains a persistent vector clock so each message has a strictly increasing clock.
        // Sends to both configured SSH transports (PeerManager) and inbound peer clients
        // (peers that connected to us via socket).
        let outbound_daemon = Arc::clone(&daemon);
        let outbound_peer_clients = Arc::clone(&peer_clients);
        tokio::spawn(async move {
            let mut event_rx = outbound_daemon.subscribe();
            let mut outbound_clock = flotilla_protocol::VectorClock::default();
            let host_name = outbound_daemon.host_name().clone();

            loop {
                match event_rx.recv().await {
                    Ok(DaemonEvent::SnapshotFull(snapshot)) => {
                        send_local_to_peers(
                            &outbound_daemon,
                            &outbound_peer_manager,
                            &outbound_peer_clients,
                            &host_name,
                            &mut outbound_clock,
                            &snapshot.repo,
                        )
                        .await;
                    }
                    Ok(DaemonEvent::SnapshotDelta(delta)) => {
                        // Deltas also indicate local data changed — send full
                        // local providers to peers (we don't replicate deltas).
                        send_local_to_peers(
                            &outbound_daemon,
                            &outbound_peer_manager,
                            &outbound_peer_clients,
                            &host_name,
                            &mut outbound_clock,
                            &delta.repo,
                        )
                        .await;
                    }
                    Ok(_) => {} // Ignore non-data events
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "outbound peer event subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        // SIGTERM handler
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");

        // Accept loop
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let count = client_count.fetch_add(1, Ordering::SeqCst) + 1;
                            info!(%count, "client connected");
                            client_notify.notify_one();

                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();
                            let peer_data_tx = peer_data_tx.clone();
                            let peer_clients = Arc::clone(&peer_clients);

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_clients,
                                )
                                .await;
                                let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
                                info!(%count, "client disconnected");
                                client_notify.notify_one();
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

/// Clear a disconnected peer's data from PeerManager and daemon overlays.
///
/// Called when an SSH connection drops. Removes the peer's stored snapshots
/// and updates the daemon's peer overlay so the UI no longer shows stale
/// checkouts and terminals from the unreachable host. Remote-only virtual
/// repos that no longer have any backing peer data are removed entirely.
async fn clear_peer_data(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &HostName,
) {
    let affected_repos = {
        let mut pm = peer_manager.lock().await;
        pm.remove_peer_data(peer_name)
    };

    // Rebuild peer overlays for each affected repo
    for repo_id in affected_repos {
        if let Some(local_path) = daemon.find_repo_by_identity(&repo_id).await {
            // Local repo — rebuild its peer overlay from remaining peers
            let peers: Vec<(HostName, flotilla_protocol::ProviderData)> = {
                let pm = peer_manager.lock().await;
                pm.get_peer_data()
                    .iter()
                    .filter_map(|(host, repos)| {
                        repos
                            .get(&repo_id)
                            .map(|state| (host.clone(), state.provider_data.clone()))
                    })
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
                    .filter_map(|(host, repos)| {
                        repos
                            .get(&repo_id)
                            .map(|state| (host.clone(), state.provider_data.clone()))
                    })
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

/// Send local-only provider data to all peers for a given repo.
///
/// Called by the outbound task whenever any snapshot event (full or delta)
/// indicates local data changed. Always sends a full snapshot to peers —
/// peer replication doesn't use deltas.
///
/// Sends to both configured SSH transports (outbound peers we connected to)
/// and inbound peer clients (peers that connected to our socket).
async fn send_local_to_peers(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    peer_clients: &Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    repo_path: &std::path::Path,
) {
    let Some(identity) = daemon.find_identity_for_path(repo_path).await else {
        return;
    };
    let Some((local_providers, seq)) = daemon.get_local_providers(repo_path).await else {
        return;
    };

    clock.tick(host_name);
    let msg = PeerDataMessage {
        origin_host: host_name.clone(),
        repo_identity: identity,
        repo_path: repo_path.to_path_buf(),
        clock: clock.clone(),
        kind: flotilla_protocol::PeerDataKind::Snapshot {
            data: Box::new(local_providers),
            seq,
        },
    };

    // Send to outbound peers (SSH transports we connected to)
    let pm = peer_manager.lock().await;
    for transport in pm.peers().values() {
        let _ = transport.send(msg.clone()).await;
    }
    drop(pm);

    // Send to inbound peer clients (peers that connected to our socket)
    let clients = peer_clients.lock().await;
    if !clients.is_empty() {
        let wire_msg = Message::PeerData(Box::new(msg));
        for (name, tx) in clients.iter() {
            debug!(peer = %name, "sending snapshot to inbound peer client");
            let _ = tx.send(wire_msg.clone()).await;
        }
    }
}

/// Forward messages from an inbound receiver to the shared peer_data channel.
///
/// Returns `true` if the inbound receiver was closed (connection dropped),
/// `false` if the outbound channel was closed (daemon shutting down).
async fn forward_until_closed(
    tx: &mpsc::Sender<PeerDataMessage>,
    inbound_rx: &mut mpsc::Receiver<PeerDataMessage>,
    peer_name: &HostName,
) -> bool {
    while let Some(msg) = inbound_rx.recv().await {
        if let Err(e) = tx.send(msg).await {
            warn!(peer = %peer_name, err = %e, "forwarding channel closed");
            return false;
        }
    }
    true
}

/// Write a JSON message followed by a newline to the writer.
async fn write_message(
    writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    msg: &Message,
) -> Result<(), ()> {
    let mut w = writer.lock().await;
    let json = serde_json::to_string(msg).map_err(|_| ())?;
    w.write_all(json.as_bytes()).await.map_err(|_| ())?;
    w.write_all(b"\n").await.map_err(|_| ())?;
    w.flush().await.map_err(|_| ())?;
    Ok(())
}

/// Handle a single client connection.
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<PeerDataMessage>,
    peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>>,
) {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write_half)));

    // Channel for outbound messages to this specific client (used for peer relay).
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);

    // Spawn event forwarder task
    let event_writer = Arc::clone(&writer);
    let mut event_rx = daemon.subscribe();
    let event_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let msg = Message::Event {
                        event: Box::new(event),
                    };
                    if write_message(&event_writer, &msg).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "event subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Spawn outbound relay task — writes messages from outbound_rx to the socket.
    let relay_writer = Arc::clone(&writer);
    let relay_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if write_message(&relay_writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Track whether this client has registered as a peer, and under what name.
    let mut peer_host_name: Option<HostName> = None;

    // Read request lines and dispatch
    let mut lines = reader.lines();
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
                                let response = dispatch_request(&daemon, id, &method, params).await;
                                if write_message(&writer, &response).await.is_err() {
                                    break;
                                }
                            }
                            Message::PeerData(peer_msg) => {
                                let origin = peer_msg.origin_host.clone();

                                // Register this client as a peer on first PeerData message.
                                if peer_host_name.is_none() {
                                    debug!(host = %origin, "registering peer client");
                                    peer_host_name = Some(origin.clone());
                                    let mut clients = peer_clients.lock().await;
                                    if clients.contains_key(&origin) {
                                        warn!(
                                            host = %origin,
                                            "duplicate peer hostname — overwriting previous connection"
                                        );
                                    }
                                    clients.insert(origin, outbound_tx.clone());
                                }

                                if let Err(e) = peer_data_tx.send(*peer_msg).await {
                                    warn!(err = %e, "failed to forward peer data");
                                }
                            }
                            other => {
                                warn!(msg = ?other, "unexpected message type from client");
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — client disconnected
                        break;
                    }
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

    // Unregister peer client on disconnect.
    if let Some(host) = peer_host_name {
        debug!(%host, "unregistering peer client");
        peer_clients.lock().await.remove(&host);
    }

    // Abort the event forwarder and relay tasks
    event_task.abort();
    relay_task.abort();
}

/// Dispatch a request to the appropriate `DaemonHandle` method.
async fn dispatch_request(
    daemon: &Arc<InProcessDaemon>,
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
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            let command: Command = match params
                .get("command")
                .cloned()
                .ok_or_else(|| "missing 'command' field".to_string())
                .and_then(|v| {
                    serde_json::from_value(v).map_err(|e| format!("invalid command: {e}"))
                }) {
                Ok(cmd) => cmd,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.execute(&repo, command).await {
                Ok(command_id) => Message::ok_response(id, &command_id),
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
            let last_seen: std::collections::HashMap<std::path::PathBuf, u64> = params
                .get("last_seen")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_else(|| {
                    warn!("replay_since: failed to parse last_seen, returning full snapshots");
                    std::collections::HashMap::new()
                });
            match daemon.replay_since(&last_seen).await {
                Ok(events) => Message::ok_response(id, &events),
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
    params
        .get(field)
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing '{field}' parameter"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::{
        Checkout, DaemonEvent, HostName, HostPath, PeerDataKind, PeerDataMessage, ProviderData,
        RepoIdentity, RepoInfo, VectorClock,
    };
    use indexmap::IndexMap;
    use std::path::Path;

    fn assert_ok_empty_response(msg: Message, expected_id: u64) {
        match msg {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
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
        let daemon = InProcessDaemon::new(vec![], config).await;
        (tmp, daemon)
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

    fn peer_snapshot(
        host: &str,
        repo_identity: &RepoIdentity,
        repo_path: &Path,
        checkout_path: &str,
        branch: &str,
    ) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: repo_identity.clone(),
            repo_path: repo_path.to_path_buf(),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData {
                    checkouts: IndexMap::from([(
                        HostPath::new(HostName::new(host), checkout_path),
                        checkout(branch),
                    )]),
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

        let unknown = dispatch_request(&daemon, 1, "not_a_method", serde_json::json!({})).await;
        match unknown {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 1);
                assert!(!ok);
                assert!(data.is_none());
                assert!(
                    error.unwrap_or_default().contains("unknown method"),
                    "unexpected error payload"
                );
            }
            other => panic!("expected response, got {other:?}"),
        }

        let missing_repo = dispatch_request(&daemon, 2, "get_state", serde_json::json!({})).await;
        match missing_repo {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 2);
                assert!(!ok);
                assert!(data.is_none());
                assert!(
                    error
                        .unwrap_or_default()
                        .contains("missing 'repo' parameter"),
                    "unexpected error payload"
                );
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_add_list_remove_repo_round_trip() {
        let (tmp, daemon) = empty_daemon().await;
        let repo_path = tmp.path().join("repo-a");
        std::fs::create_dir_all(&repo_path).unwrap();

        let add = dispatch_request(
            &daemon,
            10,
            "add_repo",
            serde_json::json!({ "path": repo_path }),
        )
        .await;
        assert_ok_empty_response(add, 10);

        let list = dispatch_request(&daemon, 11, "list_repos", serde_json::json!({})).await;
        let listed: Vec<RepoInfo> = match list {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 11);
                assert!(ok, "list_repos should be ok: {error:?}");
                serde_json::from_value(data.expect("list data")).expect("parse repo list")
            }
            other => panic!("expected response, got {other:?}"),
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, repo_path);

        let remove = dispatch_request(
            &daemon,
            12,
            "remove_repo",
            serde_json::json!({ "path": listed[0].path }),
        )
        .await;
        assert_ok_empty_response(remove, 12);
    }

    #[tokio::test]
    async fn dispatch_replay_since_with_bad_payload_degrades_to_empty_last_seen() {
        let (_tmp, daemon) = empty_daemon().await;

        let replay = dispatch_request(
            &daemon,
            30,
            "replay_since",
            serde_json::json!({ "last_seen": "invalid-shape" }),
        )
        .await;
        match replay {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 30);
                assert!(ok, "replay_since should still succeed: {error:?}");
                let events: Vec<DaemonEvent> =
                    serde_json::from_value(data.expect("replay events data")).expect("events");
                assert!(events.is_empty());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn take_peer_data_rx_returns_some_once() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let mut server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        assert!(
            server.take_peer_data_rx().is_some(),
            "first call should return Some"
        );
        assert!(
            server.take_peer_data_rx().is_none(),
            "second call should return None"
        );
    }

    #[tokio::test]
    async fn peer_clients_accessor_returns_shared_map() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        let map = server.peer_clients();
        assert!(map.lock().await.is_empty());

        // Inserting via one handle is visible via another
        let map2 = server.peer_clients();
        let (tx, _rx) = mpsc::channel(1);
        map.lock().await.insert(HostName::new("laptop"), tx);
        assert_eq!(map2.lock().await.len(), 1);
    }

    fn test_peer_msg(host: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/tmp/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 0 },
        }
    }

    #[tokio::test]
    async fn handle_client_forwards_peer_data_and_registers_peer() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
        let peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pc = Arc::clone(&peer_clients);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon, shutdown_rx, peer_data_tx, pc).await;
        });

        // Send a PeerData message from the client side
        let peer_msg = test_peer_msg("remote-host");
        let wire_msg = Message::PeerData(Box::new(peer_msg.clone()));
        let json = serde_json::to_string(&wire_msg).expect("serialize");

        let (read_half, write_half) = client_stream.into_split();
        let mut writer = BufWriter::new(write_half);
        writer.write_all(json.as_bytes()).await.expect("write");
        writer.write_all(b"\n").await.expect("newline");
        writer.flush().await.expect("flush");

        // The server should forward the peer data
        let received = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
            .await
            .expect("timeout waiting for peer data")
            .expect("channel closed");
        assert_eq!(received.origin_host, HostName::new("remote-host"));

        // The peer should now be registered in peer_clients
        // Give a brief moment for the lock to be released
        tokio::time::sleep(Duration::from_millis(50)).await;
        let map = peer_clients.lock().await;
        assert!(
            map.contains_key(&HostName::new("remote-host")),
            "peer should be registered after sending PeerData"
        );
        drop(map);

        // Drop the writer to close the connection, triggering cleanup
        drop(writer);
        drop(read_half);

        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // After disconnect, the peer should be unregistered
        let map = peer_clients.lock().await;
        assert!(
            !map.contains_key(&HostName::new("remote-host")),
            "peer should be unregistered after disconnect"
        );
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
            "[hosts.remote]\nhostname = \"10.0.0.5\"\ndaemon_socket = \"/tmp/daemon.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        // PeerManager should be initialized and accessible
        let pm = server.peer_manager.lock().await;
        // peer_data is empty since no data has been received yet
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn peer_manager_default_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        // Should still have a PeerManager with no peers
        let pm = server.peer_manager.lock().await;
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn handle_client_relays_outbound_peer_messages() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pc = Arc::clone(&peer_clients);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon, shutdown_rx, peer_data_tx, pc).await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut writer = BufWriter::new(write_half);

        // Send a PeerData message to register as a peer
        let peer_msg = test_peer_msg("relay-target");
        let wire_msg = Message::PeerData(Box::new(peer_msg));
        let json = serde_json::to_string(&wire_msg).expect("serialize");
        writer.write_all(json.as_bytes()).await.expect("write");
        writer.write_all(b"\n").await.expect("newline");
        writer.flush().await.expect("flush");

        // Wait for registration
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now push a message via peer_clients to relay back to this client
        let relay_msg = Message::PeerData(Box::new(test_peer_msg("other-host")));
        {
            let map = peer_clients.lock().await;
            let sender = map
                .get(&HostName::new("relay-target"))
                .expect("peer should be registered");
            sender.send(relay_msg).await.expect("send relay");
        }

        // Read from the client side — should receive the relayed message
        let reader = BufReader::new(read_half);
        let mut lines = reader.lines();

        // We may receive event messages (snapshots) before our peer data relay,
        // so loop until we find the PeerData message.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut found_relay = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let msg: Message = serde_json::from_str(&line).expect("parse");
                    if let Message::PeerData(peer_msg) = msg {
                        assert_eq!(peer_msg.origin_host, HostName::new("other-host"));
                        found_relay = true;
                        break;
                    }
                    // Skip non-PeerData messages (events, etc.)
                }
                _ => break,
            }
        }
        assert!(found_relay, "should have received relayed PeerData message");

        // Clean up
        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn clear_peer_data_rebuilds_remote_only_repo_without_stale_first_event() {
        let (_tmp, daemon) = empty_daemon().await;
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let repo_identity = RepoIdentity {
            authority: "github.com".into(),
            path: "owner/remote-only".into(),
        };
        let repo_path = PathBuf::from("/srv/remote-only");

        {
            let mut pm = peer_manager.lock().await;
            assert_eq!(
                pm.handle_peer_data(peer_snapshot(
                    "peer-a",
                    &repo_identity,
                    &repo_path,
                    "/srv/peer-a/remote-only",
                    "feature-a",
                )),
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
            assert_eq!(
                pm.handle_peer_data(peer_snapshot(
                    "peer-b",
                    &repo_identity,
                    &repo_path,
                    "/srv/peer-b/remote-only",
                    "feature-b",
                )),
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
        }

        let synthetic = crate::peer::synthetic_repo_path(&HostName::new("peer-a"), &repo_path);
        let merged = {
            let pm = peer_manager.lock().await;
            let peers: Vec<(HostName, ProviderData)> = pm
                .get_peer_data()
                .iter()
                .filter_map(|(host, repos)| {
                    repos
                        .get(&repo_identity)
                        .map(|state| (host.clone(), state.provider_data.clone()))
                })
                .collect();
            crate::peer::merge_provider_data(
                &ProviderData::default(),
                daemon.host_name(),
                &peers
                    .iter()
                    .map(|(h, d)| (h.clone(), d))
                    .collect::<Vec<_>>(),
            )
        };
        daemon
            .add_virtual_repo(synthetic.clone(), merged)
            .await
            .expect("add virtual repo");
        daemon
            .set_peer_providers(
                &synthetic,
                vec![
                    (
                        HostName::new("peer-a"),
                        ProviderData {
                            checkouts: IndexMap::from([(
                                HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only"),
                                checkout("feature-a"),
                            )]),
                            ..Default::default()
                        },
                    ),
                    (
                        HostName::new("peer-b"),
                        ProviderData {
                            checkouts: IndexMap::from([(
                                HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only"),
                                checkout("feature-b"),
                            )]),
                            ..Default::default()
                        },
                    ),
                ],
            )
            .await;
        {
            let mut pm = peer_manager.lock().await;
            pm.register_remote_repo(repo_identity.clone(), synthetic.clone());
        }

        let mut rx = daemon.subscribe();
        clear_peer_data(&peer_manager, &daemon, &HostName::new("peer-a")).await;

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
                assert_eq!(
                    snapshot.providers.checkouts[&remaining_key].branch,
                    "feature-b"
                );
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
