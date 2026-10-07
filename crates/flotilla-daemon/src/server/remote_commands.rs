use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, OnceLock,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    command_target::{RemoteDelivery, TargetError, TargetHost, TargetReason},
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    step::{RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, RemoteStepProgressUpdate, StepOutcome},
};
use flotilla_protocol::{
    Command, CommandAction, CommandPeerEvent, CommandValue, CrewCommandContext, DaemonEvent, EnvironmentId, HostName, NodeId,
    PeerWireMessage, RepoIdentity, RepoSelector, RoutedPeerMessage, Step, StepStatus,
};
use flotilla_resources::CrewCompletionPending;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    blob_store::TieredBlobStore,
    peer::{PeerManager, PeerSender},
};

fn command_action_name(command: &Command) -> &'static str {
    match &command.action {
        CommandAction::ConvoyDelete { .. } => "convoy_delete",
        CommandAction::ConvoyLink { .. } => "convoy_link",
        CommandAction::ConvoyUnlink { .. } => "convoy_unlink",
        CommandAction::ConvoyAbandon { .. } => "convoy_abandon",
        CommandAction::ConvoyResume { .. } => "convoy_resume",
        CommandAction::CrewComplete { .. } => "crew_complete",
        CommandAction::CrewFail { .. } => "crew_fail",
        CommandAction::CrewStall { .. } => "crew_stall",
        CommandAction::CrewSupervise { .. } => "crew_supervise",
        CommandAction::CrewHandoff { .. } => "crew_handoff",
        CommandAction::ResourceApply { .. } => "resource_apply",
        CommandAction::ResourceDelete { .. } => "resource_delete",
        CommandAction::ResourceStatusPatch { .. } => "resource_status_patch",
        _ => command.description(),
    }
}

fn command_subject(action: &CommandAction) -> String {
    match action {
        CommandAction::ConvoyDelete { namespace, name, .. }
        | CommandAction::ConvoyLink { namespace, name, .. }
        | CommandAction::ConvoyUnlink { namespace, name, .. }
        | CommandAction::ConvoyAbandon { namespace, name, .. }
        | CommandAction::ConvoyResume { namespace, name, .. } => {
            format!("convoy:{}/{}", namespace.as_deref().unwrap_or("default"), name)
        }
        CommandAction::CrewSupervise { namespace, convoy, .. } => {
            format!("convoy:{}/{}", namespace.as_deref().unwrap_or("default"), convoy)
        }
        CommandAction::CrewComplete { context, .. }
        | CommandAction::CrewFail { context, .. }
        | CommandAction::CrewStall { context, .. }
        | CommandAction::CrewHandoff { context, .. } => format!(
            "crew:{}/{}/{}/{}",
            context.namespace.as_deref().unwrap_or("default"),
            context.convoy.as_deref().unwrap_or("unknown"),
            context.vessel_ref.as_deref().unwrap_or("unknown"),
            context.role.as_deref().unwrap_or("unknown")
        ),
        CommandAction::ResourceApply { namespace, document } => {
            let kind = document.get("kind").and_then(serde_json::Value::as_str).unwrap_or("resource");
            let name = document.pointer("/metadata/name").and_then(serde_json::Value::as_str).unwrap_or("unknown");
            format!("{}:{}/{}", kind.to_lowercase(), namespace, name)
        }
        CommandAction::ResourceDelete { namespace, kind, name, .. } | CommandAction::ResourceStatusPatch { namespace, kind, name, .. } => {
            format!("{kind}:{namespace}/{name}")
        }
        _ => "unspecified".to_string(),
    }
}

#[derive(Debug, bon::Builder)]
pub(super) struct PendingRemoteCommand {
    pub(super) command_id: u64,
    pub(super) target_node_id: NodeId,
    pub(super) repo_identity: Option<RepoIdentity>,
    pub(super) repo: Option<PathBuf>,
    pub(super) finished_via_event: bool,
    /// When set, the originator is waiting for a direct acknowledgement rather
    /// than a broadcast `CommandFinished` event. `complete_remote_command`
    /// resolves this instead of broadcasting.
    pub(super) query_completion: Option<oneshot::Sender<CommandValue>>,
    pub(super) crew_completion: Option<PendingCrewCompletionRoute>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingCrewCompletionRoute {
    namespace: String,
    convoy: String,
    session_name: String,
    context: CrewCommandContext,
    message: Option<String>,
    disposition: Option<String>,
    decision_ledger_ref: Option<String>,
    force: bool,
    principal_ref: Option<flotilla_protocol::PrincipalRef>,
    authority: Option<HostName>,
}

#[derive(Clone)]
struct CrewCompletionRetryState {
    completion: PendingCrewCompletionRoute,
    generation: u64,
}

#[derive(Debug, Clone)]
pub(super) struct ForwardedCommand {
    pub(super) state: ForwardedCommandState,
}

struct ForwardedCommandOrigin {
    caller: Option<flotilla_protocol::CommandCaller>,
    session_id: Option<uuid::Uuid>,
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
    target_node_id: NodeId,
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
    target_node_id: NodeId,
    completion: oneshot::Sender<Result<(), String>>,
}

type PendingRemoteStepCancelMap = Arc<Mutex<HashMap<u64, PendingRemoteStepCancel>>>;
// TODO(phase-2): if the requester disconnects while a forwarded remote step
// batch is still running, proactively clear the inbound batch state instead of
// waiting for normal task completion.
type ForwardedRemoteStepBatchMap = Arc<Mutex<HashMap<u64, ForwardedRemoteStepBatch>>>;

#[derive(Clone)]
pub(super) struct RemoteCommandRouter {
    inner: Arc<RemoteCommandRouterInner>,
}

pub(super) struct RemoteCommandRouterInner {
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
    /// Session name -> latest completion intent and its update generation.
    retrying_crew_completions: Arc<StdMutex<HashMap<String, CrewCompletionRetryState>>>,
    blob_store: Arc<OnceLock<Arc<TieredBlobStore>>>,
}

impl std::ops::Deref for RemoteCommandRouter {
    type Target = RemoteCommandRouterInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl flotilla_core::leaf_engine::ResourceIntentPublisher for RemoteCommandRouterInner {
    async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<flotilla_protocol::ResourceRef, String> {
        let router = RemoteCommandRouter { inner: self };
        let command = Command::builder().action(CommandAction::ResourceApply { namespace: namespace.to_string(), document }).build();
        match router.dispatch_and_wait(command, uuid::Uuid::nil()).await? {
            CommandValue::ResourceObject(object) => Ok(flotilla_protocol::ResourceRef::new(
                object.value.get("apiVersion").and_then(serde_json::Value::as_str).unwrap_or("flotilla.work/v1"),
                &object.kind,
                &object.namespace,
                object
                    .value
                    .pointer("/metadata/name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("resource admission response has no name")?,
            )),
            CommandValue::Error { message } => Err(message),
            value => Err(format!("unexpected resource intent admission result: {value:?}")),
        }
    }
    async fn patch_status(
        self: Arc<Self>,
        namespace: &str,
        kind: &str,
        name: &str,
        status: serde_json::Value,
        expected_resource_version: &str,
    ) -> Result<(), String> {
        let router = RemoteCommandRouter { inner: self };
        let command = Command::builder()
            .action(CommandAction::ResourceStatusPatch {
                namespace: namespace.into(),
                kind: kind.into(),
                name: name.into(),
                status,
                expected_resource_version: Some(expected_resource_version.into()),
            })
            .build();
        match router.dispatch_and_wait(command, uuid::Uuid::nil()).await? {
            CommandValue::ResourceObject(_) => Ok(()),
            CommandValue::Error { message } => Err(message),
            value => Err(format!("unexpected resource status mutation result: {value:?}")),
        }
    }
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
        let inner = Arc::new(RemoteCommandRouterInner {
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
            retrying_crew_completions: Arc::new(StdMutex::new(HashMap::new())),
            blob_store: Arc::new(OnceLock::new()),
        });
        let publisher: Arc<dyn flotilla_core::leaf_engine::ResourceIntentPublisher> = inner.clone();
        inner.daemon.set_resource_intent_publisher(Arc::downgrade(&publisher));
        Self { inner }
    }

    pub(super) fn install_blob_store(&self, store: Arc<TieredBlobStore>) -> Result<(), String> {
        self.blob_store.set(store).map_err(|_| "blob store already installed".to_string())
    }

    pub(super) fn blob_store(&self) -> Result<Arc<TieredBlobStore>, String> {
        self.blob_store.get().cloned().ok_or_else(|| "blob store is not running".to_string())
    }

    pub(super) async fn target_node_id(&self, target: &TargetHost) -> Result<NodeId, TargetError> {
        match target {
            TargetHost::Local => Ok(self.daemon.node_id().clone()),
            TargetHost::Node(node) => Ok(node.clone()),
            TargetHost::ConvoyHome(home) => Ok(home.node_id.clone()),
            TargetHost::Placement(host_id) => {
                let peer_manager = self.peer_manager.lock().await;
                let environment_id = EnvironmentId::host(host_id.clone());
                let (node_id, host_name) = peer_manager
                    .node_for_host_environment(&environment_id)
                    .map_err(|_| TargetError::Unreachable(format!("peer host {host_id} is not connected")))?;
                peer_manager
                    .resolve_sender(&node_id)
                    .map_err(|_| TargetError::Unreachable(format!("peer host {host_name} is not connected")))?;
                Ok(node_id)
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn dispatch_execute(&self, command: Command) -> Result<u64, String> {
        self.dispatch_execute_for_caller(command, None).await
    }

    pub(super) async fn dispatch_execute_for_principal(
        &self,
        command: Command,
        dispatching_principal_ref: Option<flotilla_protocol::PrincipalRef>,
    ) -> Result<u64, String> {
        let caller =
            dispatching_principal_ref.map(|principal_ref| flotilla_protocol::CommandCaller { principal_ref, process: None, crew: None });
        self.dispatch_execute_for_caller(command, caller).await
    }

    async fn dispatch_remote_command(
        &self,
        command: Command,
        caller: Option<flotilla_protocol::CommandCaller>,
        target: &flotilla_core::command_target::CommandTarget,
        crew_completion: Option<PendingCrewCompletionRoute>,
        target_node_id: NodeId,
    ) -> Result<u64, String> {
        let existing_convoy_target = match &target.host {
            TargetHost::ConvoyHome(home) => Some(home.clone()),
            _ => None,
        };
        if let (Some(completion), Some(target)) = (&crew_completion, &existing_convoy_target) {
            self.persist_crew_completion(completion, &target.home, &format!("authority acknowledgement pending for {}", completion.convoy))
                .await;
        }
        let request_id = {
            let mut pm = self.peer_manager.lock().await;
            pm.next_request_id()
        };
        let command_id = self.next_remote_command_id.fetch_add(1, Ordering::Relaxed);
        self.pending_remote_commands.lock().await.insert(
            request_id,
            PendingRemoteCommand::builder()
                .command_id(command_id)
                .target_node_id(target_node_id.clone())
                .maybe_repo_identity(extract_command_repo_identity(&command))
                .finished_via_event(false)
                .maybe_crew_completion(crew_completion.clone())
                .build(),
        );

        let routed = RoutedPeerMessage::CommandRequest {
            request_id,
            requester_node_id: self.daemon.node_id().clone(),
            target_node_id: target_node_id.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            command: Box::new(command),
            caller: caller.map(Box::new),
            session_id: None,
        };
        let send_result = match &existing_convoy_target {
            Some(target) => self.send_routed_to_convoy_home(&target.home, &target.node_id, routed).await,
            None => self.send_routed_to(&target_node_id, routed).await,
        }
        .map_err(|error| match &target.host {
            TargetHost::Node(origin) if target.reason == TargetReason::RecordHome => {
                format!("resource origin {origin} is unreachable: {error}")
            }
            TargetHost::Node(origin) if target.reason == TargetReason::CrewSessionHome => {
                format!("session origin {origin} is unreachable: {error}")
            }
            _ => error,
        });

        match send_result {
            Ok(()) => Ok(command_id),
            Err(err) => {
                self.pending_remote_commands.lock().await.remove(&request_id);
                match (crew_completion, existing_convoy_target) {
                    (Some(completion), Some(target)) => {
                        let message = self.authority_unreachable_message(&completion.convoy, &target.home, &err);
                        self.persist_crew_completion(&completion, &target.home, &message).await;
                        self.spawn_crew_completion_retry(completion);
                        Err(format!("completion pending: {message}"))
                    }
                    (_, Some(target)) => Err(target.unreachable_message(&err)),
                    _ => Err(err),
                }
            }
        }
    }

    pub(super) async fn dispatch_execute_for_caller(
        &self,
        mut command: Command,
        caller: Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = caller.as_ref().map(|caller| caller.principal_ref.clone());
        let mut crew_completion = self.resolve_crew_command_routing(&mut command.action).await?;
        if let Some(completion) = &mut crew_completion {
            completion.principal_ref = dispatching_principal_ref.clone();
        }
        let target =
            self.daemon.resolve_command_target(&command.action, command.node_id.as_ref()).await.map_err(|error| error.to_string())?;
        let existing_convoy_target = match &target.host {
            TargetHost::ConvoyHome(home) => Some(home.clone()),
            _ => None,
        };
        if let (Some(completion), Some(target)) = (&mut crew_completion, &existing_convoy_target) {
            completion.authority = Some(target.home.clone());
        }
        if let (Some(completion), None) = (&crew_completion, &existing_convoy_target) {
            if !self.daemon.has_authoritative_convoy(&completion.namespace, &completion.convoy).await? {
                let authority = HostName::new("unknown");
                let message = format!("authority unreachable for {}: no authority route is available", completion.convoy);
                self.persist_crew_completion(completion, &authority, &message).await;
                self.spawn_crew_completion_retry(completion.clone());
                return Err(format!("completion pending: {message}"));
            }
        }
        let target_node_id = self.target_node_id(&target.host).await.map_err(|error| error.to_string())?;
        command.node_id = if matches!(&target.host, TargetHost::Local) { None } else { Some(target_node_id.clone()) };
        let local = self.daemon.node_id();
        let desc = command.description();
        let action = command_action_name(&command);
        let subject = command_subject(&command.action);
        let caller_label = caller.as_ref().map(ToString::to_string).unwrap_or_else(|| "unattributed".to_string());
        info!(%target_node_id, %local, %caller_label, %action, %subject, %desc, reason = ?target.reason, "dispatch_execute");
        if target_node_id != *self.daemon.node_id() {
            if target.delivery == RemoteDelivery::Command {
                Box::pin(self.dispatch_remote_command(command, caller, &target, crew_completion, target_node_id)).await
            } else {
                let remote_executor: Arc<dyn RemoteStepExecutor> = Arc::new(self.clone());
                Box::pin(self.daemon.execute_with_remote_executor(command, remote_executor)).await
            }
        } else {
            Box::pin(self.daemon.execute_for_caller(command, caller)).await
        }
    }

    pub(super) async fn dispatch_query(&self, command: Command, session_id: uuid::Uuid) -> Result<CommandValue, String> {
        self.dispatch_and_wait(command, session_id).await
    }

    /// Return the destination's acknowledgement for a remote command or query.
    /// Local queries retain surface projection; resource mutations await the same
    /// acknowledgement at both local and remote receiver homes.
    async fn dispatch_and_wait(&self, mut command: Command, session_id: uuid::Uuid) -> Result<CommandValue, String> {
        self.resolve_crew_command_routing(&mut command.action).await?;
        let target =
            self.daemon.resolve_command_target(&command.action, command.node_id.as_ref()).await.map_err(|error| error.to_string())?;
        let existing_convoy_target = match &target.host {
            TargetHost::ConvoyHome(home) => Some(home.clone()),
            _ => None,
        };
        let crew_convoy = match &command.action {
            CommandAction::QueryCrewList { context } => context.convoy.clone(),
            _ => None,
        };
        let target_node_id = self.target_node_id(&target.host).await.map_err(|error| error.to_string())?;
        command.node_id = if matches!(&target.host, TargetHost::Local) { None } else { Some(target_node_id.clone()) };

        if target_node_id == *self.daemon.node_id() {
            if command.action.is_query() {
                return self.execute_projected_query(command, session_id).await;
            }
            let mut events = self.daemon.subscribe();
            let command_id = self.daemon.execute_for_caller(command, None).await?;
            return tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await {
                        Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => return Ok(result),
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err("local resource acknowledgement channel closed".into())
                        }
                    }
                }
            })
            .await
            .map_err(|_| "timed out waiting for local resource acknowledgement".to_string())?;
        }

        let request_id = {
            let mut pm = self.peer_manager.lock().await;
            pm.next_request_id()
        };
        let command_id = self.next_remote_command_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();

        self.pending_remote_commands.lock().await.insert(
            request_id,
            PendingRemoteCommand::builder()
                .command_id(command_id)
                .target_node_id(target_node_id.clone())
                .maybe_repo_identity(extract_command_repo_identity(&command))
                .finished_via_event(false)
                .query_completion(tx)
                .build(),
        );

        let routed = RoutedPeerMessage::CommandRequest {
            request_id,
            requester_node_id: self.daemon.node_id().clone(),
            target_node_id: target_node_id.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            command: Box::new(command),
            caller: None,
            session_id: Some(session_id),
        };
        if let Err(err) = self.send_routed_to(&target_node_id, routed).await {
            self.pending_remote_commands.lock().await.remove(&request_id);
            return Err(match existing_convoy_target {
                Some(target) => {
                    format!("authority unreachable for {} at {}: {err}", crew_convoy.as_deref().unwrap_or("convoy"), target.home)
                }
                None => err,
            });
        }

        const REMOTE_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
        let result = match tokio::time::timeout(REMOTE_QUERY_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err("remote query response channel closed".to_string()),
            Err(_) => {
                self.pending_remote_commands.lock().await.remove(&request_id);
                Err(format!("timed out waiting for remote query result (command_id={command_id})"))
            }
        };

        result
    }

    async fn execute_projected_query(&self, command: Command, session_id: uuid::Uuid) -> Result<CommandValue, String> {
        let mut value = self.daemon.execute_query(command, session_id).await?;
        if let CommandValue::HostList(response) = &mut value {
            self.peer_manager.lock().await.project_host_list(response);
        }
        Ok(value)
    }

    pub(super) async fn dispatch_cancel(&self, command_id: u64) -> Result<(), String> {
        let remote = {
            let pending = self.pending_remote_commands.lock().await;
            pending
                .iter()
                .find(|(_, entry)| entry.command_id == command_id)
                .map(|(request_id, entry)| (*request_id, entry.target_node_id.clone()))
        };
        if let Some((command_request_id, target_node_id)) = remote {
            let cancel_id = {
                let mut pm = self.peer_manager.lock().await;
                pm.next_request_id()
            };
            let (tx, rx) = oneshot::channel();
            self.pending_remote_cancels.lock().await.insert(cancel_id, tx);
            let routed = RoutedPeerMessage::CommandCancelRequest {
                cancel_id,
                requester_node_id: self.daemon.node_id().clone(),
                target_node_id: target_node_id.clone(),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                command_request_id,
            };
            let send_result = self.send_routed_to(&target_node_id, routed).await;
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

    pub(super) async fn spawn_forwarded_command(
        &self,
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command: Command,
        caller: Option<Box<flotilla_protocol::CommandCaller>>,
        session_id: Option<uuid::Uuid>,
    ) {
        let ready = Arc::new(Notify::new());
        self.forwarded_commands
            .lock()
            .await
            .insert(request_id, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
        let router = self.clone();
        tokio::spawn(async move {
            let origin = ForwardedCommandOrigin { caller: caller.map(|caller| *caller), session_id };
            router.execute_forwarded_command(request_id, requester_node_id, reply_via, command, origin, ready).await;
        });
    }

    pub(super) fn spawn_forwarded_cancel(&self, cancel_id: u64, requester_node_id: NodeId, reply_via: NodeId, command_request_id: u64) {
        let router = self.clone();
        tokio::spawn(async move {
            router.cancel_forwarded_command(cancel_id, requester_node_id, reply_via, command_request_id).await;
        });
    }

    pub(super) async fn spawn_forwarded_remote_step_batch(
        &self,
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        request: RemoteStepBatchRequest,
    ) {
        let ready = Arc::new(Notify::new());
        self.forwarded_remote_step_batches
            .lock()
            .await
            .insert(request_id, ForwardedRemoteStepBatch { state: ForwardedRemoteStepBatchState::Launching { ready: Arc::clone(&ready) } });
        let router = self.clone();
        tokio::spawn(async move {
            router.execute_forwarded_remote_step_batch(request_id, requester_node_id, reply_via, request, ready).await;
        });
    }

    pub(super) fn spawn_forwarded_remote_step_cancel(
        &self,
        cancel_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        remote_step_request_id: u64,
    ) {
        let router = self.clone();
        tokio::spawn(async move {
            router.cancel_forwarded_remote_step_batch(cancel_id, requester_node_id, reply_via, remote_step_request_id).await;
        });
    }

    pub(super) async fn emit_remote_command_event(&self, request_id: u64, responder_node_id: NodeId, event: CommandPeerEvent) {
        let event = match event {
            CommandPeerEvent::Finished { repo_identity, repo, result } => {
                CommandPeerEvent::Finished { repo_identity, repo, result: self.route_remote_result(result).await }
            }
            event => event,
        };
        let mut pending = self.pending_remote_commands.lock().await;
        let Some(entry) = pending.get_mut(&request_id) else {
            return;
        };

        match event {
            CommandPeerEvent::Started { repo_identity, repo, description } => {
                entry.repo_identity = Some(repo_identity.clone());
                entry.repo = repo.clone();
                self.daemon.send_event(DaemonEvent::CommandStarted {
                    command_id: entry.command_id,
                    node_id: responder_node_id,
                    repo_identity,
                    repo,
                    description,
                });
            }
            CommandPeerEvent::StepUpdate { repo_identity, repo, step_index, step_count, description, status } => {
                entry.repo_identity = Some(repo_identity.clone());
                entry.repo = repo.clone();
                self.daemon.send_event(DaemonEvent::CommandStepUpdate {
                    command_id: entry.command_id,
                    node_id: responder_node_id,
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
                entry.repo = repo.clone();
                entry.finished_via_event = true;
                self.daemon.send_event(DaemonEvent::CommandFinished {
                    command_id: entry.command_id,
                    node_id: responder_node_id,
                    repo_identity,
                    repo,
                    result,
                });
            }
        }
    }

    pub(super) async fn complete_remote_command(&self, request_id: u64, responder_node_id: NodeId, result: CommandValue) {
        let result = self.route_remote_result(result).await;
        let mut pending = self.pending_remote_commands.lock().await;
        let Some(entry) = pending.remove(&request_id) else {
            return;
        };
        drop(pending);

        if let Some(completion) = entry.crew_completion.clone() {
            self.finish_crew_completion(completion, &result).await;
        }

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
            node_id: responder_node_id,
            repo_identity: entry
                .repo_identity
                .or_else(|| match &result {
                    CommandValue::TerminalPrepared { repo_identity, .. } => Some(repo_identity.clone()),
                    _ => None,
                })
                .unwrap_or_else(fallback_repo_identity),
            repo: entry.repo,
            result,
        });
    }

    async fn route_remote_result(&self, result: CommandValue) -> CommandValue {
        let CommandValue::ConvoyStarted { name, attach_plan, binding } = result else {
            return result;
        };
        let Some(binding) = binding else {
            return CommandValue::ConvoyStarted { name, attach_plan, binding: None };
        };
        if attach_plan.is_none() || binding.host == *self.daemon.host_name() {
            return CommandValue::ConvoyStarted { name, attach_plan, binding: Some(binding) };
        }
        match self.daemon.route_remote_attach_binding(&binding).await {
            Ok(plan) => CommandValue::ConvoyStarted { name, attach_plan: Some(plan), binding: Some(binding) },
            Err(message) => CommandValue::Error { message: format!("convoy {name} started, but remote attach routing failed: {message}") },
        }
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
        _responder_node_id: NodeId,
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

    pub(super) async fn complete_remote_step(&self, request_id: u64, _responder_node_id: NodeId, outcomes: Vec<StepOutcome>) {
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

    pub(super) async fn fail_pending_remote_steps_for_host(&self, node_id: &NodeId) {
        let message = format!("remote step peer disconnected: {node_id}");

        let request_ids: Vec<u64> = {
            let mut active = self.active_remote_step_batches.lock().await;
            active.extract_if(|_, entry| entry.target_node_id == *node_id).map(|(_, entry)| entry.request_id).collect()
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
            pending_cancels.iter().filter_map(|(cancel_id, pending)| (pending.target_node_id == *node_id).then_some(*cancel_id)).collect()
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

    pub(super) async fn fail_pending_remote_commands_for_host(&self, node_id: &NodeId) {
        let failed = {
            let mut pending = self.pending_remote_commands.lock().await;
            pending.extract_if(|_, entry| entry.target_node_id == *node_id).map(|(_, entry)| entry).collect::<Vec<_>>()
        };
        for entry in failed {
            let transport_error = format!("remote command peer disconnected: {node_id}");
            let message = if let Some(completion) = entry.crew_completion.clone() {
                let authority = HostName::new(node_id.to_string());
                let pending_message = self.authority_unreachable_message(&completion.convoy, &authority, &transport_error);
                self.persist_crew_completion(&completion, &authority, &pending_message).await;
                self.spawn_crew_completion_retry(completion);
                format!("completion pending: {pending_message}")
            } else {
                transport_error
            };
            if let Some(completion) = entry.query_completion {
                let _ = completion.send(CommandValue::Error { message });
                continue;
            }
            let repo_identity = entry.repo_identity.unwrap_or_else(|| RepoIdentity { authority: "local".into(), path: String::new() });
            self.daemon.send_event(DaemonEvent::CommandFinished {
                command_id: entry.command_id,
                node_id: node_id.clone(),
                repo_identity,
                repo: entry.repo,
                result: CommandValue::Error { message },
            });
        }
    }

    async fn execute_forwarded_command(
        &self,
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command: Command,
        origin: ForwardedCommandOrigin,
        ready: Arc<Notify>,
    ) {
        let responder_node_id = self.daemon.node_id().clone();

        // Query commands: execute synchronously via execute_query, send the
        // result back directly without subscribing to the event stream.
        if command.action.is_query() {
            let query_session = origin.session_id.unwrap_or(uuid::Uuid::nil());
            let result = match self.execute_projected_query(command, query_session).await {
                Ok(value) => value,
                Err(message) => CommandValue::Error { message },
            };
            self.forwarded_commands.lock().await.remove(&request_id);
            ready.notify_waiters();
            let response = RoutedPeerMessage::CommandResponse {
                request_id,
                requester_node_id,
                responder_node_id,
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                result: Box::new(result),
            };
            let _ = self.send_routed_to(&reply_via, response).await;
            return;
        }

        let mut event_rx = self.daemon.subscribe();
        let command_id = match self.daemon.execute_for_caller(command, origin.caller).await {
            Ok(command_id) => command_id,
            Err(message) => {
                self.forwarded_commands.lock().await.remove(&request_id);
                ready.notify_waiters();
                let response = RoutedPeerMessage::CommandResponse {
                    request_id,
                    requester_node_id,
                    responder_node_id,
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
                        requester_node_id: requester_node_id.clone(),
                        responder_node_id: responder_node_id.clone(),
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
                        requester_node_id: requester_node_id.clone(),
                        responder_node_id: responder_node_id.clone(),
                        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                        event: Box::new(CommandPeerEvent::StepUpdate { repo_identity, repo, step_index, step_count, description, status }),
                    };
                    let _ = self.send_routed_to(&reply_via, event).await;
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, repo_identity, repo, result, .. }) if id == command_id => {
                    let finished = RoutedPeerMessage::CommandEvent {
                        request_id,
                        requester_node_id: requester_node_id.clone(),
                        responder_node_id: responder_node_id.clone(),
                        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                        event: Box::new(CommandPeerEvent::Finished { repo_identity, repo, result: result.clone() }),
                    };
                    let response = RoutedPeerMessage::CommandResponse {
                        request_id,
                        requester_node_id,
                        responder_node_id,
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

    async fn cancel_forwarded_command(&self, cancel_id: u64, requester_node_id: NodeId, reply_via: NodeId, command_request_id: u64) {
        let responder_node_id = self.daemon.node_id().clone();
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
            requester_node_id,
            responder_node_id,
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            error,
        };
        let _ = self.send_routed_to(&reply_via, response).await;
    }

    #[cfg(test)]
    pub(super) async fn execute_forwarded_command_for_test(
        &self,
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command: Command,
        ready: Arc<Notify>,
    ) {
        let origin = ForwardedCommandOrigin { caller: None, session_id: None };
        self.execute_forwarded_command(request_id, requester_node_id, reply_via, command, origin, ready).await;
    }

    #[cfg(test)]
    pub(super) async fn cancel_forwarded_command_for_test(
        &self,
        cancel_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        command_request_id: u64,
    ) {
        self.cancel_forwarded_command(cancel_id, requester_node_id, reply_via, command_request_id).await;
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
        requester_node_id: NodeId,
        reply_via: NodeId,
        remote_step_request_id: u64,
    ) {
        self.cancel_forwarded_remote_step_batch(cancel_id, requester_node_id, reply_via, remote_step_request_id).await;
    }
}

#[async_trait]
impl RemoteStepExecutor for RemoteCommandRouter {
    async fn execute_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
    ) -> Result<Vec<StepOutcome>, String> {
        if let Some((index, step)) = request.steps.iter().enumerate().find(|(_, step)| step.host.node_id() != &request.target_node_id) {
            return Err(format!("remote step {} targets {:?}, expected remote node {}", index, step.host, request.target_node_id));
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
            .insert(request.command_id, ActiveRemoteStepBatch { request_id, target_node_id: request.target_node_id.clone() });

        let step_count = request.steps.len();
        let command_id = request.command_id;
        let target_node_id = request.target_node_id.clone();

        let routed = RoutedPeerMessage::RemoteStepRequest {
            request_id,
            requester_node_id: self.daemon.node_id().clone(),
            target_node_id: request.target_node_id.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            repo_identity: request.repo_identity,
            step_offset: request.step_offset,
            steps: request.steps,
        };

        info!(request_id, command_id, %target_node_id, step_count, "sending remote step batch");
        if let Err(err) = self.send_routed_to(&target_node_id, routed).await {
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
            .insert(cancel_id, PendingRemoteStepCancel { target_node_id: active.target_node_id.clone(), completion: tx });
        let routed = RoutedPeerMessage::RemoteStepCancelRequest {
            cancel_id,
            requester_node_id: self.daemon.node_id().clone(),
            target_node_id: active.target_node_id.clone(),
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            remote_step_request_id: active.request_id,
        };
        if let Err(err) = self.send_routed_to(&active.target_node_id, routed).await {
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
    async fn resolve_crew_command_routing(&self, action: &mut CommandAction) -> Result<Option<PendingCrewCompletionRoute>, String> {
        let (context, completion) = match action {
            CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref, force } => {
                (context, Some((message.clone(), disposition.clone(), decision_ledger_ref.clone(), *force)))
            }
            CommandAction::CrewFail { context, .. }
            | CommandAction::CrewStall { context, .. }
            | CommandAction::CrewHandoff { context, .. }
            | CommandAction::QueryCrewList { context }
            | CommandAction::QueryCrewCapabilities { context } => (context, None),
            _ => return Ok(None),
        };
        let routing = self.daemon.resolve_crew_routing_context(context).await?;
        *context = routing.command_context.clone();
        let Some((message, disposition, decision_ledger_ref, force)) = completion else { return Ok(None) };
        let Some(session_name) = routing.session_name else { return Ok(None) };
        Ok(Some(PendingCrewCompletionRoute {
            namespace: context.namespace.clone().expect("resolved crew context has namespace"),
            convoy: routing.convoy,
            session_name,
            context: context.clone(),
            message,
            disposition,
            decision_ledger_ref,
            force,
            principal_ref: None,
            authority: None,
        }))
    }

    fn authority_unreachable_message(&self, convoy: &str, authority: &HostName, cause: &str) -> String {
        format!("authority unreachable for {convoy} at {authority}: {cause}")
    }

    async fn persist_crew_completion(&self, completion: &PendingCrewCompletionRoute, authority: &HostName, last_error: &str) {
        let pending = CrewCompletionPending {
            message: completion.message.clone(),
            disposition: completion.disposition.clone(),
            decision_ledger_ref: completion.decision_ledger_ref.clone(),
            force: completion.force,
            principal_ref: completion.principal_ref.clone(),
            attempted_at: chrono::Utc::now(),
            authority: authority.to_string(),
            last_error: last_error.to_string(),
        };
        if let Err(error) = self.daemon.mark_crew_completion_pending(&completion.namespace, &completion.session_name, pending).await {
            tracing::warn!(session = %completion.session_name, %error, "failed to persist pending crew completion");
        }
    }

    async fn finish_crew_completion(&self, completion: PendingCrewCompletionRoute, result: &CommandValue) {
        match result {
            CommandValue::Ok | CommandValue::Error { .. } => {
                if let Err(error) = self.daemon.clear_crew_completion_pending(&completion.namespace, &completion.session_name).await {
                    tracing::warn!(session = %completion.session_name, %error, "failed to clear acknowledged crew completion");
                }
            }
            _ => {}
        }
    }

    fn spawn_crew_completion_retry(&self, completion: PendingCrewCompletionRoute) {
        let session_name = completion.session_name.clone();
        {
            let mut retrying = self.retrying_crew_completions.lock().expect("crew completion retry lock");
            if let Some(state) = retrying.get_mut(&session_name) {
                state.completion = completion;
                state.generation = state.generation.wrapping_add(1);
                return;
            }
            retrying.insert(session_name.clone(), CrewCompletionRetryState { completion, generation: 0 });
        }
        let router = self.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            loop {
                tokio::time::sleep(delay).await;
                let Some(state) = router.retrying_crew_completions.lock().expect("crew completion retry lock").get(&session_name).cloned()
                else {
                    return;
                };
                let command = Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::CrewComplete {
                        context: state.completion.context.clone(),
                        message: state.completion.message.clone(),
                        disposition: state.completion.disposition.clone(),
                        decision_ledger_ref: state.completion.decision_ledger_ref.clone(),
                        force: state.completion.force,
                    },
                };
                if router.dispatch_execute_for_principal(command, state.completion.principal_ref.clone()).await.is_ok() {
                    let mut retrying = router.retrying_crew_completions.lock().expect("crew completion retry lock");
                    if retrying.get(&session_name).is_some_and(|current| current.generation == state.generation) {
                        retrying.remove(&session_name);
                        return;
                    }
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        });
    }

    pub(super) async fn resume_pending_crew_completions(&self) {
        let completions = match self.daemon.pending_crew_completions().await {
            Ok(completions) => completions,
            Err(error) => {
                tracing::warn!(%error, "failed to load pending crew completions");
                return;
            }
        };
        for (session_name, pending, context) in completions {
            let convoy = context.convoy.clone().unwrap_or_else(|| "unknown-convoy".to_string());
            self.spawn_crew_completion_retry(PendingCrewCompletionRoute {
                namespace: context.namespace.clone().unwrap_or_else(|| "flotilla".to_string()),
                convoy,
                session_name,
                context,
                message: pending.message,
                disposition: pending.disposition,
                decision_ledger_ref: pending.decision_ledger_ref,
                force: pending.force,
                principal_ref: pending.principal_ref,
                authority: Some(HostName::new(pending.authority)),
            });
        }
    }

    async fn send_routed_to_convoy_home(
        &self,
        home: &flotilla_protocol::HostName,
        node_id: &NodeId,
        msg: RoutedPeerMessage,
    ) -> Result<(), String> {
        let (sender, address) = {
            let pm = self.peer_manager.lock().await;
            let address = pm.connection_address_for(node_id, home);
            let sender = pm.resolve_sender(node_id).map_err(|cause| format!("connect to {home} at {address}: {cause}"))?;
            (sender, address)
        };
        sender.send(PeerWireMessage::Routed(msg)).await.map_err(|cause| format!("connect to {home} at {address}: {cause}"))
    }

    async fn resolve_sender(&self, node_id: &NodeId) -> Result<Arc<dyn PeerSender>, String> {
        let pm = self.peer_manager.lock().await;
        pm.resolve_sender(node_id)
    }

    async fn send_routed_to(&self, node_id: &NodeId, msg: RoutedPeerMessage) -> Result<(), String> {
        let sender = self.resolve_sender(node_id).await?;
        sender.send(PeerWireMessage::Routed(msg)).await
    }

    async fn send_routed_pair_to(&self, node_id: &NodeId, first: RoutedPeerMessage, second: RoutedPeerMessage) -> Result<(), String> {
        let sender = self.resolve_sender(node_id).await?;
        sender.send(PeerWireMessage::Routed(first)).await?;
        sender.send(PeerWireMessage::Routed(second)).await
    }

    async fn execute_forwarded_remote_step_batch(
        &self,
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        request: RemoteStepBatchRequest,
        ready: Arc<Notify>,
    ) {
        let responder_node_id = self.daemon.node_id().clone();
        let cancel = CancellationToken::new();
        if let Some(entry) = self.forwarded_remote_step_batches.lock().await.get_mut(&request_id) {
            entry.state = ForwardedRemoteStepBatchState::Running { cancel: cancel.clone() };
        }
        ready.notify_waiters();

        let progress_sink = Arc::new(RoutedRemoteStepProgressSink::new(
            self.clone(),
            request_id,
            requester_node_id.clone(),
            reply_via.clone(),
            responder_node_id.clone(),
        ));

        let invalid_step = request
            .steps
            .iter()
            .enumerate()
            .find(|(_, step)| step.host.node_id() != &responder_node_id)
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
            requester_node_id,
            responder_node_id,
            remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
            outcomes: outcomes.unwrap_or_default(),
        };
        let _ = self.send_routed_to(&reply_via, response).await;
        self.forwarded_remote_step_batches.lock().await.remove(&request_id);
    }

    async fn cancel_forwarded_remote_step_batch(
        &self,
        cancel_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        remote_step_request_id: u64,
    ) {
        let responder_node_id = self.daemon.node_id().clone();
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
            requester_node_id,
            responder_node_id,
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
    requester_node_id: NodeId,
    reply_via: NodeId,
    responder_node_id: NodeId,
    state: Mutex<RoutedRemoteStepProgressState>,
}

impl RoutedRemoteStepProgressSink {
    fn new(router: RemoteCommandRouter, request_id: u64, requester_node_id: NodeId, reply_via: NodeId, responder_node_id: NodeId) -> Self {
        Self {
            router,
            request_id,
            requester_node_id,
            reply_via,
            responder_node_id,
            state: Mutex::new(RoutedRemoteStepProgressState::default()),
        }
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
                requester_node_id: self.requester_node_id.clone(),
                responder_node_id: self.responder_node_id.clone(),
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
