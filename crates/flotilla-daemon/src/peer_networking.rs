use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    ConfigLabel, DaemonEvent, HostName, PeerConnectionState, PeerDataMessage, PeerWireMessage, ProviderData, RepoIdentity,
    RoutedPeerMessage,
};
use futures::future::join_all;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::peer::{
    merge_provider_data, synthetic_repo_path, HandleResult, InboundPeerEnvelope, OverlayUpdate, PeerManager, PeerSender, SshTransport,
};

/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
pub struct PeerConnectedNotice {
    pub peer: HostName,
    pub generation: u64,
}

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
    #[allow(clippy::type_complexity)]
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
            match SshTransport::new(host_name.clone(), ConfigLabel(name.clone()), host_config, daemon.session_id()) {
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
            Self { daemon, peer_manager: Arc::clone(&peer_manager), peer_data_tx: peer_data_tx.clone(), peer_data_rx: Some(peer_data_rx) },
            peer_manager,
            peer_data_tx,
        ))
    }

    /// Start the peer networking background tasks.
    ///
    /// Spawns three concurrent task groups:
    /// 1. Per-peer SSH connection loops with reconnect
    /// 2. Inbound message processor (relay + handle + overlay updates)
    /// 3. Outbound snapshot broadcaster
    ///
    /// Returns a `peer_connected_tx` sender that callers (e.g. `handle_client`)
    /// use to notify the outbound broadcaster when a socket peer connects.
    ///
    /// Consumes `self` — call only once.
    pub fn spawn(mut self) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
        let peer_data_rx = self.peer_data_rx.take().expect("spawn() called twice");
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let peer_manager = self.peer_manager;
        let daemon = self.daemon;
        let peer_data_tx = self.peer_data_tx;

        let outbound_peer_manager = Arc::clone(&peer_manager);
        let peer_manager_task = Arc::clone(&peer_manager);
        let peer_data_tx_for_ssh = peer_data_tx.clone();
        let peer_connected_tx_for_ssh = peer_connected_tx.clone();
        let peer_daemon = Arc::clone(&daemon);

        // Spawn a parent task that owns all peer networking work.
        // The returned JoinHandle tracks this task for shutdown coordination.
        let handle = tokio::spawn(async move {
            // Task group 1 & 2: SSH connection loops + inbound processor
            let inbound_handle = tokio::spawn(async move {
                let mut rx = peer_data_rx;
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
                        // Track the remote daemon's session ID to detect restarts.
                        // When session_id changes on reconnect, stale peer data must
                        // be cleared before the new connection is used.
                        let mut last_known_session_id: Option<uuid::Uuid> = None;

                        // Forward from initial connection if available
                        if let Some((generation, mut inbound_rx)) = initial_rx {
                            let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });

                            // Save initial session ID
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

                                    // Detect remote daemon restart by comparing session IDs.
                                    // reconnect_peer() does NOT call disconnect_peer() — it only
                                    // does transport.disconnect() which does NOT clear stale peer
                                    // data. When session_id changes, explicitly clear stale data.
                                    let current_session_id = {
                                        let pm_lock = pm.lock().await;
                                        pm_lock.peer_session_id(&peer_name)
                                    };
                                    if let (Some(prev), Some(curr)) = (last_known_session_id, current_session_id) {
                                        if prev != curr {
                                            info!(
                                                peer = %peer_name,
                                                "remote daemon restarted (session_id changed), clearing stale data"
                                            );
                                            // Clear stale peer data WITHOUT disconnecting —
                                            // the fresh connection is already active.
                                            let affected_repos = {
                                                let mut pm_lock = pm.lock().await;
                                                pm_lock.clear_peer_data_for_restart(&peer_name)
                                            };
                                            if !affected_repos.is_empty() {
                                                rebuild_peer_overlays(&pm, &daemon_for_cleanup, affected_repos).await;
                                            }
                                        }
                                    }
                                    last_known_session_id = current_session_id;

                                    daemon_for_cleanup.send_event(DaemonEvent::PeerStatusChanged {
                                        host: peer_name.clone(),
                                        status: PeerConnectionState::Connected,
                                    });
                                    let _ = peer_connected_tx_clone.send(PeerConnectedNotice { peer: peer_name.clone(), generation });
                                    attempt = 1; // Reset backoff on any successful reconnect
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
                        PeerWireMessage::Goodbye { .. }
                        | PeerWireMessage::Ping { .. }
                        | PeerWireMessage::Pong { .. } => {
                            (env.connection_peer.clone(), PathBuf::new())
                        }
                    };

                    // Snapshot relay targets under lock (synchronous — no .await)
                    let relay_targets = if let PeerWireMessage::Data(msg) = &env.msg {
                        let pm = peer_manager_task.lock().await;
                        pm.prepare_relay(&origin, msg)
                    } else {
                        vec![]
                    };

                    // Send concurrently outside lock with per-peer timeout (5s)
                    if !relay_targets.is_empty() {
                        let sends = relay_targets.into_iter().map(|(name, sender, msg)| async move {
                            match tokio::time::timeout(Duration::from_secs(5), sender.send(PeerWireMessage::Data(msg))).await {
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

                    // Re-acquire lock for handle_inbound
                    let mut pm = peer_manager_task.lock().await;
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
                                    synthetic_repo_path(&origin, &repo_path);
                                debug!(
                                    repo = %updated_repo_id,
                                    path = %synthetic.display(),
                                    "creating/updating virtual repo for remote-only peer"
                                );

                                // Build merged provider data for virtual repo
                                let merged = merge_provider_data(
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
                            warn!(
                                %request_id,
                                requester = %requester_host,
                                reply_via = %reply_via,
                                command = %command.description(),
                                "ignoring routed peer command in peer_networking task"
                            );
                        }
                        HandleResult::CommandEventReceived { request_id, responder_host, .. } => {
                            warn!(%request_id, responder = %responder_host, "ignoring routed peer command event in peer_networking task");
                        }
                        HandleResult::CommandResponseReceived { request_id, responder_host, .. } => {
                            warn!(%request_id, responder = %responder_host, "ignoring routed peer command response in peer_networking task");
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
            });

            // Task group 3: Outbound snapshot broadcaster
            let outbound_daemon = Arc::clone(&daemon);
            let mut peer_connected_rx = peer_connected_rx;
            let outbound_handle = tokio::spawn(async move {
                let mut event_rx = outbound_daemon.subscribe();
                let mut outbound_clock = flotilla_protocol::VectorClock::default();
                let host_name = outbound_daemon.host_name().clone();
                let mut last_sent_versions: HashMap<PathBuf, u64> = HashMap::new();

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

            // Wait for both task groups — if either exits, the other will
            // eventually follow (channels close).
            let _ = inbound_handle.await;
            let _ = outbound_handle.await;
        }); // end parent task

        // The peer_connected_tx lets DaemonServer notify the outbound broadcaster
        // when socket peers connect, so they receive current local state.
        (handle, peer_connected_tx)
    }
}

/// Rebuild daemon overlays for repo identities affected by peer disconnect or failover.
pub(crate) async fn rebuild_peer_overlays(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    affected_repos: Vec<RepoIdentity>,
) {
    for repo_id in affected_repos {
        if let Some(local_path) = daemon.find_repo_by_identity(&repo_id).await {
            // Local repo — rebuild its peer overlay from remaining peers
            let peers: Vec<(HostName, ProviderData)> = {
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
                let peers: Vec<(HostName, ProviderData)> = pm
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

pub(crate) async fn dispatch_resync_requests(peer_manager: &Arc<Mutex<PeerManager>>, requests: Vec<RoutedPeerMessage>) {
    for request in requests {
        let target = match &request {
            RoutedPeerMessage::RequestResync { target_host, .. } => target_host.clone(),
            RoutedPeerMessage::ResyncSnapshot { requester_host, .. } => requester_host.clone(),
            RoutedPeerMessage::CommandRequest { target_host, .. } => target_host.clone(),
            RoutedPeerMessage::CommandEvent { requester_host, .. } => requester_host.clone(),
            RoutedPeerMessage::CommandResponse { requester_host, .. } => requester_host.clone(),
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

pub(crate) async fn disconnect_peer_and_rebuild(
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
            OverlayUpdate::SetProviders { identity, peers } => {
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
            OverlayUpdate::RemoveRepo { identity, path } => {
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
/// Returns `true` if at least one peer was successfully sent to.
pub(crate) async fn send_local_to_peers(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    repo_path: &std::path::Path,
    local_providers: ProviderData,
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
pub(crate) async fn send_local_to_peer(
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

/// Result of the keepalive-aware forwarding loop.
enum ForwardResult {
    /// Peer connection dropped (EOF on inbound receiver).
    Disconnected,
    /// Main forwarding channel closed (daemon shutting down).
    Shutdown,
    /// No messages received within the keepalive timeout.
    KeepaliveTimeout,
}

const PING_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(90);

/// Forward messages from an inbound receiver to the shared peer_data channel,
/// with periodic keepalive pings and liveness timeout detection.
///
/// Sends `Ping` messages every 30 seconds. If no message (including Pongs)
/// is received within 90 seconds, returns `KeepaliveTimeout` to trigger
/// reconnection. Pong messages update `last_message_at` but are not
/// forwarded to the inbound processor.
async fn forward_with_keepalive(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
    sender: Arc<dyn PeerSender>,
) -> ForwardResult {
    let mut ping_interval = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_message_at = Instant::now();

    loop {
        tokio::select! {
            msg = inbound_rx.recv() => {
                match msg {
                    Some(peer_msg) => {
                        last_message_at = Instant::now();
                        // Skip forwarding Pong messages to the inbound processor
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
                if last_message_at.elapsed() > KEEPALIVE_TIMEOUT {
                    warn!(
                        peer = %peer_name,
                        elapsed_secs = last_message_at.elapsed().as_secs(),
                        "keepalive timeout — no messages received in 90s"
                    );
                    return ForwardResult::KeepaliveTimeout;
                }
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if let Err(e) = sender.send(PeerWireMessage::Ping { timestamp }).await {
                    debug!(peer = %peer_name, err = %e, "failed to send keepalive ping");
                }
            }
        }
    }
}
