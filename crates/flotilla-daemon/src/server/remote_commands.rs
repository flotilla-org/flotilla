use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    step::{RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, RemoteStepProgressUpdate, StepOutcome},
};
use flotilla_protocol::{
    Command, CommandAction, CommandPeerEvent, CommandValue, DaemonEvent, HostName, PeerWireMessage, RepoIdentity, RepoSelector,
    RoutedPeerMessage, Step, StepStatus,
};
use tokio::sync::{oneshot, Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::peer::{PeerManager, PeerSender};

#[derive(Debug)]
pub(super) struct PendingRemoteCommand {
    pub(super) command_id: u64,
    pub(super) target_host: HostName,
    pub(super) repo_identity: Option<RepoIdentity>,
    pub(super) repo: Option<PathBuf>,
    pub(super) finished_via_event: bool,
    /// When set, the originator is waiting for a direct query result rather
    /// than a broadcast `CommandFinished` event.  `complete_remote_command`
    /// resolves this instead of broadcasting.
    pub(super) query_completion: Option<oneshot::Sender<CommandValue>>,
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

struct PendingRemoteStepBatch {
    command_id: u64,
    progress_sink: Arc<dyn RemoteStepProgressSink>,
    failed_message: Option<String>,
    completion: oneshot::Sender<Result<Vec<StepOutcome>, String>>,
}

#[derive(Clone)]
struct ActiveRemoteStepBatch {
    request_id: u64,
    target_host: HostName,
}

#[derive(Clone)]
struct ForwardedRemoteStepBatch {
    state: ForwardedRemoteStepBatchState,
}

#[derive(Clone)]
enum ForwardedRemoteStepBatchState {
    Launching { ready: Arc<Notify> },
    Running { cancel: CancellationToken },
}

type PendingRemoteStepBatchMap = Arc<Mutex<HashMap<u64, PendingRemoteStepBatch>>>;
type ActiveRemoteStepBatchMap = Arc<Mutex<HashMap<u64, ActiveRemoteStepBatch>>>;
struct PendingRemoteStepCancel {
    target_host: HostName,
    completion: oneshot::Sender<Result<(), String>>,
}

type PendingRemoteStepCancelMap = Arc<Mutex<HashMap<u64, PendingRemoteStepCancel>>>;
// TODO(phase-2): if the requester disconnects while a forwarded remote step
// batch is still running, proactively clear the inbound batch state instead of
// waiting for normal task completion.
type ForwardedRemoteStepBatchMap = Arc<Mutex<HashMap<u64, ForwardedRemoteStepBatch>>>;

#[derive(Clone)]
pub(super) struct RemoteCommandRouter {
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    pending_remote_commands: PendingRemoteCommandMap,
    forwarded_commands: ForwardedCommandMap,
    pending_remote_cancels: PendingRemoteCancelMap,
    pending_remote_step_batches: PendingRemoteStepBatchMap,
    active_remote_step_batches: ActiveRemoteStepBatchMap,
    pending_remote_step_cancels: PendingRemoteStepCancelMap,
    forwarded_remote_step_batches: ForwardedRemoteStepBatchMap,
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
        Self {
            daemon,
            peer_manager,
            pending_remote_commands,
            forwarded_commands,
            pending_remote_cancels,
            pending_remote_step_batches: Arc::new(Mutex::new(HashMap::new())),
            active_remote_step_batches: Arc::new(Mutex::new(HashMap::new())),
            pending_remote_step_cancels: Arc::new(Mutex::new(HashMap::new())),
            forwarded_remote_step_batches: Arc::new(Mutex::new(HashMap::new())),
            next_remote_command_id,
        }
    }

    pub(super) async fn dispatch_execute(&self, command: Command) -> Result<u64, String> {
        let target_host = command.host.clone().unwrap_or_else(|| self.daemon.host_name().clone());
        let local = self.daemon.host_name();
        let desc = command.description();
        info!(%target_host, %local, %desc, "dispatch_execute");
        if target_host != *self.daemon.host_name() {
            if command.action.is_query() {
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
                    query_completion: None,
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
                let remote_executor: Arc<dyn RemoteStepExecutor> = Arc::new(self.clone());
                self.daemon.execute_with_remote_executor(command, remote_executor).await
            }
        } else {
            self.daemon.execute(command).await
        }
    }

    /// Dispatch a query command and return the result synchronously.
    ///
    /// For local targets this calls `execute_query` directly.  For remote
    /// targets the command is forwarded via the peer manager and we wait on a
    /// oneshot for the `CommandResponse` to arrive — no `CommandFinished`
    /// broadcast is synthesised.
    pub(super) async fn dispatch_query(&self, command: Command, session_id: uuid::Uuid) -> Result<CommandValue, String> {
        let target_host = command.host.clone().unwrap_or_else(|| self.daemon.host_name().clone());

        if target_host == *self.daemon.host_name() {
            return self.daemon.execute_query(command, session_id).await;
        }

        let request_id = {
            let mut pm = self.peer_manager.lock().await;
            pm.next_request_id()
        };
        let command_id = self.next_remote_command_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();

        self.pending_remote_commands.lock().await.insert(request_id, PendingRemoteCommand {
            command_id,
            target_host: target_host.clone(),
            repo_identity: extract_command_repo_identity(&command),
            repo: None,
            finished_via_event: false,
            query_completion: Some(tx),
        });

        let routed = RoutedPeerMessage::CommandRequest {
            request_id,
            requester_host: self.daemon.host_name().clone(),
            target_host: target_host.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            command: Box::new(command),
        };
        if let Err(err) = self.send_routed_to(&target_host, routed).await {
            self.pending_remote_commands.lock().await.remove(&request_id);
            return Err(err);
        }

        const REMOTE_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
        match tokio::time::timeout(REMOTE_QUERY_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err("remote query response channel closed".to_string()),
            Err(_) => {
                self.pending_remote_commands.lock().await.remove(&request_id);
                Err(format!("timed out waiting for remote query result (command_id={command_id})"))
            }
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

    pub(super) async fn spawn_forwarded_remote_step_batch(
        &self,
        request_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        request: RemoteStepBatchRequest,
    ) {
        let ready = Arc::new(Notify::new());
        self.forwarded_remote_step_batches
            .lock()
            .await
            .insert(request_id, ForwardedRemoteStepBatch { state: ForwardedRemoteStepBatchState::Launching { ready: Arc::clone(&ready) } });
        let router = self.clone();
        tokio::spawn(async move {
            router.execute_forwarded_remote_step_batch(request_id, requester_host, reply_via, request, ready).await;
        });
    }

    pub(super) fn spawn_forwarded_remote_step_cancel(
        &self,
        cancel_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        remote_step_request_id: u64,
    ) {
        let router = self.clone();
        tokio::spawn(async move {
            router.cancel_forwarded_remote_step_batch(cancel_id, requester_host, reply_via, remote_step_request_id).await;
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

        // Query commands: resolve the oneshot directly without broadcasting
        // a CommandFinished event.
        if let Some(tx) = entry.query_completion {
            let _ = tx.send(result);
            return;
        }

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

    pub(super) async fn emit_remote_step_event(
        &self,
        request_id: u64,
        _responder_host: HostName,
        batch_step_index: usize,
        batch_step_count: usize,
        description: String,
        status: StepStatus,
    ) {
        let progress_sink = {
            let mut pending = self.pending_remote_step_batches.lock().await;
            let Some(entry) = pending.get_mut(&request_id) else {
                info!(request_id, "emit_remote_step_event: no pending batch found");
                return;
            };
            if let StepStatus::Failed { message } = &status {
                entry.failed_message = Some(message.clone());
            }
            Arc::clone(&entry.progress_sink)
        };
        progress_sink.emit(RemoteStepProgressUpdate { batch_step_index, batch_step_count, description, status }).await;
    }

    pub(super) async fn complete_remote_step(&self, request_id: u64, _responder_host: HostName, outcomes: Vec<StepOutcome>) {
        info!(request_id, outcome_count = outcomes.len(), "complete_remote_step");
        let entry = self.pending_remote_step_batches.lock().await.remove(&request_id);
        let Some(entry) = entry else {
            return;
        };
        self.active_remote_step_batches.lock().await.remove(&entry.command_id);
        let result = match entry.failed_message {
            Some(message) => Err(message),
            None => Ok(outcomes),
        };
        let _ = entry.completion.send(result);
    }

    pub(super) async fn complete_remote_step_cancel(&self, cancel_id: u64, error: Option<String>) {
        let pending = self.pending_remote_step_cancels.lock().await.remove(&cancel_id);
        if let Some(pending) = pending {
            let _ = pending.completion.send(match error {
                Some(message) => Err(message),
                None => Ok(()),
            });
        }
    }

    pub(super) async fn fail_pending_remote_steps_for_host(&self, host: &HostName) {
        let message = format!("remote step peer disconnected: {host}");

        let request_ids: Vec<u64> = {
            let mut active = self.active_remote_step_batches.lock().await;
            active.extract_if(|_, entry| entry.target_host == *host).map(|(_, entry)| entry.request_id).collect()
        };

        if !request_ids.is_empty() {
            let mut pending_batches = self.pending_remote_step_batches.lock().await;
            for request_id in request_ids {
                if let Some(entry) = pending_batches.remove(&request_id) {
                    let _ = entry.completion.send(Err(message.clone()));
                }
            }
        }

        let cancel_ids: Vec<u64> = {
            let pending_cancels = self.pending_remote_step_cancels.lock().await;
            pending_cancels.iter().filter_map(|(cancel_id, pending)| (pending.target_host == *host).then_some(*cancel_id)).collect()
        };
        if !cancel_ids.is_empty() {
            let mut pending_cancels = self.pending_remote_step_cancels.lock().await;
            for cancel_id in cancel_ids {
                if let Some(pending) = pending_cancels.remove(&cancel_id) {
                    let _ = pending.completion.send(Err(message.clone()));
                }
            }
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
        let responder_host = self.daemon.host_name().clone();

        // Query commands: execute synchronously via execute_query, send the
        // result back directly without subscribing to the event stream.
        if command.action.is_query() {
            let result = match self.daemon.execute_query(command, uuid::Uuid::nil()).await {
                Ok(value) => value,
                Err(message) => CommandValue::Error { message },
            };
            self.forwarded_commands.lock().await.remove(&request_id);
            ready.notify_waiters();
            let response = RoutedPeerMessage::CommandResponse {
                request_id,
                requester_host,
                responder_host,
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                result: Box::new(result),
            };
            let _ = self.send_routed_to(&reply_via, response).await;
            return;
        }

        let mut event_rx = self.daemon.subscribe();
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

    #[cfg(test)]
    pub(super) async fn insert_running_forwarded_remote_step_batch_for_test(&self, request_id: u64, cancel: CancellationToken) {
        self.forwarded_remote_step_batches
            .lock()
            .await
            .insert(request_id, ForwardedRemoteStepBatch { state: ForwardedRemoteStepBatchState::Running { cancel } });
    }

    #[cfg(test)]
    pub(super) async fn cancel_forwarded_remote_step_batch_for_test(
        &self,
        cancel_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        remote_step_request_id: u64,
    ) {
        self.cancel_forwarded_remote_step_batch(cancel_id, requester_host, reply_via, remote_step_request_id).await;
    }
}

#[async_trait]
impl RemoteStepExecutor for RemoteCommandRouter {
    async fn execute_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
    ) -> Result<Vec<StepOutcome>, String> {
        if let Some((index, step)) = request.steps.iter().enumerate().find(|(_, step)| *step.host.host_name() != request.target_host) {
            return Err(format!("remote step {} targets {:?}, expected remote host {}", index, step.host, request.target_host));
        }

        let request_id = {
            let mut pm = self.peer_manager.lock().await;
            pm.next_request_id()
        };
        let (tx, rx) = oneshot::channel();
        self.pending_remote_step_batches.lock().await.insert(request_id, PendingRemoteStepBatch {
            command_id: request.command_id,
            progress_sink,
            failed_message: None,
            completion: tx,
        });
        self.active_remote_step_batches
            .lock()
            .await
            .insert(request.command_id, ActiveRemoteStepBatch { request_id, target_host: request.target_host.clone() });

        let step_count = request.steps.len();
        let command_id = request.command_id;
        let target_host = request.target_host.clone();

        let routed = RoutedPeerMessage::RemoteStepRequest {
            request_id,
            requester_host: self.daemon.host_name().clone(),
            target_host: request.target_host.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            repo_identity: request.repo_identity,
            repo_path: request.repo.as_path().to_path_buf(),
            step_offset: request.step_offset,
            steps: request.steps,
        };

        info!(request_id, command_id, %target_host, step_count, "sending remote step batch");
        if let Err(err) = self.send_routed_to(&target_host, routed).await {
            self.pending_remote_step_batches.lock().await.remove(&request_id);
            self.active_remote_step_batches.lock().await.remove(&command_id);
            return Err(err);
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err("remote step response channel closed".to_string()),
        }
    }

    async fn cancel_active_batch(&self, command_id: u64) -> Result<(), String> {
        let Some(active) = self.active_remote_step_batches.lock().await.get(&command_id).cloned() else {
            return Ok(());
        };

        let cancel_id = {
            let mut pm = self.peer_manager.lock().await;
            pm.next_request_id()
        };
        let (tx, rx) = oneshot::channel();
        self.pending_remote_step_cancels
            .lock()
            .await
            .insert(cancel_id, PendingRemoteStepCancel { target_host: active.target_host.clone(), completion: tx });
        let routed = RoutedPeerMessage::RemoteStepCancelRequest {
            cancel_id,
            requester_host: self.daemon.host_name().clone(),
            target_host: active.target_host.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            remote_step_request_id: active.request_id,
        };
        if let Err(err) = self.send_routed_to(&active.target_host, routed).await {
            self.pending_remote_step_cancels.lock().await.remove(&cancel_id);
            return Err(err);
        }

        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(message),
            Ok(Err(_)) => Err("remote step cancel response channel closed".to_string()),
            Err(_) => {
                self.pending_remote_step_cancels.lock().await.remove(&cancel_id);
                Err("timed out waiting for remote step cancel response".to_string())
            }
        }
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

    async fn execute_forwarded_remote_step_batch(
        &self,
        request_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        request: RemoteStepBatchRequest,
        ready: Arc<Notify>,
    ) {
        let responder_host = self.daemon.host_name().clone();
        let cancel = CancellationToken::new();
        if let Some(entry) = self.forwarded_remote_step_batches.lock().await.get_mut(&request_id) {
            entry.state = ForwardedRemoteStepBatchState::Running { cancel: cancel.clone() };
        }
        ready.notify_waiters();

        let progress_sink = Arc::new(RoutedRemoteStepProgressSink::new(
            self.clone(),
            request_id,
            requester_host.clone(),
            reply_via.clone(),
            responder_host.clone(),
        ));

        let invalid_step = request
            .steps
            .iter()
            .enumerate()
            .find(|(_, step)| *step.host.host_name() != responder_host)
            .map(|(index, step)| (index, step.description.clone()));

        let steps = request.steps.clone();
        let outcomes = if let Some((index, description)) = invalid_step {
            progress_sink.emit_failed(index, steps.len(), description, "remote step batch targets the wrong host".into()).await;
            Err("remote step batch targets the wrong host".to_string())
        } else {
            self.daemon.execute_remote_step_batch(request, progress_sink.clone(), cancel.clone()).await
        };

        if let Err(message) = &outcomes {
            if !cancel.is_cancelled() {
                progress_sink.emit_failed_if_missing(message.clone(), steps.len(), &steps).await;
            }
        }

        let response = RoutedPeerMessage::RemoteStepResponse {
            request_id,
            requester_host,
            responder_host,
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            outcomes: outcomes.unwrap_or_default(),
        };
        let _ = self.send_routed_to(&reply_via, response).await;
        self.forwarded_remote_step_batches.lock().await.remove(&request_id);
    }

    async fn cancel_forwarded_remote_step_batch(
        &self,
        cancel_id: u64,
        requester_host: HostName,
        reply_via: HostName,
        remote_step_request_id: u64,
    ) {
        let responder_host = self.daemon.host_name().clone();
        let error = match tokio::time::timeout(
            Duration::from_secs(5),
            await_forwarded_remote_step_cancel(&self.forwarded_remote_step_batches, remote_step_request_id),
        )
        .await
        {
            Ok(Ok(cancel)) => {
                cancel.cancel();
                None
            }
            Ok(Err(message)) => Some(message),
            Err(_) => Some(format!("timed out waiting for remote step batch registration: {remote_step_request_id}")),
        };

        let response = RoutedPeerMessage::RemoteStepCancelResponse {
            cancel_id,
            requester_host,
            responder_host,
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            error,
        };
        let _ = self.send_routed_to(&reply_via, response).await;
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

async fn await_forwarded_remote_step_cancel(
    forwarded_remote_step_batches: &ForwardedRemoteStepBatchMap,
    request_id: u64,
) -> Result<CancellationToken, String> {
    loop {
        let ready = {
            let forwarded = forwarded_remote_step_batches.lock().await;
            match forwarded.get(&request_id) {
                Some(ForwardedRemoteStepBatch { state: ForwardedRemoteStepBatchState::Running { cancel } }) => {
                    return Ok(cancel.clone());
                }
                Some(ForwardedRemoteStepBatch { state: ForwardedRemoteStepBatchState::Launching { ready } }) => Arc::clone(ready),
                None => return Err(format!("remote step batch not found: {request_id}")),
            }
        };
        ready.notified().await;
    }
}

#[derive(Default)]
struct RoutedRemoteStepProgressState {
    last_started: Option<(usize, usize, String)>,
    saw_failed: bool,
}

struct RoutedRemoteStepProgressSink {
    router: RemoteCommandRouter,
    request_id: u64,
    requester_host: HostName,
    reply_via: HostName,
    responder_host: HostName,
    state: Mutex<RoutedRemoteStepProgressState>,
}

impl RoutedRemoteStepProgressSink {
    fn new(router: RemoteCommandRouter, request_id: u64, requester_host: HostName, reply_via: HostName, responder_host: HostName) -> Self {
        Self { router, request_id, requester_host, reply_via, responder_host, state: Mutex::new(RoutedRemoteStepProgressState::default()) }
    }

    async fn send_update(&self, batch_step_index: usize, batch_step_count: usize, description: String, status: StepStatus) {
        {
            let mut state = self.state.lock().await;
            match &status {
                StepStatus::Started => state.last_started = Some((batch_step_index, batch_step_count, description.clone())),
                StepStatus::Failed { .. } => state.saw_failed = true,
                _ => {}
            }
        }
        let _ = self
            .router
            .send_routed_to(&self.reply_via, RoutedPeerMessage::RemoteStepEvent {
                request_id: self.request_id,
                requester_host: self.requester_host.clone(),
                responder_host: self.responder_host.clone(),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                batch_step_index,
                batch_step_count,
                description,
                status,
            })
            .await;
    }

    async fn emit_failed(&self, batch_step_index: usize, batch_step_count: usize, description: String, message: String) {
        self.send_update(batch_step_index, batch_step_count, description, StepStatus::Failed { message }).await;
    }

    async fn emit_failed_if_missing(&self, message: String, batch_step_count: usize, steps: &[Step]) {
        let failure = {
            let state = self.state.lock().await;
            if state.saw_failed {
                None
            } else {
                state.last_started.clone().or_else(|| steps.first().map(|step| (0usize, batch_step_count, step.description.clone())))
            }
        };

        if let Some((batch_step_index, batch_step_count, description)) = failure {
            self.emit_failed(batch_step_index, batch_step_count, description, message).await;
        }
    }
}

#[async_trait]
impl RemoteStepProgressSink for RoutedRemoteStepProgressSink {
    async fn emit(&self, update: RemoteStepProgressUpdate) {
        self.send_update(update.batch_step_index, update.batch_step_count, update.description, update.status).await;
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
