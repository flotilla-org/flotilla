use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    Command, CommandAction, CommandPeerEvent, CommandValue, DaemonEvent, HostName, PeerWireMessage, RepoIdentity, RepoSelector,
    RoutedPeerMessage,
};
use tokio::sync::{oneshot, Mutex, Notify};

use crate::peer::{PeerManager, PeerSender};

#[derive(Debug, Clone)]
pub(super) struct PendingRemoteCommand {
    pub(super) command_id: u64,
    pub(super) target_host: HostName,
    pub(super) repo_identity: Option<RepoIdentity>,
    pub(super) repo: Option<PathBuf>,
    pub(super) finished_via_event: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ForwardedCommand {
    pub(super) state: ForwardedCommandState,
}

#[derive(Debug, Clone)]
pub(super) enum ForwardedCommandState {
    Launching { ready: Arc<Notify> },
    Running { command_id: u64 },
}

pub(super) type PendingRemoteCommandMap = Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>;
pub(super) type ForwardedCommandMap = Arc<Mutex<HashMap<u64, ForwardedCommand>>>;
pub(super) type PendingRemoteCancelMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>;

#[derive(Clone)]
pub(super) struct RemoteCommandRouter {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    pending_remote_commands: PendingRemoteCommandMap,
    forwarded_commands: ForwardedCommandMap,
    pending_remote_cancels: PendingRemoteCancelMap,
    next_remote_command_id: Arc<AtomicU64>,
}

impl RemoteCommandRouter {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        peer_manager: Arc<Mutex<PeerManager>>,
        pending_remote_commands: PendingRemoteCommandMap,
        forwarded_commands: ForwardedCommandMap,
        pending_remote_cancels: PendingRemoteCancelMap,
        next_remote_command_id: Arc<AtomicU64>,
    ) -> Self {
        Self { daemon, peer_manager, pending_remote_commands, forwarded_commands, pending_remote_cancels, next_remote_command_id }
    }

    pub(super) async fn dispatch_execute(&self, command: Command) -> Result<u64, String> {
        let target_host = command.host.clone().unwrap_or_else(|| self.daemon.host_name().clone());
        if target_host != *self.daemon.host_name() {
            let request_id = {
                let mut pm = self.peer_manager.lock().await;
                pm.next_request_id()
            };
            let command_id = self.next_remote_command_id.fetch_add(1, Ordering::Relaxed);
            self.pending_remote_commands.lock().await.insert(request_id, PendingRemoteCommand {
                command_id,
                target_host: target_host.clone(),
                repo_identity: extract_command_repo_identity(&command),
                repo: None,
                finished_via_event: false,
            });

            let routed = RoutedPeerMessage::CommandRequest {
                request_id,
                requester_host: self.daemon.host_name().clone(),
                target_host: target_host.clone(),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                command: Box::new(command),
            };
            let send_result = self.send_routed_to(&target_host, routed).await;

            match send_result {
                Ok(()) => Ok(command_id),
                Err(err) => {
                    self.pending_remote_commands.lock().await.remove(&request_id);
                    Err(err)
                }
            }
        } else {
            self.daemon.execute(command).await
        }
    }

    pub(super) async fn dispatch_cancel(&self, command_id: u64) -> Result<(), String> {
        let remote = {
            let pending = self.pending_remote_commands.lock().await;
            pending
                .iter()
                .find(|(_, entry)| entry.command_id == command_id)
                .map(|(request_id, entry)| (*request_id, entry.target_host.clone()))
        };
        if let Some((command_request_id, target_host)) = remote {
            let cancel_id = {
                let mut pm = self.peer_manager.lock().await;
                pm.next_request_id()
            };
            let (tx, rx) = oneshot::channel();
            self.pending_remote_cancels.lock().await.insert(cancel_id, tx);
            let routed = RoutedPeerMessage::CommandCancelRequest {
                cancel_id,
                requester_host: self.daemon.host_name().clone(),
                target_host: target_host.clone(),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                command_request_id,
            };
            let send_result = self.send_routed_to(&target_host, routed).await;
            if let Err(err) = send_result {
                self.pending_remote_cancels.lock().await.remove(&cancel_id);
                return Err(err);
            }
            match tokio::time::timeout(Duration::from_secs(5), rx).await {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(message))) => Err(message),
                Ok(Err(_)) => Err("remote cancel response channel closed".to_string()),
                Err(_) => {
                    self.pending_remote_cancels.lock().await.remove(&cancel_id);
                    Err("timed out waiting for remote cancel response".to_string())
                }
            }
        } else {
            self.daemon.cancel(command_id).await
        }
    }

    pub(super) async fn spawn_forwarded_command(&self, request_id: u64, requester_host: HostName, reply_via: HostName, command: Command) {
        let ready = Arc::new(Notify::new());
        self.forwarded_commands
            .lock()
            .await
            .insert(request_id, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
        let router = self.clone();
        tokio::spawn(async move {
            router.execute_forwarded_command(request_id, requester_host, reply_via, command, ready).await;
        });
    }

    pub(super) fn spawn_forwarded_cancel(&self, cancel_id: u64, requester_host: HostName, reply_via: HostName, command_request_id: u64) {
        let router = self.clone();
        tokio::spawn(async move {
            router.cancel_forwarded_command(cancel_id, requester_host, reply_via, command_request_id).await;
        });
    }

    pub(super) async fn emit_remote_command_event(&self, request_id: u64, responder_host: HostName, event: CommandPeerEvent) {
        let mut pending = self.pending_remote_commands.lock().await;
        let Some(entry) = pending.get_mut(&request_id) else {
            return;
        };

        match event {
            CommandPeerEvent::Started { repo_identity, repo, description } => {
                entry.repo_identity = Some(repo_identity.clone());
                entry.repo = Some(repo.clone());
                self.daemon.send_event(DaemonEvent::CommandStarted {
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
                self.daemon.send_event(DaemonEvent::CommandStepUpdate {
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
                self.daemon.send_event(DaemonEvent::CommandFinished {
                    command_id: entry.command_id,
                    host: responder_host,
                    repo_identity,
                    repo,
                    result,
                });
            }
        }
    }

    pub(super) async fn complete_remote_command(&self, request_id: u64, responder_host: HostName, result: CommandValue) {
        let mut pending = self.pending_remote_commands.lock().await;
        let Some(entry) = pending.remove(&request_id) else {
            return;
        };

        if entry.finished_via_event {
            return;
        }

        let fallback_repo_identity =
            || RepoIdentity { authority: "local".into(), path: entry.repo.clone().unwrap_or_default().display().to_string() };

        self.daemon.send_event(DaemonEvent::CommandFinished {
            command_id: entry.command_id,
            host: responder_host,
            repo_identity: entry
                .repo_identity
                .or_else(|| match &result {
                    CommandValue::TerminalPrepared { repo_identity, .. } => Some(repo_identity.clone()),
                    _ => None,
                })
                .unwrap_or_else(fallback_repo_identity),
            repo: entry.repo.unwrap_or_default(),
            result,
        });
    }

    pub(super) async fn complete_remote_cancel(&self, cancel_id: u64, error: Option<String>) {
        let tx = self.pending_remote_cancels.lock().await.remove(&cancel_id);
        if let Some(tx) = tx {
            let _ = tx.send(match error {
                Some(message) => Err(message),
                None => Ok(()),
            });
        }
    }

    async fn execute_forwarded_command(
        &self,
        request_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        command: Command,
        ready: Arc<Notify>,
    ) {
        let mut event_rx = self.daemon.subscribe();
        let responder_host = self.daemon.host_name().clone();
        let command_id = match self.daemon.execute(command).await {
            Ok(command_id) => command_id,
            Err(message) => {
                self.forwarded_commands.lock().await.remove(&request_id);
                ready.notify_waiters();
                let response = RoutedPeerMessage::CommandResponse {
                    request_id,
                    requester_host,
                    responder_host,
                    remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                    result: Box::new(CommandValue::Error { message }),
                };
                let _ = self.send_routed_to(&reply_via, response).await;
                return;
            }
        };
        if let Some(entry) = self.forwarded_commands.lock().await.get_mut(&request_id) {
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
                    let _ = self.send_routed_to(&reply_via, event).await;
                }
                Ok(DaemonEvent::CommandStepUpdate {
                    command_id: id,
                    repo_identity,
                    repo,
                    step_index,
                    step_count,
                    description,
                    status,
                    ..
                }) if id == command_id => {
                    let event = RoutedPeerMessage::CommandEvent {
                        request_id,
                        requester_host: requester_host.clone(),
                        responder_host: responder_host.clone(),
                        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                        event: Box::new(CommandPeerEvent::StepUpdate { repo_identity, repo, step_index, step_count, description, status }),
                    };
                    let _ = self.send_routed_to(&reply_via, event).await;
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
                    let _ = self.send_routed_pair_to(&reply_via, finished, response).await;
                    self.forwarded_commands.lock().await.remove(&request_id);
                    break;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    self.forwarded_commands.lock().await.remove(&request_id);
                    break;
                }
            }
        }
    }

    async fn cancel_forwarded_command(&self, cancel_id: u64, requester_host: HostName, reply_via: HostName, command_request_id: u64) {
        let responder_host = self.daemon.host_name().clone();
        let error =
            match tokio::time::timeout(Duration::from_secs(5), await_forwarded_command_id(&self.forwarded_commands, command_request_id))
                .await
            {
                Ok(Ok(command_id)) => self.daemon.cancel(command_id).await.err(),
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
        let _ = self.send_routed_to(&reply_via, response).await;
    }

    #[cfg(test)]
    pub(super) async fn execute_forwarded_command_for_test(
        &self,
        request_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        command: Command,
        ready: Arc<Notify>,
    ) {
        self.execute_forwarded_command(request_id, requester_host, reply_via, command, ready).await;
    }

    #[cfg(test)]
    pub(super) async fn cancel_forwarded_command_for_test(
        &self,
        cancel_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        command_request_id: u64,
    ) {
        self.cancel_forwarded_command(cancel_id, requester_host, reply_via, command_request_id).await;
    }
}

impl RemoteCommandRouter {
    async fn resolve_sender(&self, host: &HostName) -> Result<Arc<dyn PeerSender>, String> {
        let pm = self.peer_manager.lock().await;
        pm.resolve_sender(host)
    }

    async fn send_routed_to(&self, host: &HostName, msg: RoutedPeerMessage) -> Result<(), String> {
        let sender = self.resolve_sender(host).await?;
        sender.send(PeerWireMessage::Routed(msg)).await
    }

    async fn send_routed_pair_to(&self, host: &HostName, first: RoutedPeerMessage, second: RoutedPeerMessage) -> Result<(), String> {
        let sender = self.resolve_sender(host).await?;
        sender.send(PeerWireMessage::Routed(first)).await?;
        sender.send(PeerWireMessage::Routed(second)).await
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

pub(super) fn extract_command_repo_identity(command: &Command) -> Option<RepoIdentity> {
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
