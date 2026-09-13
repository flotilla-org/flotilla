use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_core::{in_process::InProcessDaemon, path_context::ExecutionEnvironmentPath, step::RemoteStepBatchRequest};
use flotilla_protocol::{ConfigLabel, NodeId, NodeInfo, PeerConnectionState, PeerWireMessage, RepoIdentity};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::{
    remote_commands::RemoteCommandRouter, replicator::PeerReplicatorSupervisors, shared::sync_peer_query_state, PeerConnectedNotice,
    PeerConnectionEvent, SshTransport,
};
use crate::peer::{dispatch_pending_sends, peer_resource_socket_path, HandleResult, InboundPeerEnvelope, PeerManager, PeerSender};

pub(super) enum ForwardResult {
    Disconnected,
    Shutdown,
    KeepaliveTimeout,
}

enum PostHandleAction {
    ReconnectSuppressed {
        peer: NodeId,
    },
    CommandRequested {
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command: flotilla_protocol::Command,
        principal_ref: Option<flotilla_protocol::PrincipalRef>,
        session_id: Option<uuid::Uuid>,
    },
    CommandCancelRequested {
        cancel_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command_request_id: u64,
    },
    CommandEventReceived {
        request_id: u64,
        responder_node_id: NodeId,
        event: flotilla_protocol::CommandPeerEvent,
    },
    CommandResponseReceived {
        request_id: u64,
        responder_node_id: NodeId,
        result: flotilla_protocol::CommandValue,
    },
    CommandCancelResponseReceived {
        cancel_id: u64,
        error: Option<String>,
    },
    RemoteStepRequested {
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        repo_identity: RepoIdentity,
        step_offset: usize,
        steps: Vec<flotilla_protocol::Step>,
    },
    RemoteStepEventReceived {
        request_id: u64,
        responder_node_id: NodeId,
        batch_step_index: usize,
        batch_step_count: usize,
        description: String,
        status: flotilla_protocol::StepStatus,
    },
    RemoteStepResponseReceived {
        request_id: u64,
        responder_node_id: NodeId,
        outcomes: Vec<flotilla_protocol::StepOutcome>,
    },
    RemoteStepCancelRequested {
        cancel_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        remote_step_request_id: u64,
    },
    RemoteStepCancelResponseReceived {
        cancel_id: u64,
        error: Option<String>,
    },
    Ignored,
}

const PING_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(90);
const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);

async fn retry_with_backoff<T, BeforeAttempt, BeforeAttemptFuture, Dial, DialFuture, OnFailure, OnFailureFuture>(
    mut before_attempt: BeforeAttempt,
    mut dial: Dial,
    mut on_failure: OnFailure,
) -> T
where
    BeforeAttempt: FnMut(u32, Duration) -> BeforeAttemptFuture,
    BeforeAttemptFuture: Future<Output = ()>,
    Dial: FnMut() -> DialFuture,
    DialFuture: Future<Output = Result<T, String>>,
    OnFailure: FnMut(u32, String) -> OnFailureFuture,
    OnFailureFuture: Future<Output = ()>,
{
    let mut attempt = 1_u32;
    loop {
        let delay = SshTransport::backoff_delay(attempt);
        before_attempt(attempt, delay).await;
        tokio::time::sleep(delay).await;

        let error = match dial().await {
            Ok(connection) => return connection,
            Err(error) => error,
        };
        on_failure(attempt, error).await;
        attempt = attempt.saturating_add(1);
    }
}

#[cfg(test)]
pub(super) async fn retry_with_backoff_for_test<T, BeforeAttempt, BeforeAttemptFuture, Dial, DialFuture, OnFailure, OnFailureFuture>(
    before_attempt: BeforeAttempt,
    dial: Dial,
    on_failure: OnFailure,
) -> T
where
    BeforeAttempt: FnMut(u32, Duration) -> BeforeAttemptFuture,
    BeforeAttemptFuture: Future<Output = ()>,
    Dial: FnMut() -> DialFuture,
    DialFuture: Future<Output = Result<T, String>>,
    OnFailure: FnMut(u32, String) -> OnFailureFuture,
    OnFailureFuture: Future<Output = ()>,
{
    retry_with_backoff(before_attempt, dial, on_failure).await
}

fn resource_socket_path_for(resource_socket_dir: Option<&Path>, target_label: &ConfigLabel) -> Option<PathBuf> {
    resource_socket_dir.and_then(|directory| {
        peer_resource_socket_path(directory, target_label)
            .inspect_err(|error| warn!(target = %target_label.0, %error, "invalid peer resource socket path"))
            .ok()
    })
}

#[derive(bon::Builder)]
#[builder(builder_type(vis = "pub(super)"))]
pub(super) struct PeerRuntime {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    inbound_peer_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    remote_command_router: RemoteCommandRouter,
    resource_socket_dir: Option<PathBuf>,
}

impl PeerRuntime {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        peer_manager: Arc<Mutex<PeerManager>>,
        inbound_peer_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
        inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
        remote_command_router: RemoteCommandRouter,
        resource_socket_dir: Option<PathBuf>,
    ) -> Self {
        Self { daemon, peer_manager, inbound_peer_rx, inbound_peer_tx, remote_command_router, resource_socket_dir }
    }

    pub(super) fn spawn(self) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) {
        let outbound_peer_manager = Arc::clone(&self.peer_manager);
        let peer_manager_task = Arc::clone(&self.peer_manager);
        let inbound_peer_tx_for_ssh = self.inbound_peer_tx.clone();
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectionEvent>();
        let peer_connected_tx_for_ssh = peer_connected_tx.clone();
        let peer_daemon = Arc::clone(&self.daemon);
        let remote_command_router_task = self.remote_command_router.clone();
        let resource_socket_dir = self.resource_socket_dir;
        let inbound_peer_rx = self.inbound_peer_rx;

        let inbound_handle = tokio::spawn(async move {
            if let Some(mut rx) = inbound_peer_rx {
                let mut initial_connections = HashMap::new();
                let configured_targets = {
                    let mut pm = peer_manager_task.lock().await;
                    let targets = pm.configured_targets();
                    for connection in pm.connect_all().await {
                        initial_connections.insert(connection.label.clone(), connection);
                    }
                    targets
                };

                for connection in initial_connections.values() {
                    peer_daemon.publish_peer_connection_status(&connection.node, PeerConnectionState::Connected).await;
                }
                sync_peer_query_state(&peer_manager_task, &peer_daemon).await;

                for target in configured_targets {
                    let tx = inbound_peer_tx_for_ssh.clone();
                    let pm = Arc::clone(&peer_manager_task);
                    let daemon_for_cleanup = Arc::clone(&peer_daemon);
                    let remote_command_router_for_cleanup = remote_command_router_task.clone();
                    let initial_connection = initial_connections.remove(&target.label);
                    let peer_connected_tx_clone = peer_connected_tx_for_ssh.clone();
                    let target_label = target.label.clone();
                    let resource_socket_dir = resource_socket_dir.clone();

                    tokio::spawn(async move {
                        let mut current_peer: Option<NodeInfo> = None;
                        let mut last_known_session_id: Option<uuid::Uuid> = None;

                        if let Some(initial_connection) = initial_connection {
                            let peer_name = initial_connection.node.node_id.clone();
                            current_peer = Some(initial_connection.node.clone());
                            let mut inbound_rx = initial_connection.inbound_rx;
                            let generation = initial_connection.generation;
                            info!(peer = %peer_name, generation, "connected successfully");
                            let _ = peer_connected_tx_clone.send(PeerConnectionEvent::Connected(PeerConnectedNotice {
                                peer: peer_name.clone(),
                                generation,
                                resource_socket_path: resource_socket_path_for(resource_socket_dir.as_deref(), &target_label),
                            }));
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
                                ForwardResult::Shutdown => {
                                    let _ = peer_connected_tx_clone
                                        .send(PeerConnectionEvent::Disconnected { peer: peer_name.clone(), generation });
                                    return;
                                }
                                ForwardResult::Disconnected => {
                                    info!(target = %target_label.0, peer = %peer_name, "SSH connection dropped, will reconnect");
                                }
                                ForwardResult::KeepaliveTimeout => {
                                    info!(target = %target_label.0, peer = %peer_name, "keepalive timeout, forcing reconnect");
                                }
                            }
                            let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                            remote_command_router_for_cleanup.fail_pending_remote_commands_for_host(&peer_name).await;
                            remote_command_router_for_cleanup.fail_pending_remote_steps_for_host(&peer_name).await;
                            if plan.was_active {
                                daemon_for_cleanup
                                    .publish_peer_connection_status(&initial_connection.node, PeerConnectionState::Disconnected)
                                    .await;
                            }
                        }

                        loop {
                            let suppressed = if let Some(peer) = current_peer.as_ref() {
                                let mut pm = pm.lock().await;
                                pm.reconnect_suppressed_until(&peer.node_id)
                                    .map(|deadline| (peer.node_id.clone(), deadline.saturating_duration_since(Instant::now())))
                            } else {
                                None
                            };
                            if let Some((peer_name, delay)) = suppressed {
                                info!(target = %target_label.0, peer = %peer_name, delay_secs = delay.as_secs(), "reconnect suppressed after peer retirement");
                                tokio::time::sleep(delay).await;
                            }

                            let peer_for_status = current_peer.clone();
                            let pm_for_before = Arc::clone(&pm);
                            let daemon_for_before = Arc::clone(&daemon_for_cleanup);
                            let target_for_before = target_label.clone();
                            let pm_for_dial = Arc::clone(&pm);
                            let target_for_dial = target_label.clone();
                            let pm_for_failure = Arc::clone(&pm);
                            let daemon_for_failure = Arc::clone(&daemon_for_cleanup);
                            let target_for_failure = target_label.clone();
                            let connection = retry_with_backoff(
                                move |attempt, delay| {
                                    let pm = Arc::clone(&pm_for_before);
                                    let daemon = Arc::clone(&daemon_for_before);
                                    let target = target_for_before.clone();
                                    let peer = peer_for_status.clone();
                                    async move {
                                        {
                                            let mut pm = pm.lock().await;
                                            pm.note_reconnect_backoff(&target, attempt, delay);
                                        }
                                        if let Some(peer) = peer.as_ref() {
                                            daemon.publish_peer_connection_status(peer, PeerConnectionState::Reconnecting).await;
                                        }
                                        info!(target = %target.0, %attempt, delay_secs = delay.as_secs(), "reconnecting after backoff");
                                    }
                                },
                                move || {
                                    let pm = Arc::clone(&pm_for_dial);
                                    let target = target_for_dial.clone();
                                    async move {
                                        let mut pm = pm.lock().await;
                                        pm.reconnect_target(&target, RECONNECT_ATTEMPT_TIMEOUT).await
                                    }
                                },
                                move |attempt, error| {
                                    let pm = Arc::clone(&pm_for_failure);
                                    let daemon = Arc::clone(&daemon_for_failure);
                                    let target = target_for_failure.clone();
                                    async move {
                                        warn!(target = %target.0, err = %error, %attempt, "reconnection failed");
                                        sync_peer_query_state(&pm, &daemon).await;
                                    }
                                },
                            )
                            .await;

                            let peer_name = connection.node.node_id.clone();
                            current_peer = Some(connection.node.clone());
                            let generation = connection.generation;
                            let mut inbound_rx = connection.inbound_rx;
                            info!(peer = %peer_name, generation, "reconnected successfully");
                            last_known_session_id =
                                handle_remote_restart_if_needed(&pm, &daemon_for_cleanup, &peer_name, last_known_session_id).await;
                            sync_peer_query_state(&pm, &daemon_for_cleanup).await;
                            daemon_for_cleanup
                                .publish_peer_connection_status(
                                    current_peer.as_ref().expect("current peer"),
                                    PeerConnectionState::Connected,
                                )
                                .await;
                            let _ = peer_connected_tx_clone.send(PeerConnectionEvent::Connected(PeerConnectedNotice {
                                peer: peer_name.clone(),
                                generation,
                                resource_socket_path: resource_socket_path_for(resource_socket_dir.as_deref(), &target_label),
                            }));
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
                                ForwardResult::Shutdown => {
                                    let _ = peer_connected_tx_clone
                                        .send(PeerConnectionEvent::Disconnected { peer: peer_name.clone(), generation });
                                    return;
                                }
                                ForwardResult::Disconnected => {
                                    info!(target = %target_label.0, peer = %peer_name, "SSH connection dropped, will reconnect");
                                }
                                ForwardResult::KeepaliveTimeout => {
                                    info!(target = %target_label.0, peer = %peer_name, "keepalive timeout, forcing reconnect");
                                }
                            }
                            let plan = disconnect_peer_and_rebuild(&pm, &daemon_for_cleanup, &peer_name, generation).await;
                            remote_command_router_for_cleanup.fail_pending_remote_commands_for_host(&peer_name).await;
                            remote_command_router_for_cleanup.fail_pending_remote_steps_for_host(&peer_name).await;
                            if plan.was_active {
                                daemon_for_cleanup
                                    .publish_peer_connection_status(
                                        current_peer.as_ref().expect("current peer"),
                                        PeerConnectionState::Disconnected,
                                    )
                                    .await;
                            }
                        }
                    });
                }

                loop {
                    tokio::select! {
                        maybe_env = rx.recv() => {
                            let Some(env) = maybe_env else { break };
                            if let PeerWireMessage::HostSummary(summary) = &env.msg {
                                peer_daemon.publish_peer_summary(summary.clone()).await;
                            }

                            let (post_handle_action, pending_sends) = {
                                let mut pm = peer_manager_task.lock().await;
                                let post_handle_action = match pm.handle_inbound(env).await {
                                    HandleResult::ReconnectSuppressed { peer } => PostHandleAction::ReconnectSuppressed { peer },
                                    HandleResult::CommandRequested { request_id, requester_node_id, reply_via, command, principal_ref, session_id } => {
                                        PostHandleAction::CommandRequested { request_id, requester_node_id, reply_via, command, principal_ref, session_id }
                                    }
                                    HandleResult::CommandCancelRequested { cancel_id, requester_node_id, reply_via, command_request_id } => {
                                        PostHandleAction::CommandCancelRequested { cancel_id, requester_node_id, reply_via, command_request_id }
                                    }
                                    HandleResult::CommandEventReceived { request_id, responder_node_id, event } => {
                                        PostHandleAction::CommandEventReceived { request_id, responder_node_id, event }
                                    }
                                    HandleResult::CommandResponseReceived { request_id, responder_node_id, result } => {
                                        PostHandleAction::CommandResponseReceived { request_id, responder_node_id, result }
                                    }
                                    HandleResult::CommandCancelResponseReceived { cancel_id, responder_node_id: _, error } => {
                                        PostHandleAction::CommandCancelResponseReceived { cancel_id, error }
                                    }
                                    HandleResult::RemoteStepRequested {
                                        request_id,
                                        requester_node_id,
                                        reply_via,
                                        repo_identity,
                                        step_offset,
                                        steps,
                                    } => PostHandleAction::RemoteStepRequested {
                                        request_id,
                                        requester_node_id,
                                        reply_via,
                                        repo_identity,
                                        step_offset,
                                        steps,
                                    },
                                    HandleResult::RemoteStepEventReceived {
                                        request_id,
                                        responder_node_id,
                                        batch_step_index,
                                        batch_step_count,
                                        description,
                                        status,
                                    } => PostHandleAction::RemoteStepEventReceived {
                                        request_id,
                                        responder_node_id,
                                        batch_step_index,
                                        batch_step_count,
                                        description,
                                        status,
                                    },
                                    HandleResult::RemoteStepResponseReceived { request_id, responder_node_id, outcomes } => {
                                        PostHandleAction::RemoteStepResponseReceived { request_id, responder_node_id, outcomes }
                                    }
                                    HandleResult::RemoteStepCancelRequested {
                                        cancel_id,
                                        requester_node_id,
                                        reply_via,
                                        remote_step_request_id,
                                    } => PostHandleAction::RemoteStepCancelRequested {
                                        cancel_id,
                                        requester_node_id,
                                        reply_via,
                                        remote_step_request_id,
                                    },
                                    HandleResult::RemoteStepCancelResponseReceived { cancel_id, responder_node_id: _, error } => {
                                        PostHandleAction::RemoteStepCancelResponseReceived { cancel_id, error }
                                    }
                                    HandleResult::Ignored => PostHandleAction::Ignored,
                                };
                                let pending_sends = pm.take_pending_sends();
                                (post_handle_action, pending_sends)
                            };
                            dispatch_pending_sends(pending_sends).await;

                            match post_handle_action {
                                PostHandleAction::ReconnectSuppressed { peer } => {
                                    info!(peer = %peer, "peer requested reconnect suppression");
                                }
                                PostHandleAction::CommandRequested { request_id, requester_node_id, reply_via, command, principal_ref, session_id } => {
                                    remote_command_router_task
                                        .spawn_forwarded_command(request_id, requester_node_id, reply_via, command, principal_ref, session_id)
                                        .await;
                                }
                                PostHandleAction::CommandCancelRequested { cancel_id, requester_node_id, reply_via, command_request_id } => {
                                    remote_command_router_task
                                        .spawn_forwarded_cancel(cancel_id, requester_node_id, reply_via, command_request_id);
                                }
                                PostHandleAction::CommandEventReceived { request_id, responder_node_id, event } => {
                                    remote_command_router_task.emit_remote_command_event(request_id, responder_node_id, event).await;
                                }
                                PostHandleAction::CommandResponseReceived { request_id, responder_node_id, result } => {
                                    remote_command_router_task.complete_remote_command(request_id, responder_node_id, result).await;
                                }
                                PostHandleAction::CommandCancelResponseReceived { cancel_id, error } => {
                                    remote_command_router_task.complete_remote_cancel(cancel_id, error).await;
                                }
                                PostHandleAction::RemoteStepRequested {
                                    request_id,
                                    requester_node_id,
                                    reply_via,
                                    repo_identity,
                                    step_offset,
                                    steps,
                                } => {
                                    let local_event_repo = peer_daemon
                                        .preferred_local_path_for_identity(&repo_identity)
                                        .await
                                        .map(ExecutionEnvironmentPath::new);
                                    if local_event_repo.is_none() {
                                        warn!(%repo_identity, "forwarded remote step request has no local event repo path on responder");
                                    }
                                    remote_command_router_task
                                        .spawn_forwarded_remote_step_batch(
                                            request_id,
                                            requester_node_id,
                                            reply_via,
                                            RemoteStepBatchRequest {
                                                command_id: request_id,
                                                target_node_id: peer_daemon.node_id().clone(),
                                                repo_identity,
                                                repo: local_event_repo,
                                                step_offset,
                                                steps,
                                            },
                                        )
                                        .await;
                                }
                                PostHandleAction::RemoteStepEventReceived {
                                    request_id,
                                    responder_node_id,
                                    batch_step_index,
                                    batch_step_count,
                                    description,
                                    status,
                                } => {
                                    remote_command_router_task
                                        .emit_remote_step_event(
                                            request_id,
                                            responder_node_id,
                                            batch_step_index,
                                            batch_step_count,
                                            description,
                                            status,
                                        )
                                        .await;
                                }
                                PostHandleAction::RemoteStepResponseReceived { request_id, responder_node_id, outcomes } => {
                                    remote_command_router_task.complete_remote_step(request_id, responder_node_id, outcomes).await;
                                }
                                PostHandleAction::RemoteStepCancelRequested {
                                    cancel_id,
                                    requester_node_id,
                                    reply_via,
                                    remote_step_request_id,
                                } => {
                                    remote_command_router_task.spawn_forwarded_remote_step_cancel(
                                        cancel_id,
                                        requester_node_id,
                                        reply_via,
                                        remote_step_request_id,
                                    );
                                }
                                PostHandleAction::RemoteStepCancelResponseReceived { cancel_id, error } => {
                                    remote_command_router_task.complete_remote_step_cancel(cancel_id, error).await;
                                }
                                PostHandleAction::Ignored => {}
                            }
                            sync_peer_query_state(&peer_manager_task, &peer_daemon).await;
                        }
                    }
                }
            }
        });

        let outbound_daemon = Arc::clone(&self.daemon);
        let outbound_remote_command_router = self.remote_command_router.clone();
        let mut peer_connected_rx = peer_connected_rx;
        tokio::spawn(async move {
            let mut peer_replicators = PeerReplicatorSupervisors::default();

            while let Some(event) = peer_connected_rx.recv().await {
                match event {
                    PeerConnectionEvent::Connected(notice) => {
                        debug!(peer = %notice.peer, generation = notice.generation, "starting peer resource replication");
                        peer_replicators
                            .peer_connected(
                                outbound_remote_command_router.clone(),
                                Arc::clone(&outbound_daemon),
                                notice.peer.clone(),
                                notice.generation,
                                notice.resource_socket_path,
                            )
                            .await;
                        send_link_state(&outbound_daemon, &outbound_peer_manager, &notice.peer, notice.generation).await;
                    }
                    PeerConnectionEvent::Disconnected { peer, generation } => {
                        peer_replicators.peer_disconnected(&peer, generation);
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
    peer_name: &NodeId,
    last_known_session_id: Option<uuid::Uuid>,
) -> Option<uuid::Uuid> {
    let current_session_id = {
        let pm_lock = peer_manager.lock().await;
        pm_lock.peer_session_id(peer_name)
    };

    if let (Some(prev), Some(curr)) = (last_known_session_id, current_session_id) {
        if prev != curr {
            info!(peer = %peer_name, "remote daemon restarted (session_id changed), clearing stale peer state");
            {
                let mut pm_lock = peer_manager.lock().await;
                pm_lock.clear_peer_state_for_restart(peer_name);
            }
            sync_peer_query_state(peer_manager, daemon).await;
        }
    }

    current_session_id
}

pub(super) async fn disconnect_peer_and_rebuild(
    peer_manager: &Arc<Mutex<PeerManager>>,
    daemon: &Arc<InProcessDaemon>,
    peer_name: &NodeId,
    generation: u64,
) -> crate::peer::DisconnectPlan {
    let plan = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(peer_name, generation)
    };
    sync_peer_query_state(peer_manager, daemon).await;
    plan
}

pub(super) async fn send_link_state(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    peer: &NodeId,
    generation: u64,
) -> bool {
    let sender = {
        let pm = peer_manager.lock().await;
        pm.get_sender_if_current(peer, generation)
    };
    let Some(sender) = sender else {
        debug!(peer = %peer, "peer connection superseded, skipping local state send");
        return false;
    };

    if let Err(e) = sender.send(PeerWireMessage::HostSummary(daemon.local_host_summary().await)).await {
        debug!(peer = %peer, err = %e, "failed to send host summary to peer");
        return false;
    }

    let advertisements = {
        let pm = peer_manager.lock().await;
        pm.route_advertisements_for(peer)
    };
    for advertisement in advertisements {
        if let Err(e) = sender.send(advertisement).await {
            debug!(peer = %peer, err = %e, "failed to send route advertisement to peer");
            return false;
        }
    }
    true
}

async fn forward_with_keepalive(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &NodeId,
    generation: u64,
    sender: Arc<dyn PeerSender>,
) -> ForwardResult {
    forward_with_keepalive_for_test(tx, inbound_rx, peer_name, generation, sender, PING_INTERVAL, KEEPALIVE_TIMEOUT).await
}

pub(super) async fn forward_with_keepalive_for_test(
    tx: &mpsc::Sender<InboundPeerEnvelope>,
    inbound_rx: &mut mpsc::Receiver<PeerWireMessage>,
    peer_name: &NodeId,
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
