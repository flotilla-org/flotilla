//! Operator acceptance and fixture recorder for the installed first-party CLI.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    holder_transport::{AppServer, AppServerClient, RpcError},
    providers::ProcessCommandRunner,
};
use serde_json::{json, Value};

struct Recorder {
    inner: Arc<dyn AppServer>,
    interactions: Mutex<Vec<Value>>,
}
#[async_trait]
impl AppServer for Recorder {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let result = self.inner.request(method, params.clone()).await;
        self.interactions.lock().expect("recording lock").push(
            json!({"method":method,"params":params,"result":result.as_ref().ok(),"error":result.as_ref().err().map(RpcError::reason)}),
        );
        result
    }
}
#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().ok_or("usage: codex_transport_probe SOCKET OUTPUT [MODEL]")?;
    let output = PathBuf::from(args.next().ok_or("missing recording output")?);
    let model = args.next();
    let mode = args.next().unwrap_or_else(|| "delivery".into());
    let prompt = match mode.as_str() {
        "steering" => "Run sleep 3 using the shell tool, then describe trees in one sentence.",
        "approval" => "Use your shell tool to run mkdir /tmp/flotilla-approval-probe with escalated permissions. Explicitly request approval because this test uses a read-only sandbox.",
        "delivery" => "Reply with exactly OK. Do not run tools.",
        _ => return Err("mode must be delivery, steering, or approval".into()),
    };
    let rpc = Recorder { inner: AppServerClient::connect(&ProcessCommandRunner, &endpoint).await?, interactions: Mutex::new(Vec::new()) };
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?.to_string_lossy().into_owned();
    let start = rpc
        .request(
            "thread/start",
            json!({"cwd":cwd,"model":model,"approvalPolicy":if mode == "approval" { "on-request" } else { "never" },"sandbox":"read-only"}),
        )
        .await
        .map_err(|error| error.reason())?;
    let thread = start["thread"]["id"].as_str().ok_or("thread id missing")?;
    let started = rpc.request("turn/start", json!({"threadId":thread,"clientUserMessageId":"fixture-batch","input":[{"type":"text","text":format!("[flotilla batch: fixture-batch]\n{prompt}"),"text_elements":[]}]})).await.map_err(|error| error.reason())?;
    if mode == "steering" {
        rpc.request(
            "turn/steer",
            json!({"threadId":thread,"expectedTurnId":started["turn"]["id"],"clientUserMessageId":"fixture-steer",
            "input":[{"type":"text","text":"[flotilla batch: fixture-steer]\nStop counting and reply with STEERED.","text_elements":[]}]}),
        )
        .await
        .map_err(|error| error.reason())?;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    rpc.request("thread/resume", json!({"threadId":thread,"excludeTurns":false})).await.map_err(|error| error.reason())?;
    let mut accepted = false;
    for _ in 0..120 {
        let read = rpc.request("thread/read", json!({"threadId":thread,"includeTurns":true})).await.map_err(|error| error.reason())?;
        accepted |= read["thread"]["turns"].as_array().is_some_and(|turns| {
            turns.iter().any(|turn| {
                turn["items"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["type"] == "userMessage"
                            && (item["clientId"] == "fixture-batch"
                                || item["content"].as_array().is_some_and(|content| {
                                    content.iter().any(|part| {
                                        part["text"].as_str().is_some_and(|text| text.starts_with("[flotilla batch: fixture-batch]\n"))
                                    })
                                }))
                    })
                })
            })
        });
        let approval = rpc.inner.event_snapshot().iter().any(|event| event["method"] == "item/commandExecution/requestApproval");
        if read["thread"]["status"]["type"] == "idle" || (mode == "approval" && approval) {
            let interactions = rpc.interactions.lock().expect("recording lock");
            let encoded = serde_json::to_string_pretty(&json!({"interactions":*interactions,"events":rpc.inner.event_snapshot()}))
                .map_err(|error| error.to_string())?;
            // Mask environment paths, not the observed protocol layout or events.
            let encoded = encoded.replace(&cwd, "{cwd}").replace(thread, "{thread}");
            std::fs::write(output, encoded).map_err(|error| error.to_string())?;
            if mode == "approval" && !approval {
                return Err("expected structured approval request did not arrive".into());
            }
            if mode == "steering"
                && !read["thread"]["turns"].as_array().is_some_and(|turns| {
                    turns.iter().any(|turn| {
                        turn["items"].as_array().is_some_and(|items| items.iter().any(|item| item["clientId"] == "fixture-steer"))
                    })
                })
            {
                return Err("steering receipt missing".into());
            }
            if mode != "approval" && !accepted {
                return Err("clientUserMessageId was not preserved as a userMessage item id; recorded evidence for inspection".into());
            }
            println!("PASS: {mode}: first-party protocol and structured evidence");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = rpc.request("turn/interrupt", json!({"threadId":thread,"turnId":started["turn"]["id"]})).await;
    std::fs::write(
        "/tmp/codex-transport-failure.json",
        serde_json::to_string_pretty(
            &json!({"interactions":*rpc.interactions.lock().expect("interactions"),"events":rpc.inner.event_snapshot()}),
        )
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Err("turn did not complete within acceptance bound; diagnostic in /tmp/codex-transport-failure.json".into())
}
