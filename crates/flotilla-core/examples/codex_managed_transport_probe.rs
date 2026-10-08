//! Live operator probe of the Rust adapter; no recorded model output.
use chrono::Utc;
use flotilla_core::{
    agent_adapter::{AgentLaunchRequest, AgentSession, CrewIdentity},
    holder_transport::{managed, AppServer, AppServerClient, CodexTransport, HolderEvent},
    path_context::ExecutionEnvironmentPath,
    providers::ProcessCommandRunner,
};
use flotilla_resources::{
    CrewSessionStatus, HolderTransport, InMemoryBackend, InputMeta, MessageBatch, MessageSubmission, MessageTransportOutcome,
    ResourceBackend, ResourceObject, TerminalAttentionState, TerminalBrief, TerminalSession, TerminalSessionSource, TerminalSessionSpec,
    TerminalSessionStatus,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

async fn idle(session: &dyn AgentSession) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if session.attention().await? == TerminalAttentionState::Idle {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "model turn did not finish".to_string())?
}

async fn holder(binding: HolderTransport) -> Result<ResourceObject<TerminalSession>, String> {
    let terminals = ResourceBackend::InMemory(InMemoryBackend::default()).using::<TerminalSession>("operator-probe");
    let object = terminals
        .create(
            &InputMeta::builder().name("probe".into()).build(),
            &TerminalSessionSpec::builder()
                .env_ref("probe".into())
                .role("reviewer".into())
                .source(TerminalSessionSource::Tool { command: "probe".into() })
                .cwd("/".into())
                .pool("cleat".into())
                .build(),
        )
        .await
        .map_err(|error| error.to_string())?;
    terminals
        .update_status(
            "probe",
            &object.metadata.resource_version,
            &TerminalSessionStatus {
                crew: Some(
                    CrewSessionStatus::builder()
                        .id("probe-reviewer".into())
                        .adapter("codex".into())
                        .stance("work".into())
                        .input_transports(vec![binding])
                        .build(),
                ),
                session_id: Some("probe-reviewer".into()),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.to_string())
}
fn batch(holder: &ResourceObject<TerminalSession>, id: &str, text: String, interrupting: bool) -> MessageBatch {
    let submission = MessageSubmission::builder()
        .batch_id(id.into())
        .crew_id("probe-reviewer".into())
        .session("probe-reviewer".into())
        .started_at(Utc::now())
        .build();
    MessageBatch::builder().id(id.into()).holder(holder.clone()).submission(submission).text(text).interrupting(interrupting).build()
}
async fn deliver(session: &dyn AgentSession, batch: &MessageBatch) -> Result<(), String> {
    let outcome = session.submit(batch).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut outcome = outcome;
        loop {
            match outcome {
                MessageTransportOutcome::Accepted { .. } => return Ok(()),
                MessageTransportOutcome::Pending => {}
                MessageTransportOutcome::NotSubmitted { reason } | MessageTransportOutcome::Unconfirmed { reason } => return Err(reason),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            outcome = session.poll(batch).await;
        }
    })
    .await
    .map_err(|_| "native receipt timeout".to_string())?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonStatus {
    socket_path: String,
}
#[derive(serde::Deserialize)]
struct ThreadResponse {
    thread: ThreadId,
}
#[derive(serde::Deserialize)]
struct ThreadId {
    id: String,
}
async fn approval(binary: &str, binding: &HolderTransport, root: &std::path::Path) -> Result<(), String> {
    let HolderTransport::AgentApi { endpoint, .. } = binding else { return Err("native binding required".into()) };
    let status = std::process::Command::new(binary)
        .env("CODEX_HOME", endpoint)
        .args(["app-server", "daemon", "start"])
        .output()
        .map_err(|error| error.to_string())?;
    if !status.status.success() {
        return Err("daemon status failed".into());
    }
    let status: DaemonStatus = serde_json::from_slice(&status.stdout).map_err(|error| error.to_string())?;
    let client = AppServerClient::connect_with_binary(&ProcessCommandRunner, binary, &status.socket_path).await?;
    let response = client.request("thread/start", serde_json::json!({"cwd":root,"model":std::env::var("CODEX_TRANSPORT_MODEL").unwrap_or_else(|_| "gpt-6.1-sol".into()),"historyMode":"legacy","approvalPolicy":"on-request","sandbox":"read-only"})).await.map_err(|error| error.reason())?;
    let thread: ThreadResponse = serde_json::from_value(response).map_err(|error| error.to_string())?;
    let transport = CodexTransport::started(client.clone(), thread.thread.id.clone());
    let target = root.join("approval-required");
    let result: Result<(), String> = async {
        client.request("turn/start", serde_json::json!({"threadId":thread.thread.id,"input":[{"type":"text","text":format!("Use your shell tool to run mkdir {} with escalated permissions. Explicitly request approval because this test uses a read-only sandbox.", flotilla_protocol::arg::shell_quote(&target.to_string_lossy())),"text_elements":[]}]})).await.map_err(|error| error.reason())?;
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if transport.events().iter().any(|event| matches!(event, HolderEvent::ApprovalRequested { .. })) {
                    if transport.attention().await? != TerminalAttentionState::NeedsInput || target.exists() {
                        return Err("approval was not held for user input".into());
                    }
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await.map_err(|_| "structured approval did not arrive".to_string())?
    }.await;
    let cleanup = client.request("thread/archive", serde_json::json!({"threadId":thread.thread.id})).await;
    result?;
    cleanup.map_err(|error| error.reason())?;
    println!("PASS: native structured approval held without auto-approval");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().try_init();
    let root = PathBuf::from(std::env::args().nth(1).ok_or("usage: probe <scratch-directory>")?);
    let binary = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let runner = Arc::new(ProcessCommandRunner);
    let environment = std::env::vars().collect::<Vec<_>>();
    let mut launches = Vec::new();
    let mut identities = Vec::new();
    let material = PathBuf::from(std::env::var("CODEX_HOME")?);
    let mut skill_tokens = std::collections::BTreeMap::new();
    for role in ["coder", "reviewer"] {
        let role_home = material.join("crews").join(role);
        let skill = role_home.join("skills").join(format!("probe-{role}"));
        std::fs::create_dir_all(&skill)?;
        std::fs::copy(material.join("auth.json"), role_home.join("auth.json"))?;
        std::fs::write(role_home.join("config.toml"), format!("[shell_environment_policy.set]\nMATERIAL_ROLE = '{role}'\n"))?;
        let token = uuid::Uuid::new_v4().to_string();
        let target = root.join(format!("{role}-skill.txt"));
        std::fs::write(skill.join("SKILL.md"), format!("---\nname: probe-{role}\ndescription: Use when asked to prove staged role material.\n---\nRun this exact command through your shell tool: python3 -c {}\n", flotilla_protocol::arg::shell_quote(&format!("open({:?}, 'w').write({token:?})", target.to_string_lossy()))))?;
        skill_tokens.insert(role, token);
    }
    let result: Result<(), String> = async {
        for role in ["coder", "reviewer"] {
            let identity = CrewIdentity { crew: format!("probe-{role}"), role: role.into(), terminal: format!("probe-terminal-{role}"), vessel: "operator-probe".into() };
            let mut crew_environment = environment.clone();
            crew_environment.retain(|(key, _)| key != "CODEX_HOME");
            crew_environment.push(("CODEX_HOME".into(), material.join("crews").join(role).to_string_lossy().into_owned()));
            let target = root.join(format!("{role}.json"));
            let content = format!("Use the skill named probe-{role} to create its specified proof file. Do not invent the skill instructions. Also run this exact Python command using your shell tool, then finish: python3 -c {}", flotilla_protocol::arg::shell_quote(&format!("import json,os;open({:?},'w').write(json.dumps({{k:os.environ.get(k) for k in ['FLOTILLA_CREW_ID','FLOTILLA_CREW_ROLE','FLOTILLA_TERMINAL_SESSION','MATERIAL_ROLE']}}))", target.to_string_lossy())));
            let request = AgentLaunchRequest {
                role: role.into(), model: Some(std::env::var("CODEX_TRANSPORT_MODEL").unwrap_or_else(|_| "gpt-6.1-sol".into())),
                brief: TerminalBrief { path: root.join("brief.md").to_string_lossy().into_owned(), content, artifact_digest: None, copies: Vec::new() },
                environment: crew_environment.clone(), fulfilment_grants: None,
            };
            println!("probe: starting {role}");
            let launch = managed::start(&binary, runner.clone(), &ExecutionEnvironmentPath::new(&root), &request, &crew_environment, &identity, true).await?;
            launches.push(launch);
            identities.push(identity);
        }
        let home = |binding: &HolderTransport| match binding { HolderTransport::AgentApi { endpoint, .. } => endpoint.clone(), _ => panic!("native binding") };
        if home(&launches[0].binding) != home(&launches[1].binding) || launches[0].binding == launches[1].binding {
            return Err("crew bindings must share a home but have different threads".into());
        }
        for (launch, identity) in launches.iter().zip(&identities) {
            println!("probe: waiting for {} tool execution", identity.role);
            idle(&*launch.session).await?;
            let actual: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join(format!("{}.json", identity.role))).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
            let skill_output = std::fs::read_to_string(root.join(format!("{}-skill.txt", identity.role))).map_err(|error| error.to_string())?;
            if skill_output != skill_tokens[identity.role.as_str()] { return Err("role skill did not execute its configured command".into()); }
            if actual["MATERIAL_ROLE"] != identity.role { return Err("wrong source role configuration".into()); }
            for (key, value) in identity.environment() {
                if actual[&key] != value { return Err(format!("wrong tool identity: {key}")); }
            }
        }
        println!("probe: reconnecting reviewer");
        let reconnected = managed::connect(&binary, runner.clone(), &launches[1].binding, &identities[1], true).await?;
        let events = launches[1].session.events();
        if !events.iter().any(|event| matches!(event, HolderEvent::TurnStarted { .. })) || !events.iter().any(|event| matches!(event, HolderEvent::TurnCompleted { .. })) { return Err("structured turn events missing".into()); }
        let holder = holder(launches[1].binding.clone()).await?;
        let ordinary_path = root.join("ordinary-message.txt");
        let steering_path = root.join("urgent-message.txt");
        let ordinary = batch(&holder, "operator-ordinary", format!("Run this exact command using your shell tool, then finish: python3 -c {}", flotilla_protocol::arg::shell_quote(&format!("import time;time.sleep(15);open({:?}, 'w').write('ordinary')", ordinary_path.to_string_lossy()))), false);
        println!("probe: ordinary native message");
        deliver(&*reconnected, &ordinary).await?;
        if reconnected.attention().await? != TerminalAttentionState::Working { return Err("ordinary message did not start an active turn".into()); }
        let urgent = batch(&holder, "operator-urgent", format!("Also run this command before completing the current turn: python3 -c {}", flotilla_protocol::arg::shell_quote(&format!("open({:?}, 'w').write('urgent')", steering_path.to_string_lossy()))), true);
        println!("probe: urgent native steering");
        deliver(&*reconnected, &urgent).await?;
        idle(&*reconnected).await?;
        if std::fs::read_to_string(ordinary_path).map_err(|error| error.to_string())? != "ordinary" || std::fs::read_to_string(steering_path).map_err(|error| error.to_string())? != "urgent" { return Err("native message tool outputs missing".into()); }
        println!("PASS: ordinary delivery and active-turn steering have correlated receipts and actual tool effects");
        approval(&binary, &launches[1].binding, &root).await?;
        println!("probe: retiring coder");
        launches[0].session.stop(false).await?;
        idle(&*reconnected).await?;
        if launches[0].session.attention().await.is_ok() { return Err("archived crew thread is still loaded".into()); }
        println!("PASS: Rust bootstrap, separate tool identities, staged role skills, shared home, native proxy, reconnect, and independent crew retirement");
        Ok(())
    }.await;
    // Even a failed assertion must retire the private probe vessel.
    if let Some(launch) = launches.last() {
        launch.session.stop(true).await?;
    }
    result.map_err(Into::into)
}
