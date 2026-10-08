//! Holder input selection and the first-party Codex app-server adapter.
//! Durable queueing and acknowledgements remain in MessageInbox. A protocol
//! response acknowledges a request; a correlated userMessage item proves input.
pub mod managed;
mod protocol;
pub use protocol::RequestId;
use protocol::{Thread, ThreadResponse, ThreadStatus};

use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_resources::{
    HolderTransport, MessageBatch, MessageObservation, MessageSubmission, MessageTransport, MessageTransportOutcome, ResourceObject,
    TerminalSession,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{client_async, tungstenite::Message as WsMessage};

use crate::providers::{ChannelLabel, CommandRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCapability<'a> {
    AgentApi(&'a str),
    Hook(&'a str),
}

/// Empty declarations decode previous-generation holders as guarded screen.
/// Unsupported methods can be skipped, but an unavailable selected method
/// cannot silently fall through after input may have crossed the seam.
pub fn select_transport<'a>(
    holder: &'a ResourceObject<TerminalSession>,
    supported: &[TransportCapability<'_>],
) -> Result<Option<&'a HolderTransport>, String> {
    let methods =
        holder.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.input_transports.as_slice()).unwrap_or_default();
    if methods.is_empty() {
        return Ok(None);
    }
    methods
        .iter()
        .find(|method| match method {
            HolderTransport::AgentApi { adapter, .. } => supported.contains(&TransportCapability::AgentApi(adapter.as_str())),
            HolderTransport::Hook { name } => supported.contains(&TransportCapability::Hook(name.as_str())),
            HolderTransport::Screen => true,
        })
        .map(Some)
        .ok_or_else(|| "holder declares no supported input transport".into())
}

#[derive(Debug, Clone)]
pub enum RpcError {
    Rejected(String),
    Unavailable(String),
}
impl RpcError {
    pub fn reason(&self) -> String {
        match self {
            Self::Rejected(reason) | Self::Unavailable(reason) => reason.clone(),
        }
    }
}

/// The protocol-process seam: tests inject a schema-enforcing app-server peer.
#[async_trait]
pub trait AppServer: Send + Sync {
    fn available(&self) -> bool {
        true
    }
    /// Drain newly received events once; lifecycle consumers must not replay them.
    fn events(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

struct Call {
    method: String,
    params: Value,
    reply: Option<oneshot::Sender<Result<Value, RpcError>>>,
}

/// One subscribed protocol client. Dropping it cancels its proxy. Explicit
/// request deadlines also detect daemon death without relying on socket EOF.
pub struct AppServerClient {
    events: Arc<StdMutex<VecDeque<Value>>>,
    calls: mpsc::Sender<Call>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for AppServerClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AppServerClient {
    pub async fn connect(runner: &dyn CommandRunner, endpoint: &str) -> Result<Arc<Self>, String> {
        Self::connect_with_binary(runner, "codex", endpoint).await
    }
    pub async fn connect_with_binary(runner: &dyn CommandRunner, binary: &str, endpoint: &str) -> Result<Arc<Self>, String> {
        let stream =
            runner.open_stream(binary, &["app-server", "proxy", "--sock", endpoint], Path::new("/"), &ChannelLabel::Default).await?;
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), client_async("ws://localhost/", stream.io))
            .await
            .map_err(|_| "app-server handshake timed out")?
            .map_err(|error| error.to_string())?;
        let (calls, mut receiver) = mpsc::channel::<Call>(32);
        let events = Arc::new(StdMutex::new(VecDeque::new()));
        let observed = Arc::clone(&events);
        let task = tokio::spawn(async move {
            let _process = stream.process;
            let mut pending = BTreeMap::new();
            let mut sequence = 0i64;
            loop {
                tokio::select! {
                    call = receiver.recv() => {
                        let Some(call) = call else { break };
                        sequence += 1;
                        let id = call.reply.map(|reply| { pending.insert(sequence, reply); sequence });
                        let message = protocol::Outgoing { id, method: &call.method, params: &call.params };
                        let Ok(encoded) = serde_json::to_string(&message) else { break };
                        if socket.send(WsMessage::Text(encoded.into())).await.is_err() { break; }
                    }
                    event = socket.next() => {
                        match event {
                            Some(Ok(WsMessage::Text(text))) => {
                                let message = match serde_json::from_str::<protocol::Incoming>(&text) {
                                    Ok(message) => message,
                                    Err(error) => {
                                        tracing::warn!(%error, "invalid Codex RPC envelope");
                                        break;
                                    }
                                };
                                match message {
                                    protocol::Incoming::Event { method, id, params } => {
                                        let mut events = observed.lock().expect("app-server events lock");
                                        if events.len() == 256 { events.pop_front(); }
                                        events.push_back(json!({"method": method, "id": id, "params": params}));
                                    }
                                    protocol::Incoming::Success { id: protocol::RequestId::Number(id), result } => {
                                        if let Some(reply) = pending.remove(&id) { let _ = reply.send(Ok(result)); }
                                    }
                                    protocol::Incoming::Failure { id: protocol::RequestId::Number(id), error } => {
                                        if let Some(reply) = pending.remove(&id) {
                                            let _ = reply.send(Err(RpcError::Rejected(format!("{}: {} ({:?})", error.code, error.message, error.data))));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(WsMessage::Ping(bytes))) => {
                                if socket.send(WsMessage::Pong(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                            _ => {}
                        }
                    }
                }
            }
            for (_, reply) in pending {
                let _ = reply.send(Err(RpcError::Unavailable("app-server disconnected".into())));
            }
        });
        let client = Arc::new(Self { events, calls, task });
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "flotilla",
                        "title": "Flotilla",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await
            .map_err(|error| error.reason())?;
        client
            .calls
            .send(Call { method: "initialized".into(), params: json!({}), reply: None })
            .await
            .map_err(|_| "app-server initialization closed")?;
        Ok(client)
    }
}
#[async_trait]
impl AppServer for AppServerClient {
    fn available(&self) -> bool {
        !self.calls.is_closed()
    }
    fn events(&self) -> Vec<Value> {
        self.events.lock().expect("app-server events lock").drain(..).collect()
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (reply, receive) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(10), async {
            self.calls
                .send(Call { method: method.into(), params, reply: Some(reply) })
                .await
                .map_err(|_| RpcError::Unavailable("app-server client stopped".into()))?;
            receive.await.map_err(|_| RpcError::Unavailable("app-server reply lost".into()))?
        })
        .await
        .unwrap_or_else(|_| {
            // Every request is also a heartbeat. A missed deadline deliberately
            // closes this client and all pending calls: none can be trusted as
            // unsent, and reconnect must poll receipts rather than replay input.
            self.task.abort();
            Err(RpcError::Unavailable("app-server heartbeat/request deadline exceeded".into()))
        })
    }
}

pub struct CodexTransport {
    rpc: Arc<dyn AppServer>,
    thread: String,
}
impl CodexTransport {
    /// thread/start already subscribes this client; resume would require a
    /// rollout that may not yet exist before the first input is materialized.
    pub fn started(rpc: Arc<dyn AppServer>, thread: String) -> Arc<Self> {
        Arc::new(Self { rpc, thread })
    }
    pub fn available(&self) -> bool {
        self.rpc.available()
    }
    pub async fn resume(rpc: Arc<dyn AppServer>, thread: String) -> Result<Arc<Self>, String> {
        rpc.request(
            "thread/resume",
            serde_json::to_value(protocol::ThreadResume { thread_id: &thread, exclude_turns: false }).map_err(|error| error.to_string())?,
        )
        .await
        .map_err(|error| error.reason())?;
        Ok(Arc::new(Self { rpc, thread }))
    }
    async fn read(&self, include_turns: bool) -> Result<Thread, String> {
        let params =
            serde_json::to_value(protocol::ThreadRead { thread_id: &self.thread, include_turns }).map_err(|error| error.to_string())?;
        let response = self.rpc.request("thread/read", params).await.map_err(|error| error.reason())?;
        let response: ThreadResponse =
            serde_json::from_value(response).map_err(|error| format!("invalid Codex thread response: {error}"))?;
        if response.thread.id != self.thread {
            return Err("app-server returned a different thread".into());
        }
        Ok(response.thread)
    }
    fn receipt(thread: &Thread, id: &str) -> Option<String> {
        let marker = format!("[flotilla batch: {id}]\n");
        thread.turns.iter().find_map(|turn| {
            turn.items
                .iter()
                .any(|item| match item {
                    protocol::Item::UserMessage { client_id, content } => {
                        client_id.as_deref() == Some(id)
                            || content.iter().any(|part| matches!(part, protocol::Content::Text { text, .. } if text.starts_with(&marker)))
                    }
                    protocol::Item::Other => false,
                })
                .then(|| format!("codex userMessage {id} in turn {}", turn.id))
        })
    }
    pub async fn wait_for_launch_receipt(&self, id: &str) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if Self::receipt(&self.read(true).await?, id).is_some() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "Codex launch input receipt timed out".to_string())?
    }
    pub async fn attention(&self) -> Result<flotilla_resources::TerminalAttentionState, String> {
        use flotilla_resources::TerminalAttentionState as Attention;
        let thread = self.read(false).await?;
        match thread.status {
            ThreadStatus::Idle => Ok(Attention::Idle),
            ThreadStatus::Active { flags } if flags.iter().any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput") => {
                Ok(Attention::NeedsInput)
            }
            ThreadStatus::Active { .. } => Ok(Attention::Working),
            ThreadStatus::Unavailable => Err("app-server thread is unavailable".into()),
        }
    }
}
#[async_trait]
impl MessageTransport for CodexTransport {
    fn supports_steering(&self, _: &ResourceObject<TerminalSession>) -> bool {
        true
    }
    fn uses_structured_evidence(&self, _: &ResourceObject<TerminalSession>) -> bool {
        true
    }
    async fn observe(
        &self,
        _: &ResourceObject<TerminalSession>,
        submission: Option<&MessageSubmission>,
    ) -> Result<MessageObservation, String> {
        let thread = self.read(submission.is_some()).await?;
        let state = &thread.status;
        Ok(MessageObservation {
            ready: matches!(state, ThreadStatus::Idle),
            working: matches!(state, ThreadStatus::Active { .. }),
            evidence: submission.and_then(|submission| Self::receipt(&thread, &submission.batch_id)),
            waiting_reason: Some("waiting for structured Codex turn boundary".into()),
            ..Default::default()
        })
    }
    async fn submit(&self, batch: &MessageBatch) -> MessageTransportOutcome {
        let thread = match self.read(true).await {
            Ok(thread) => thread,
            Err(reason) => return MessageTransportOutcome::NotSubmitted { reason },
        };
        if let Some(evidence) = Self::receipt(&thread, &batch.id) {
            return MessageTransportOutcome::Accepted { evidence };
        }
        let active = thread.turns.iter().rev().find(|turn| turn.status == "inProgress");
        let expected_turn_id = match thread.status {
            ThreadStatus::Active { .. } => {
                if !batch.interrupting {
                    return MessageTransportOutcome::NotSubmitted { reason: "Codex turn became active before delivery".into() };
                }
                let Some(turn) = active else {
                    return MessageTransportOutcome::NotSubmitted { reason: "active Codex turn id unavailable".into() };
                };
                Some(turn.id.as_str())
            }
            ThreadStatus::Idle => None,
            ThreadStatus::Unavailable => return MessageTransportOutcome::NotSubmitted { reason: "Codex thread is not ready".into() },
        };
        let method = if expected_turn_id.is_some() { "turn/steer" } else { "turn/start" };
        let params = serde_json::to_value(protocol::Input {
            thread_id: &self.thread,
            client_user_message_id: &batch.id,
            expected_turn_id,
            input: vec![protocol::Content::Text {
                text: format!("[flotilla batch: {}]\n{}", batch.id, batch.text),
                text_elements: Vec::new(),
            }],
        })
        .expect("serializable Codex input");
        match self.rpc.request(method, params).await {
            Ok(_) => MessageTransportOutcome::Pending,
            Err(RpcError::Rejected(reason)) => MessageTransportOutcome::NotSubmitted { reason },
            Err(RpcError::Unavailable(reason)) => MessageTransportOutcome::Unconfirmed { reason },
        }
    }
    async fn poll(&self, batch: &MessageBatch) -> MessageTransportOutcome {
        match self.read(true).await {
            Ok(thread) => Self::receipt(&thread, &batch.id)
                .map(|evidence| MessageTransportOutcome::Accepted { evidence })
                .unwrap_or(MessageTransportOutcome::Pending),
            Err(reason) => MessageTransportOutcome::Unconfirmed { reason },
        }
    }
}

/// First-party events shared with consumers without leaking harness payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderEvent {
    TurnStarted { turn: String },
    TurnCompleted { turn: String, status: String },
    ApprovalRequested { request: RequestId, method: String, item: String },
    ApprovalResolved { request: RequestId },
}
impl CodexTransport {
    pub fn events(&self) -> Vec<HolderEvent> {
        self.rpc
            .events()
            .into_iter()
            .filter_map(|event| {
                let notification = serde_json::from_value::<protocol::Notification>(event).ok()?;
                let request = notification.id;
                match notification.event {
                    protocol::Event::TurnStarted(event) if event.thread_id == self.thread => {
                        Some(HolderEvent::TurnStarted { turn: event.turn.id })
                    }
                    protocol::Event::TurnCompleted(event) if event.thread_id == self.thread => {
                        Some(HolderEvent::TurnCompleted { turn: event.turn.id, status: event.turn.status })
                    }
                    protocol::Event::CommandApproval(event) if event.thread_id == self.thread => Some(HolderEvent::ApprovalRequested {
                        request: request?,
                        method: "item/commandExecution/requestApproval".into(),
                        item: event.item_id,
                    }),
                    protocol::Event::FileApproval(event) if event.thread_id == self.thread => Some(HolderEvent::ApprovalRequested {
                        request: request?,
                        method: "item/fileChange/requestApproval".into(),
                        item: event.item_id,
                    }),
                    protocol::Event::PermissionsApproval(event) if event.thread_id == self.thread => Some(HolderEvent::ApprovalRequested {
                        request: request?,
                        method: "item/permissions/requestApproval".into(),
                        item: event.item_id,
                    }),
                    protocol::Event::Resolved(event) if event.thread_id == self.thread => {
                        Some(HolderEvent::ApprovalResolved { request: event.request_id })
                    }
                    _ => None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
