//! Holder input selection and the first-party Codex app-server adapter.
//! Durable queueing and acknowledgements remain in MessageInbox. A protocol
//! response acknowledges a request; a correlated userMessage item proves input.
pub mod supervisor;

use std::{
    collections::BTreeMap,
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
    events: Arc<StdMutex<Vec<Value>>>,
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
        let stream =
            runner.open_stream("codex", &["app-server", "proxy", "--sock", endpoint], Path::new("/"), &ChannelLabel::Default).await?;
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), client_async("ws://localhost/", stream.io))
            .await
            .map_err(|_| "app-server handshake timed out")?
            .map_err(|error| error.to_string())?;
        let (calls, mut receiver) = mpsc::channel::<Call>(32);
        let events = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let task = tokio::spawn(async move {
            let _process = stream.process;
            let mut pending = BTreeMap::new();
            let mut sequence = 0u64;
            loop {
                tokio::select! {
                    call = receiver.recv() => {
                        let Some(call) = call else { break };
                        sequence += 1;
                        let mut message = json!({"method":call.method,"params":call.params});
                        if let Some(reply) = call.reply { message["id"] = json!(sequence); pending.insert(sequence, reply); }
                        if socket.send(WsMessage::Text(message.to_string().into())).await.is_err() { break; }
                    }
                    event = socket.next() => {
                        match event {
                            Some(Ok(WsMessage::Text(text))) => {
                                let Ok(value) = serde_json::from_str::<Value>(&text) else { break };
                                if value.get("method").is_some() {
                                    let mut events = observed.lock().expect("app-server events lock");
                                    if events.len() == 256 { events.remove(0); }
                                    events.push(value);
                                    continue;
                                }
                                if let Some(reply) = value["id"].as_u64().and_then(|id| pending.remove(&id)) {
                                    let result = if let Some(error) = value.get("error") { Err(RpcError::Rejected(error.to_string())) }
                                        else { Ok(value["result"].clone()) };
                                    let _ = reply.send(result);
                                }
                            }
                            Some(Ok(WsMessage::Ping(bytes))) => { if socket.send(WsMessage::Pong(bytes)).await.is_err() { break; } }
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
        client.request("initialize", json!({"clientInfo":{"name":"flotilla","title":"Flotilla","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await.map_err(|error| error.reason())?;
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
        self.events.lock().expect("app-server events lock").clone()
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
        rpc.request("thread/resume", json!({"threadId":thread,"excludeTurns":false})).await.map_err(|error| error.reason())?;
        Ok(Arc::new(Self { rpc, thread }))
    }
    async fn read(&self) -> Result<Value, String> {
        let response =
            self.rpc.request("thread/read", json!({"threadId":self.thread,"includeTurns":true})).await.map_err(|error| error.reason())?;
        if response["thread"]["id"].as_str() != Some(self.thread.as_str()) {
            return Err("app-server returned a different thread".into());
        }
        Ok(response["thread"].clone())
    }
    fn evidence(thread: &Value, id: &str) -> Option<String> {
        thread["turns"].as_array()?.iter().find_map(|turn| {
            turn["items"]
                .as_array()?
                .iter()
                .find(|item| {
                    item["type"] == "userMessage"
                        && (item["clientId"].as_str() == Some(id)
                            || item["content"].as_array().is_some_and(|content| {
                                content.iter().any(|part| {
                                    part["type"] == "text"
                                        && part["text"].as_str().is_some_and(|text| text.starts_with(&format!("[flotilla batch: {id}]\n")))
                                })
                            }))
                })
                .map(|_| format!("codex userMessage {id} in turn {}", turn["id"]))
        })
    }
    pub async fn wait_for_launch_receipt(&self, id: &str) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if Self::evidence(&self.read().await?, id).is_some() {
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
        let thread = self.read().await?;
        match thread["status"]["type"].as_str() {
            Some("idle") => Ok(Attention::Idle),
            Some("active")
                if thread["status"]["activeFlags"]
                    .as_array()
                    .is_some_and(|flags| flags.iter().any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput")) =>
            {
                Ok(Attention::NeedsInput)
            }
            Some("active") => Ok(Attention::Working),
            _ => Err("app-server thread is unavailable".into()),
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
        let thread = self.read().await?;
        let state = thread["status"]["type"].as_str();
        Ok(MessageObservation {
            ready: state == Some("idle"),
            working: state == Some("active"),
            evidence: submission.and_then(|submission| Self::evidence(&thread, &submission.batch_id)),
            waiting_reason: Some("waiting for structured Codex turn boundary".into()),
            ..Default::default()
        })
    }
    async fn submit(&self, batch: &MessageBatch) -> MessageTransportOutcome {
        let thread = match self.read().await {
            Ok(thread) => thread,
            Err(reason) => return MessageTransportOutcome::NotSubmitted { reason },
        };
        if let Some(evidence) = Self::evidence(&thread, &batch.id) {
            return MessageTransportOutcome::Accepted { evidence };
        }
        let active = thread["turns"].as_array().and_then(|turns| turns.iter().rev().find(|turn| turn["status"] == "inProgress"));
        let input = json!([{"type":"text","text":format!("[flotilla batch: {}]\n{}", batch.id, batch.text),"text_elements":[]}]);
        let (method, params) = if thread["status"]["type"] == "active" {
            if !batch.interrupting {
                return MessageTransportOutcome::NotSubmitted { reason: "Codex turn became active before delivery".into() };
            }
            let Some(turn) = active else {
                return MessageTransportOutcome::NotSubmitted { reason: "active Codex turn id unavailable".into() };
            };
            ("turn/steer", json!({"threadId":self.thread,"expectedTurnId":turn["id"],"clientUserMessageId":batch.id,"input":input}))
        } else if thread["status"]["type"] == "idle" {
            ("turn/start", json!({"threadId":self.thread,"clientUserMessageId":batch.id,"input":input}))
        } else {
            return MessageTransportOutcome::NotSubmitted { reason: "Codex thread is not ready".into() };
        };
        match self.rpc.request(method, params).await {
            Ok(_) => MessageTransportOutcome::Pending,
            Err(RpcError::Rejected(reason)) => MessageTransportOutcome::NotSubmitted { reason },
            Err(RpcError::Unavailable(reason)) => MessageTransportOutcome::Unconfirmed { reason },
        }
    }
    async fn poll(&self, batch: &MessageBatch) -> MessageTransportOutcome {
        match self.read().await {
            Ok(thread) => Self::evidence(&thread, &batch.id)
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
    ApprovalRequested { request: Value, method: String, item: String },
    ApprovalResolved { request: Value },
}
impl CodexTransport {
    pub fn events(&self) -> Vec<HolderEvent> {
        self.rpc
            .events()
            .into_iter()
            .filter_map(|event| {
                let params = &event["params"];
                if params["threadId"].as_str() != Some(self.thread.as_str()) {
                    return None;
                }
                match event["method"].as_str()? {
                    "turn/started" => Some(HolderEvent::TurnStarted { turn: params["turn"]["id"].as_str()?.into() }),
                    "turn/completed" => Some(HolderEvent::TurnCompleted {
                        turn: params["turn"]["id"].as_str()?.into(),
                        status: params["turn"]["status"].as_str()?.into(),
                    }),
                    method @ ("item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
                    | "item/permissions/requestApproval") => Some(HolderEvent::ApprovalRequested {
                        request: event["id"].clone(),
                        method: method.into(),
                        item: params["itemId"].as_str()?.into(),
                    }),
                    "serverRequest/resolved" => Some(HolderEvent::ApprovalResolved { request: params["requestId"].clone() }),
                    _ => None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
