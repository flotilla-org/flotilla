use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{DaemonEvent, HostName, PeerConnectionState, PeerDataMessage, PeerWireMessage, RepoIdentity, RoutedPeerMessage};
use futures::future::join_all;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::{remote_commands::RemoteCommandRouter, shared::sync_peer_query_state, PeerConnectedNotice, SshTransport};
use crate::peer::{HandleResult, InboundPeerEnvelope, PeerManager, PeerSender};

pub(super) enum ForwardResult {
    Disconnected,
    Shutdown,
    KeepaliveTimeout,
}

const PING_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(90);

pub(super) struct PeerRuntime {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    remote_command_router: RemoteCommandRouter,
}

impl PeerRuntime {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        peer_manager: Arc<Mutex<PeerManager>>,
        peer_data_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
        peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
        remote_command_router: RemoteCommandRouter,
    ) -> Self {
        Self { daemon, peer_manager, peer_data_rx, peer_data_tx, remote_command_router }
    }

    pub(super) fn spawn(self) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) {
        let outbound_peer_manager = Arc::clone(&self.peer_manager);
        let peer_manager_task = Arc::clone(&self.peer_manager);
        let peer_data_tx_for_ssh = self.peer_data_tx.clone();
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();
        let peer_connected_tx_for_ssh = peer_connected_tx.clone();
        let peer_daemon = Arc::clone(&self.daemon);
        let remote_command_router_task = self.remote_command_router.clone();
        let peer_data_rx = self.peer_data_rx;

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
                    peer_daemon.publish_peer_connection_status(name, PeerConnectionState::Connecting).await;
                }
                for name in &peer_names {
                    let status =
                        if initial_rx_map.contains_key(name) { PeerConnectionState::Connected } else { PeerConnectionState::Disconnected };
                    peer_daemon.publish_peer_connection_status(name, status).await;
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
                                daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Disconnected).await;
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
                            daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Reconnecting).await;
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
                                    daemon_for_cleanup.publish_peer_connection_status(&peer_name, PeerConnectionState::Connected).await;
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
                                        daemon_for_cleanup
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
                                peer_daemon.publish_peer_summary(&origin, summary.clone()).await;
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
                                            if let Err(e) = peer_daemon
                                                .add_virtual_repo(updated_repo_id.clone(), synthetic.clone(), peers, overlay_version)
                                                .await
                                            {
                                                warn!(repo = %updated_repo_id, err = %e, "failed to add virtual repo");
                                            } else {
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
                                        remote_command_router_task
                                            .spawn_forwarded_command(request_id, requester_host, reply_via, command)
                                            .await;
                                    }
                                    HandleResult::CommandCancelRequested { cancel_id, requester_host, reply_via, command_request_id } => {
                                        drop(pm);
                                        remote_command_router_task
                                            .spawn_forwarded_cancel(cancel_id, requester_host, reply_via, command_request_id);
                                    }
                                    HandleResult::CommandEventReceived { request_id, responder_host, event } => {
                                        drop(pm);
                                        remote_command_router_task.emit_remote_command_event(request_id, responder_host, event).await;
                                    }
                                    HandleResult::CommandResponseReceived { request_id, responder_host, result } => {
                                        drop(pm);
                                        remote_command_router_task.complete_remote_command(request_id, responder_host, result).await;
                                    }
                                    HandleResult::CommandCancelResponseReceived { cancel_id, responder_host: _, error } => {
                                        drop(pm);
                                        remote_command_router_task.complete_remote_cancel(cancel_id, error).await;
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

        let outbound_daemon = Arc::clone(&self.daemon);
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
}

pub(super) async fn handle_remote_restart_if_needed(
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

pub(super) async fn relay_peer_data(peer_manager: &Arc<Mutex<PeerManager>>, origin: &HostName, msg: &PeerDataMessage) {
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

pub(super) async fn rebuild_peer_overlays(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    affected_repos: Vec<flotilla_protocol::RepoIdentity>,
) {
    for repo_id in affected_repos {
        if let Some(local_path) = daemon.preferred_local_path_for_identity(&repo_id).await {
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
            let mut pm = peer_manager.lock().await;
            if pm.has_peer_data_for(&repo_id) {
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

pub(super) async fn dispatch_resync_requests(peer_manager: &Arc<Mutex<PeerManager>>, requests: Vec<RoutedPeerMessage>) {
    for request in requests {
        let target = match &request {
            RoutedPeerMessage::RequestResync { target_host, .. } => target_host.clone(),
            RoutedPeerMessage::ResyncSnapshot { requester_host, .. } => requester_host.clone(),
            RoutedPeerMessage::CommandRequest { target_host, .. } => target_host.clone(),
            RoutedPeerMessage::CommandCancelRequest { target_host, .. } => target_host.clone(),
            RoutedPeerMessage::CommandEvent { requester_host, .. } => requester_host.clone(),
            RoutedPeerMessage::CommandResponse { requester_host, .. } => requester_host.clone(),
            RoutedPeerMessage::CommandCancelResponse { requester_host, .. } => requester_host.clone(),
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

pub(super) async fn disconnect_peer_and_rebuild(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &HostName,
    generation: u64,
) -> crate::peer::DisconnectPlan {
    let mut plan = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(peer_name, generation)
    };

    for update in &plan.overlay_updates {
        match update {
            crate::peer::OverlayUpdate::SetProviders { identity, peers, overlay_version } => {
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

pub(super) async fn send_local_to_peers(
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

pub(super) fn should_send_local_version(
    last_sent_versions: &std::collections::HashMap<RepoIdentity, u64>,
    repo_identity: &RepoIdentity,
    local_data_version: u64,
) -> bool {
    local_data_version > last_sent_versions.get(repo_identity).copied().unwrap_or(0)
}

pub(super) async fn send_local_to_peer(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    host_name: &HostName,
    clock: &mut flotilla_protocol::VectorClock,
    peer: &HostName,
    generation: u64,
) -> bool {
    let repo_paths = daemon.tracked_repo_paths().await;

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

async fn forward_with_keepalive(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &HostName,
    generation: u64,
    sender: Arc<dyn PeerSender>,
) -> ForwardResult {
    forward_with_keepalive_for_test(tx, inbound_rx, peer_name, generation, sender, PING_INTERVAL, KEEPALIVE_TIMEOUT).await
}

pub(super) async fn forward_with_keepalive_for_test(
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
