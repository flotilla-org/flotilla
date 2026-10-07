use std::sync::Mutex;

use chrono::Utc;
use flotilla_resources::{
    CrewSessionStatus, InMemoryBackend, InputMeta, ResourceBackend, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
};
use hegel::generators as gs;

use super::*;

async fn holder(methods: Vec<HolderTransport>) -> ResourceObject<TerminalSession> {
    let terminals = ResourceBackend::InMemory(InMemoryBackend::default()).using::<TerminalSession>("flotilla");
    let object = terminals
        .create(
            &InputMeta::builder().name("holder".into()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("environment".into())
                .role("coder".into())
                .source(TerminalSessionSource::Tool { command: "test".into() })
                .cwd("/work".into())
                .pool("cleat".into())
                .build(),
        )
        .await
        .expect("holder");
    terminals
        .update_status(
            "holder",
            &object.metadata.resource_version,
            &TerminalSessionStatus {
                crew: Some(
                    CrewSessionStatus::builder()
                        .id("crew".into())
                        .adapter("codex".into())
                        .stance("work".into())
                        .input_transports(methods)
                        .build(),
                ),
                session_id: Some("terminal".into()),
                ..Default::default()
            },
        )
        .await
        .expect("crew")
}
async fn batch(interrupting: bool) -> MessageBatch {
    let submission = MessageSubmission::builder()
        .batch_id("batch".into())
        .crew_id("crew".into())
        .session("terminal".into())
        .started_at(Utc::now())
        .build();
    MessageBatch::builder()
        .id("batch".into())
        .holder(holder(Vec::new()).await)
        .submission(submission)
        .text("message".into())
        .interrupting(interrupting)
        .build()
}

// Process/protocol fake: validates the documented input envelope instead of
// accepting arbitrary requests. Storage and batching use real implementations.
struct Peer {
    thread: Mutex<Value>,
    calls: Mutex<Vec<String>>,
    error: Option<RpcError>,
    events: Mutex<Vec<Value>>,
}
impl Peer {
    fn new(active: bool, error: Option<RpcError>) -> Arc<Self> {
        Arc::new(Self {
            thread: Mutex::new(json!({"id":"thread","status":{"type":if active {"active"} else {"idle"},"activeFlags":[]},
            "turns":[{"id":"turn","status":if active {"inProgress"} else {"completed"},"items":[]}]})),
            calls: Mutex::new(Vec::new()),
            error,
            events: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl AppServer for Peer {
    fn events(&self) -> Vec<Value> {
        std::mem::take(&mut *self.events.lock().expect("events"))
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.calls.lock().expect("calls").push(method.into());
        assert_eq!(params["threadId"], "thread");
        match method {
            "thread/resume" => {
                assert_eq!(params["excludeTurns"], false);
                Ok(json!({"thread":*self.thread.lock().expect("thread")}))
            }
            "thread/read" => {
                assert_eq!(params["includeTurns"], true);
                Ok(json!({"thread":*self.thread.lock().expect("thread")}))
            }
            "turn/start" | "turn/steer" => {
                assert_eq!(params["clientUserMessageId"], "batch");
                assert_eq!(params["input"][0]["type"], "text");
                assert!(params["input"][0]["text"].as_str().expect("text").starts_with("[flotilla batch: batch]\n"));
                if method == "turn/steer" {
                    assert_eq!(params["expectedTurnId"], "turn");
                }
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                if method == "turn/steer" {
                    Ok(json!({"turnId":"turn"}))
                } else {
                    Ok(json!({"turn":{"id":"turn","status":"inProgress","items":[]}}))
                }
            }
            _ => panic!("unexpected method {method}"),
        }
    }
}

// Holder declarations select the first supported method, regardless of the
// harness named on the crew. Empty old declarations retain guarded screen.
#[hegel::test]
fn selection_belongs_to_the_holder(tc: hegel::TestCase) {
    // Both prime harness declarations, unsupported hooks, explicit screen,
    // absent declarations, and unsupported-only declarations are generated.
    let kind = tc.draw(gs::integers::<u8>().min_value(0).max_value(4));
    let methods = match kind {
        0 => Vec::new(),
        1 => vec![
            HolderTransport::AgentApi { adapter: "codex".into(), endpoint: "socket".into(), session: "thread".into() },
            HolderTransport::Screen,
        ],
        2 => vec![
            HolderTransport::AgentApi { adapter: "claude-code".into(), endpoint: "channel".into(), session: "session".into() },
            HolderTransport::Screen,
        ],
        3 => vec![HolderTransport::Hook { name: "unknown".into() }, HolderTransport::Screen],
        _ => vec![HolderTransport::Hook { name: "unknown".into() }],
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    rt.block_on(async {
        let holder = holder(methods).await;
        let selected = select_transport(&holder, &[TransportCapability::AgentApi("codex"), TransportCapability::AgentApi("claude-code")]);
        match kind {
            0 => assert_eq!(selected.expect("old holder"), None),
            1 | 2 => assert!(matches!(selected.expect("native"), Some(HolderTransport::AgentApi { .. }))),
            3 => assert_eq!(selected.expect("fallback"), Some(&HolderTransport::Screen)),
            _ => assert!(selected.is_err()),
        }
    });
}

// Ordinary input waits during an active turn. Interrupting supervisor batches
// steer the known active turn; RPC replies alone never acknowledge delivery.
#[hegel::test]
fn only_urgent_input_steers_and_only_items_acknowledge(tc: hegel::TestCase) {
    // Cover idle/active, ordinary/urgent, accepted/rejected/connection-lost,
    // duplicate polls, unrelated user input, and late correlated receipts.
    let active = tc.draw(gs::booleans());
    let urgent = tc.draw(gs::booleans());
    let failure = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
    let error = match failure {
        1 => Some(RpcError::Rejected("rejected".into())),
        2 => Some(RpcError::Unavailable("lost".into())),
        _ => None,
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    rt.block_on(async {
        let peer = Peer::new(active, error);
        let transport = CodexTransport::resume(peer.clone(), "thread".into()).await.expect("resume");
        let batch = batch(urgent).await;
        let outcome = transport.submit(&batch).await;
        if active && !urgent {
            assert!(matches!(outcome, MessageTransportOutcome::NotSubmitted { .. }));
            assert!(!peer.calls.lock().expect("calls").iter().any(|method| method.starts_with("turn/")));
        } else {
            assert!(peer.calls.lock().expect("calls").iter().any(|method| method == if active { "turn/steer" } else { "turn/start" }));
            match failure {
                0 => assert_eq!(outcome, MessageTransportOutcome::Pending),
                1 => assert!(matches!(outcome, MessageTransportOutcome::NotSubmitted { .. })),
                _ => assert!(matches!(outcome, MessageTransportOutcome::Unconfirmed { .. })),
            }
        }
        peer.thread.lock().expect("thread")["turns"][0]["items"] = json!([{"type":"userMessage","clientId":"unrelated","id":"different"}]);
        for _ in 0..3 {
            assert_eq!(transport.poll(&batch).await, MessageTransportOutcome::Pending);
        }
        let before = peer.calls.lock().expect("calls").iter().filter(|method| method.starts_with("turn/")).count();
        peer.thread.lock().expect("thread")["turns"][0]["items"] =
            json!([{"type":"userMessage","clientId":"batch","id":"server-generated"}]);
        assert!(matches!(transport.poll(&batch).await, MessageTransportOutcome::Accepted { .. }));
        assert_eq!(peer.calls.lock().expect("calls").iter().filter(|method| method.starts_with("turn/")).count(), before);
    });
}

// The actual 0.160 recording proves native item correlation, completed-turn
// events, and the resumed history shape. It is generated by the operator probe.
#[tokio::test]
async fn recorded_first_party_receipts_and_turn_events() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/codex_0_160_delivery.json")).expect("recorded fixture");
    let reads = fixture["interactions"]
        .as_array()
        .expect("interactions")
        .iter()
        .filter(|entry| entry["method"] == "thread/read")
        .collect::<Vec<_>>();
    assert!(!reads.is_empty());
    let mut receipt = false;
    for read in reads {
        receipt |= CodexTransport::evidence(&read["result"]["thread"], "fixture-batch").is_some();
        assert!(CodexTransport::evidence(&read["result"]["thread"], "unrelated-batch").is_none());
    }
    assert!(receipt);
    let peer = Arc::new(Peer {
        thread: Mutex::new(json!({})),
        calls: Mutex::new(Vec::new()),
        error: None,
        events: Mutex::new(fixture["events"].as_array().expect("events").clone()),
    });
    let transport = CodexTransport { rpc: peer, thread: "{thread}".into() };
    let events = transport.events();
    assert!(events.iter().any(|event| matches!(event, HolderEvent::TurnStarted { .. })));
    assert!(events.iter().any(|event| matches!(event, HolderEvent::TurnCompleted { status, .. } if status == "completed")));
    assert!(transport.events().is_empty(), "turn events are consumed once");
}

// Approval requests and resolution are structured observations. No approval
// decision is issued by observing attention, leaving policy to the client.
#[tokio::test]
async fn structured_approvals_require_input_without_auto_approval() {
    let peer = Arc::new(Peer {
        thread: Mutex::new(json!({"id":"thread","status":{"type":"active","activeFlags":["waitingOnApproval"]},"turns":[]})),
        calls: Mutex::new(Vec::new()),
        error: None,
        events: Mutex::new(vec![
            json!({"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","turnId":"turn","itemId":"command"}}),
            json!({"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":42}}),
        ]),
    });
    let transport = CodexTransport::resume(peer.clone(), "thread".into()).await.expect("resume");
    assert_eq!(transport.attention().await.expect("attention"), flotilla_resources::TerminalAttentionState::NeedsInput);
    assert_eq!(
        transport.events(),
        vec![
            HolderEvent::ApprovalRequested {
                request: json!(42),
                method: "item/commandExecution/requestApproval".into(),
                item: "command".into()
            },
            HolderEvent::ApprovalResolved { request: json!(42) }
        ]
    );
    assert!(peer.calls.lock().expect("calls").iter().all(|method| method.starts_with("thread/")));
}

// A protocol subprocess may keep its socket open after daemon death. The
// request heartbeat deadline cancels the proxy and makes reconnect possible.
#[tokio::test(start_paused = true)]
async fn heartbeat_timeout_cancels_the_protocol_process_without_reissuing_input() {
    use std::{
        path::Path,
        sync::atomic::{AtomicBool, Ordering},
    };

    use crate::providers::{CommandOutput, CommandProcess, CommandStream};
    struct Process(Arc<AtomicBool>);
    impl Drop for Process {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl CommandProcess for Process {
        fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
            Ok(None)
        }
        async fn kill(&mut self) -> Result<(), String> {
            Ok(())
        }
        async fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
            Err("fake process".into())
        }
    }
    // Process seam only. The WebSocket and JSON-RPC code are real.
    struct Runner {
        stopped: Arc<AtomicBool>,
    }
    #[async_trait]
    impl CommandRunner for Runner {
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            panic!("unexpected command")
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            panic!("unexpected command")
        }
        async fn open_stream(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandStream, String> {
            assert_eq!(cmd, "codex");
            assert_eq!(args, ["app-server", "proxy", "--sock", "/private/socket"]);
            let (client, server) = tokio::io::duplex(8192);
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(server).await.expect("handshake");
                let request = ws.next().await.expect("initialize").expect("frame");
                let request: Value = serde_json::from_str(request.to_text().expect("text")).expect("json");
                assert_eq!(request["method"], "initialize");
                assert_eq!(request["params"]["clientInfo"]["name"], "flotilla");
                ws.send(WsMessage::Text(json!({"id":request["id"],"result":{}}).to_string().into())).await.expect("initialize response");
                let initialized = ws.next().await.expect("initialized").expect("frame");
                let initialized: Value = serde_json::from_str(initialized.to_text().expect("text")).expect("json");
                assert_eq!(initialized["method"], "initialized");
                assert!(initialized.get("id").is_none());
                let request = ws.next().await.expect("heartbeat").expect("frame");
                let request: Value = serde_json::from_str(request.to_text().expect("text")).expect("json");
                assert_eq!(request["method"], "thread/read");
                // A server request can use the same id as our heartbeat. It must
                // remain an approval event, not satisfy the pending heartbeat.
                ws.send(WsMessage::Text(json!({"id":request["id"],"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","itemId":"command"}}).to_string().into())).await.expect("approval event");
                std::future::pending::<()>().await;
            });
            Ok(CommandStream { io: Box::new(client), process: Box::new(Process(Arc::clone(&self.stopped))) })
        }
    }
    let stopped = Arc::new(AtomicBool::new(false));
    let client = AppServerClient::connect(&Runner { stopped: Arc::clone(&stopped) }, "/private/socket").await.expect("initialize");
    assert!(matches!(client.request("thread/read", json!({"threadId":"thread","includeTurns":true})).await, Err(RpcError::Unavailable(_))));
    tokio::task::yield_now().await;
    assert!(stopped.load(Ordering::SeqCst));
    assert!(!client.available());
    assert_eq!(client.event_snapshot().len(), 1);
    assert_eq!(client.events().len(), 1);
    assert!(client.events().is_empty(), "approval event is delivered only once");
    assert!(client.events().is_empty());
}

// Recorded steering must retain the active turn and produce a distinct native
// userMessage receipt. Recorded approvals must surface without a client reply.
#[test]
fn recorded_steering_and_approval_contracts() {
    let steering: Value = serde_json::from_str(include_str!("fixtures/codex_0_160_steering.json")).expect("steering recording");
    let entries = steering["interactions"].as_array().expect("interactions");
    let start = entries.iter().find(|entry| entry["method"] == "turn/start").expect("start");
    let steer = entries.iter().find(|entry| entry["method"] == "turn/steer").expect("steer");
    assert_eq!(steer["params"]["expectedTurnId"], start["result"]["turn"]["id"]);
    assert!(steer["error"].is_null());
    assert_eq!(steer["result"]["turnId"], start["result"]["turn"]["id"]);
    assert!(entries.iter().filter(|entry| entry["method"] == "thread/read").any(|entry| CodexTransport::evidence(
        &entry["result"]["thread"],
        "fixture-steer"
    )
    .is_some()));
    let approval: Value = serde_json::from_str(include_str!("fixtures/codex_0_160_approval.json")).expect("approval recording");
    let peer = Arc::new(Peer {
        thread: Mutex::new(json!({})),
        calls: Mutex::new(Vec::new()),
        error: None,
        events: Mutex::new(approval["events"].as_array().expect("events").clone()),
    });
    let transport = CodexTransport { rpc: peer, thread: "{thread}".into() };
    assert!(transport.events().iter().any(|event| matches!(event, HolderEvent::ApprovalRequested { .. })));
    assert!(approval["interactions"]
        .as_array()
        .expect("interactions")
        .iter()
        .all(|entry| !entry["method"].as_str().expect("method").contains("Approval")));
}

// Readiness can change between an inbox observation and the transport call.
// Rechecking an active thread prevents ordinary messages from becoming steers.
#[tokio::test]
async fn ordinary_input_rechecks_the_boundary_before_submission() {
    let peer = Peer::new(false, None);
    let transport = CodexTransport::resume(peer.clone(), "thread".into()).await.expect("resume");
    let batch = batch(false).await;
    assert!(transport.observe(&batch.holder, None).await.expect("idle").ready);
    peer.thread.lock().expect("thread")["status"]["type"] = json!("active");
    assert!(matches!(transport.submit(&batch).await, MessageTransportOutcome::NotSubmitted { .. }));
    assert!(!peer.calls.lock().expect("calls").iter().any(|method| method.starts_with("turn/")));
}

// The holder is not published as launched until its inline brief has a native
// receipt. A missing receipt times out without submitting the brief again.
#[tokio::test(start_paused = true)]
async fn launch_receipt_is_bounded_and_never_resubmitted() {
    let peer = Peer::new(true, None);
    let transport = CodexTransport::started(peer.clone(), "thread".into());
    assert!(transport.wait_for_launch_receipt("launch").await.is_err());
    assert!(peer.calls.lock().expect("calls").iter().all(|method| method == "thread/read"));
    peer.thread.lock().expect("thread")["turns"][0]["items"] = json!([{"type":"userMessage","clientId":"launch","id":"generated"}]);
    transport.wait_for_launch_receipt("launch").await.expect("correlated launch receipt");
}

// A turn may start in another client after the idle read. An explicit native
// rejection is unsent, whereas losing the reply remains held for receipt polling.
#[tokio::test]
async fn turn_start_race_rejection_is_unsent_and_lost_reply_is_ambiguous() {
    struct RacingPeer {
        inner: Arc<Peer>,
        lost: bool,
    }
    #[async_trait]
    impl AppServer for RacingPeer {
        async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            if method == "turn/start" {
                // Independent client starts a turn between read and start.
                let mut thread = self.inner.thread.lock().expect("thread");
                thread["status"]["type"] = json!("active");
                thread["turns"][0]["status"] = json!("inProgress");
                return Err(if self.lost {
                    RpcError::Unavailable("reply lost after concurrent start".into())
                } else {
                    RpcError::Rejected("thread already active".into())
                });
            }
            self.inner.request(method, params).await
        }
    }
    for lost in [false, true] {
        let rpc = Arc::new(RacingPeer { inner: Peer::new(false, None), lost });
        let transport = CodexTransport::started(rpc.clone(), "thread".into());
        let batch = batch(false).await;
        let result = transport.submit(&batch).await;
        if lost {
            assert!(matches!(result, MessageTransportOutcome::Unconfirmed { .. }));
        } else {
            assert!(matches!(result, MessageTransportOutcome::NotSubmitted { .. }));
        }
        assert_eq!(rpc.inner.thread.lock().expect("thread")["status"]["type"], "active");
        assert!(matches!(transport.poll(&batch).await, MessageTransportOutcome::Pending));
        assert_eq!(rpc.inner.calls.lock().expect("calls").as_slice(), ["thread/read", "thread/read"]);
    }
}
