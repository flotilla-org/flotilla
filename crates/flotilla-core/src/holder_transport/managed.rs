//! Codex-owned daemon lifecycle. The vessel shares a daemon; crews own threads.
use super::{protocol, AppServer, AppServerClient, CodexTransport};
use crate::{
    agent_adapter::{AgentLaunchRequest, AgentSession, AgentSessionLaunch, CrewIdentity},
    path_context::ExecutionEnvironmentPath,
    providers::{terminal::TerminalEnvVars, ChannelLabel, CommandRunner},
};
use async_trait::async_trait;
use flotilla_protocol::arg::shell_quote;
use flotilla_resources::{
    HolderTransport, MessageBatch, MessageObservation, MessageSubmission, MessageTransport, MessageTransportOutcome, ResourceObject,
    TerminalAttentionState, TerminalSession,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonStatus {
    socket_path: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStart<'a> {
    // Codex 0.160's paginated daemon store cannot list turns yet.
    // Durable native receipts require the supported rollout history contract.
    history_mode: &'static str,
    cwd: &'a str,
    model: &'a Option<String>,
    approval_policy: &'a Option<String>,
    sandbox: &'a Option<String>,
    config: BTreeMap<&'static str, Value>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Resume<'a> {
    thread_id: &'a str,
    exclude_turns: bool,
    approval_policy: &'a Option<String>,
    sandbox: &'a Option<String>,
    config: BTreeMap<&'static str, Value>,
}

fn identity_config(identity: &CrewIdentity, configured: &toml::Value) -> Result<BTreeMap<&'static str, Value>, String> {
    let policy = configured.get("shell_environment_policy");
    let mut set = policy.and_then(|policy| policy.get("set")).and_then(toml::Value::as_table).cloned().unwrap_or_default();
    set.extend(identity.environment().into_iter().map(|(key, value)| (key, toml::Value::String(value))));
    let mut result = BTreeMap::from([("shell_environment_policy.set", serde_json::to_value(set).map_err(|error| error.to_string())?)]);
    if let Some(include) =
        policy.and_then(|policy| policy.get("include_only")).and_then(toml::Value::as_array).filter(|include| !include.is_empty())
    {
        let mut include = include.clone();
        for key in identity.environment().keys() {
            let key = toml::Value::String(key.clone());
            if !include.contains(&key) {
                include.push(key);
            }
        }
        result.insert("shell_environment_policy.include_only", serde_json::to_value(include).map_err(|error| error.to_string())?);
    }
    Ok(result)
}
fn selected_skills(
    config: &mut BTreeMap<&'static str, Value>,
    configured: &toml::Value,
    available: &[String],
    selected: &[String],
) -> Result<(), String> {
    let mut rules = configured
        .get("skills")
        .and_then(|skills| skills.get("config"))
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    for path in available {
        if !selected.contains(path) {
            rules.push(serde_json::json!({"path":path,"enabled":false}));
        }
    }
    if !rules.is_empty() {
        config.insert("skills.config", Value::Array(rules));
    }
    Ok(())
}
async fn skill_paths(runner: &dyn CommandRunner, root: &Path) -> Result<Vec<String>, String> {
    let output = runner
        .run(
            "sh",
            &[
                "-c",
                "if [ -d \"$1\" ]; then find -L \"$1\" -type f -name SKILL.md -exec realpath --zero -- {} +; fi",
                "flotilla-codex-skills",
                &root.to_string_lossy(),
            ],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await?;
    Ok(output.split('\0').filter(|path| !path.is_empty()).map(str::to_owned).collect())
}
async fn session_config(runner: &dyn CommandRunner, home: &Path, identity: &CrewIdentity) -> Result<BTreeMap<&'static str, Value>, String> {
    if identity.role.is_empty() || matches!(identity.role.as_str(), "." | "..") || identity.role.contains(['/', '\\', '\0']) {
        return Err("invalid Codex crew role".into());
    }
    let material = home.join("material-home");
    let role = material.join("crews").join(&identity.role);
    let role_config = role.join("config.toml");
    let config = home.join("config.toml");
    let body = runner
        .run(
            "sh",
            &[
                "-c",
                "if [ -f \"$1\" ]; then cat \"$1\"; elif [ -f \"$2\" ]; then cat \"$2\"; fi",
                "flotilla-codex-config",
                &role_config.to_string_lossy(),
                &config.to_string_lossy(),
            ],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await?;
    let configured = toml::from_str::<toml::Value>(&body).map_err(|error| format!("invalid vessel Codex configuration: {error}"))?;
    let mut result = identity_config(identity, &configured)?;
    let available = skill_paths(runner, &home.join("skills")).await?;
    // The common root contains all frozen role material. Only this role's
    // canonical paths are enabled; shared files selected by both remain enabled.
    let role_skills = runner
        .run(
            "sh",
            &[
                "-c",
                "if [ -d \"$1\" ]; then printf '%s' \"$1\"; else printf '%s' \"$2\"; fi",
                "flotilla-codex-skill-root",
                &role.join("skills").to_string_lossy(),
                &material.join("skills").to_string_lossy(),
            ],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await?;
    let selected = skill_paths(runner, Path::new(&role_skills)).await?;
    selected_skills(&mut result, &configured, &available, &selected)?;
    Ok(result)
}
pub(crate) fn policy(unattended: bool) -> (Option<String>, Option<String>) {
    (unattended.then(|| "never".into()), unattended.then(|| "danger-full-access".into()))
}
fn home(environment: &TerminalEnvVars, vessel: &str) -> Result<(PathBuf, PathBuf), String> {
    if vessel.is_empty() || !vessel.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("invalid vessel identity for Codex home".into());
    }
    let source = environment
        .iter()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| environment.iter().find(|(key, _)| key == "HOME").map(|(_, value)| Path::new(value).join(".codex")))
        .ok_or("Codex daemon needs CODEX_HOME or HOME")?;
    let base =
        source.parent().filter(|parent| parent.file_name().is_some_and(|name| name == "crews")).and_then(Path::parent).unwrap_or(&source);
    Ok((base.join("flotilla-vessels").join(vessel), source))
}
fn link_if_absent(from: &Path, to: &Path, directory_source: bool) -> String {
    // Native Codex launch is Linux-only. GNU -T is intentional: a concurrent
    // seeder can publish a directory symlink after the absence check, and plain
    // ln -s would follow it and create a link inside the source directory.
    let source_test = if directory_source { "-d" } else { "-e" };
    let from = shell_quote(&from.to_string_lossy());
    let to = shell_quote(&to.to_string_lossy());
    format!("if [ {source_test} {from} ] && [ ! -e {to} ]; then ln -sT {from} {to} 2>/dev/null || test -L {to}; fi\n")
}
async fn lifecycle(
    runner: &dyn CommandRunner,
    binary: &str,
    home: &Path,
    environment: &TerminalEnvVars,
    verb: &str,
    seed: Option<&Path>,
) -> Result<DaemonStatus, String> {
    let mut script = String::from("set -eu\n");
    for (key, value) in environment {
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || !key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return Err("invalid Codex environment key".into());
        }
        if !matches!(key.as_str(), "CODEX_HOME" | "FLOTILLA_CREW_ID" | "FLOTILLA_CREW_ROLE" | "FLOTILLA_TERMINAL_SESSION") {
            script.push_str(&format!("export {key}={}\n", shell_quote(value)));
        }
    }
    script.push_str(&format!(
        "export CODEX_HOME={}\nunset FLOTILLA_CREW_ID FLOTILLA_CREW_ROLE FLOTILLA_TERMINAL_SESSION\n",
        shell_quote(&home.to_string_lossy())
    ));
    if let Some(seed) = seed {
        script.push_str(&format!("umask 077\nmkdir -p {}\n", shell_quote(&home.to_string_lossy())));
        for file in ["auth.json", "config.toml"] {
            let from = seed.join(file);
            let to = home.join(file);
            if file == "auth.json" {
                script.push_str(&format!(
                    "if [ -f {} ]; then ln -sf {} {}; fi\n",
                    shell_quote(&from.to_string_lossy()),
                    shell_quote(&from.to_string_lossy()),
                    shell_quote(&to.to_string_lossy())
                ));
            } else {
                script.push_str(&format!(
                    "if [ -f {} ] && [ ! -e {} ]; then codex_seed_tmp=$(mktemp {})\ncp {} \"$codex_seed_tmp\"\nln \"$codex_seed_tmp\" {} 2>/dev/null || test -f {}\nrm -f \"$codex_seed_tmp\"\nfi\n",
                    shell_quote(&from.to_string_lossy()),
                    shell_quote(&to.to_string_lossy()),
                    shell_quote(&home.join("config.XXXXXX").to_string_lossy()),
                    shell_quote(&from.to_string_lossy()),
                    shell_quote(&to.to_string_lossy()),
                    shell_quote(&to.to_string_lossy())
                ));
            }
        }
        let base = if seed.parent().and_then(Path::file_name).is_some_and(|name| name == "crews") {
            seed.parent().and_then(Path::parent).ok_or("invalid crew material home")?
        } else {
            seed
        };
        for (name, from) in
            [("material-home", base.to_path_buf()), ("plugins", base.join("plugins")), ("AGENTS.md", base.join("AGENTS.md"))]
        {
            script.push_str(&link_if_absent(&from, &home.join(name), false));
        }
        let skills = if seed.parent().and_then(Path::file_name).is_some_and(|name| name == "crews") {
            base.join("crews")
        } else {
            seed.join("skills")
        };
        script.push_str(&link_if_absent(&skills, &home.join("skills"), true));
        let settings_dir = home.join("app-server-daemon");
        let settings = settings_dir.join("settings.json");
        script.push_str(&format!(
            "mkdir -p {}\nif [ ! -e {} ]; then settings_tmp=$(mktemp {})\nprintf '%s' {} > \"$settings_tmp\"\nln \"$settings_tmp\" {} 2>/dev/null || test -f {}\nrm -f \"$settings_tmp\"\nfi\n",
            shell_quote(&settings_dir.to_string_lossy()),
            shell_quote(&settings.to_string_lossy()),
            shell_quote(&settings_dir.join("settings.XXXXXX").to_string_lossy()),
            shell_quote(r#"{"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":2}"#),
            shell_quote(&settings.to_string_lossy()),
            shell_quote(&settings.to_string_lossy())
        ));
    }
    if verb == "bootstrap" {
        // Bootstrap replaces a running daemon. Serialize only first-install
        // selection and use idempotent start once our private package exists.
        // -o prevents the detached daemon from inheriting the bootstrap lock.
        script.push_str(&format!(
            "exec flock -o {} sh -c {} sh {}\n",
            shell_quote(&home.join("flotilla-bootstrap.lock").to_string_lossy()),
            shell_quote("if [ -x \"$CODEX_HOME/packages/app-server-daemon/current/bin/codex\" ]; then exec \"$1\" app-server daemon start; else exec \"$1\" app-server daemon bootstrap; fi"),
            shell_quote(binary)
        ));
    } else {
        script.push_str(&format!("exec {} app-server daemon {verb}\n", shell_quote(binary)));
    }
    let output = tokio::time::timeout(
        // Existing owned homes may have a configured grace up to 300s.
        Duration::from_secs(360),
        runner.run_with_input("sh", &["-s"], Path::new("/"), &ChannelLabel::Default, script.as_bytes()),
    )
    .await
    .map_err(|_| "Codex daemon lifecycle timed out")??;
    serde_json::from_str(&output).map_err(|error| format!("invalid Codex daemon lifecycle reply: {error}"))
}

pub struct ManagedSession {
    transport: Arc<CodexTransport>,
    runner: Arc<dyn CommandRunner>,
    binary: String,
    home: PathBuf,
    binding: HolderTransport,
}
#[async_trait]
impl MessageTransport for ManagedSession {
    fn binding(&self, _: &ResourceObject<TerminalSession>) -> Option<HolderTransport> {
        Some(self.binding.clone())
    }
    fn supports_steering(&self, _: &ResourceObject<TerminalSession>) -> bool {
        true
    }
    fn uses_structured_evidence(&self, _: &ResourceObject<TerminalSession>) -> bool {
        true
    }
    async fn observe(&self, h: &ResourceObject<TerminalSession>, s: Option<&MessageSubmission>) -> Result<MessageObservation, String> {
        self.transport.observe(h, s).await
    }
    async fn submit(&self, b: &MessageBatch) -> MessageTransportOutcome {
        self.transport.submit(b).await
    }
    async fn poll(&self, b: &MessageBatch) -> MessageTransportOutcome {
        self.transport.poll(b).await
    }
}
#[async_trait]
impl AgentSession for ManagedSession {
    fn events(&self) -> Vec<super::HolderEvent> {
        self.transport.events()
    }
    fn available(&self) -> bool {
        self.transport.available()
    }
    async fn attention(&self) -> Result<TerminalAttentionState, String> {
        self.transport.attention().await
    }
    async fn stop(&self, retiring_vessel: bool) -> Result<(), String> {
        // Archive unloads this thread, including its current work; never stop
        // the shared daemon merely because a crew exits or is replaced.
        if retiring_vessel {
            lifecycle(&*self.runner, &self.binary, &self.home, &Vec::new(), "stop", None).await?;
            return Ok(());
        }
        self.transport
            .rpc
            .request(
                "thread/archive",
                serde_json::to_value(protocol::ThreadId { thread_id: &self.transport.thread }).map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.reason())?;
        Ok(())
    }
}

async fn start_thread(
    client: Arc<dyn AppServer>,
    params: ThreadStart<'_>,
    request: &AgentLaunchRequest,
    identity: &CrewIdentity,
) -> Result<Arc<CodexTransport>, String> {
    let response: protocol::ThreadResponse = serde_json::from_value(
        client
            .request("thread/start", serde_json::to_value(params).map_err(|error| error.to_string())?)
            .await
            .map_err(|error| error.reason())?,
    )
    .map_err(|error| error.to_string())?;
    let thread = response.thread.id;
    let prompt =
        format!("{}\n\nThis is also at `{}`; re-read that file after a context compaction.", request.brief.content, request.brief.path);
    let batch = format!("launch-{}", identity.crew);
    let input = protocol::Input {
        thread_id: &thread,
        client_user_message_id: &batch,
        expected_turn_id: None,
        input: vec![protocol::Content::Text { text: format!("[flotilla batch: {batch}]\n{prompt}"), text_elements: Vec::new() }],
    };
    let transport = CodexTransport::started(client.clone(), thread.clone());
    let launched = async {
        client
            .request("turn/start", serde_json::to_value(input).map_err(|error| error.to_string())?)
            .await
            .map_err(|error| error.reason())?;
        transport.wait_for_launch_receipt(&batch).await
    }
    .await;
    if let Err(error) = launched {
        // A failed crew launch owns its thread, never the shared vessel daemon.
        let cleanup = client
            .request("thread/archive", serde_json::to_value(protocol::ThreadId { thread_id: &thread }).map_err(|error| error.to_string())?)
            .await;
        if let Err(cleanup) = cleanup {
            tracing::warn!(thread, reason = %cleanup.reason(), "failed to archive unsuccessful Codex launch");
        }
        return Err(error);
    }
    Ok(transport)
}

pub async fn start(
    binary: &str,
    runner: Arc<dyn CommandRunner>,
    cwd: &ExecutionEnvironmentPath,
    request: &AgentLaunchRequest,
    environment: &TerminalEnvVars,
    identity: &CrewIdentity,
    unattended: bool,
) -> Result<AgentSessionLaunch, String> {
    let (home, source) = home(environment, &identity.vessel)?;
    let daemon = lifecycle(&*runner, binary, &home, environment, "bootstrap", Some(&source)).await?;
    let client =
        AppServerClient::connect_with_binary(&*runner, binary, daemon.socket_path.as_deref().ok_or("Codex daemon omitted socket path")?)
            .await?;
    let (approval_policy, sandbox) = policy(unattended);
    let config = session_config(&*runner, &home, identity).await?;
    let overlay = config
        .iter()
        .map(|(key, value)| {
            let value = toml::Value::try_from(value).map_err(|error| error.to_string())?;
            Ok(format!("-c {}", shell_quote(&format!("{key}={value}"))))
        })
        .collect::<Result<Vec<_>, String>>()?
        .join(" ");
    let params = ThreadStart {
        history_mode: "legacy",
        cwd: cwd.as_path().to_str().ok_or("non-UTF8 Codex cwd")?,
        model: &request.model,
        approval_policy: &approval_policy,
        sandbox: &sandbox,
        config,
    };
    let transport = start_thread(client, params, request, identity).await?;
    let thread = transport.thread.clone();
    let binding =
        HolderTransport::AgentApi { adapter: "codex".into(), endpoint: home.to_string_lossy().into_owned(), session: thread.clone() };
    let attach_command = format!(
        "env CODEX_HOME={} {} --remote {} {} {} {} {} resume {}",
        shell_quote(&home.to_string_lossy()),
        shell_quote(binary),
        shell_quote(&format!("unix://{}", daemon.socket_path.as_deref().ok_or("Codex daemon omitted socket path")?)),
        overlay,
        request.model.as_deref().map(|model| format!("-m {}", shell_quote(model))).unwrap_or_default(),
        approval_policy.as_deref().map(|policy| format!("-a {}", shell_quote(policy))).unwrap_or_default(),
        sandbox.as_deref().map(|mode| format!("-s {}", shell_quote(mode))).unwrap_or_default(),
        shell_quote(&thread)
    );
    Ok(AgentSessionLaunch {
        attach_command,
        binding: binding.clone(),
        session: Arc::new(ManagedSession { transport, runner, binary: binary.into(), home, binding }),
    })
}
/// Vessel finalization must never bootstrap a daemon just to stop it.
pub async fn stop_vessel(binary: &str, runner: &dyn CommandRunner, binding: &HolderTransport) -> Result<(), String> {
    let HolderTransport::AgentApi { endpoint, .. } = binding else { return Err("Codex binding is not an agent API".into()) };
    lifecycle(runner, binary, Path::new(endpoint), &Vec::new(), "stop", None).await?;
    Ok(())
}

pub async fn connect(
    binary: &str,
    runner: Arc<dyn CommandRunner>,
    binding: &HolderTransport,
    identity: &CrewIdentity,
    unattended: bool,
) -> Result<Arc<dyn AgentSession>, String> {
    let HolderTransport::AgentApi { endpoint, session, .. } = binding else { return Err("Codex binding is not an agent API".into()) };
    let home = PathBuf::from(endpoint);
    let daemon = lifecycle(&*runner, binary, &home, &Vec::new(), "start", None).await?;
    let client =
        AppServerClient::connect_with_binary(&*runner, binary, daemon.socket_path.as_deref().ok_or("Codex daemon omitted socket path")?)
            .await?;
    let approval_policy = unattended.then(|| "never".into());
    let sandbox = unattended.then(|| "danger-full-access".into());
    let params = Resume {
        thread_id: session,
        exclude_turns: false,
        approval_policy: &approval_policy,
        sandbox: &sandbox,
        config: session_config(&*runner, &home, identity).await?,
    };
    client
        .request("thread/resume", serde_json::to_value(params).map_err(|error| error.to_string())?)
        .await
        .map_err(|error| error.reason())?;
    Ok(Arc::new(ManagedSession {
        transport: CodexTransport::started(client, session.clone()),
        runner,
        binary: binary.into(),
        home,
        binding: binding.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    // Stateful peer: archiving one crew leaves other threads available;
    // stopping the vessel invalidates every thread connection.
    #[derive(Default)]
    struct VesselPeer {
        threads: std::sync::Mutex<std::collections::BTreeSet<String>>,
        stopped: std::sync::atomic::AtomicBool,
        archive_fails: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl AppServer for VesselPeer {
        async fn request(&self, method: &str, params: Value) -> Result<Value, super::super::RpcError> {
            use super::super::RpcError;
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RpcError::Unavailable("vessel stopped".into()));
            }
            let mut threads = self.threads.lock().expect("threads");
            if method == "thread/start" {
                assert_eq!(params["historyMode"], "legacy", "durable receipts require rollout history");
                threads.insert("new".into());
                return Ok(serde_json::json!({"thread":{"id":"new","status":{"type":"idle"},"turns":[]}}));
            }
            let thread = params["threadId"].as_str().expect("thread id");
            match method {
                "turn/start" => Err(RpcError::Rejected("launch rejected".into())),
                "thread/archive" if self.archive_fails.load(std::sync::atomic::Ordering::SeqCst) => {
                    Err(RpcError::Rejected("archive failed".into()))
                }
                "thread/archive" => {
                    threads.remove(thread);
                    Ok(serde_json::json!({}))
                }
                "thread/read" if threads.contains(thread) => {
                    Ok(serde_json::json!({"thread":{"id":thread,"status":{"type":"idle"},"turns":[]}}))
                }
                _ => Err(RpcError::Rejected("thread unavailable".into())),
            }
        }
    }
    #[async_trait]
    impl CommandRunner for VesselPeer {
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            panic!("unexpected command")
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<crate::providers::CommandOutput, String> {
            panic!("unexpected output command")
        }
        async fn run_with_input(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            let script = std::str::from_utf8(input).expect("script");
            assert!(script.contains("app-server daemon stop"));
            self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("{}".into())
        }
    }
    #[tokio::test]
    async fn retiring_one_crew_keeps_the_other_thread_alive_until_vessel_teardown() {
        let peer = Arc::new(VesselPeer::default());
        peer.threads.lock().expect("threads").extend(["coder".into(), "reviewer".into()]);
        let session = |thread: &str| ManagedSession {
            transport: CodexTransport::started(peer.clone(), thread.into()),
            runner: peer.clone(),
            binary: "codex".into(),
            home: PathBuf::from("/private/vessel"),
            binding: HolderTransport::AgentApi { adapter: "codex".into(), endpoint: "/private/vessel".into(), session: thread.into() },
        };
        let coder = session("coder");
        let reviewer = session("reviewer");
        coder.stop(false).await.expect("retire coder");
        assert!(coder.attention().await.is_err(), "retired thread is unloaded");
        assert!(reviewer.attention().await.is_ok(), "other crew retains native input");
        reviewer.stop(true).await.expect("retire vessel");
        assert!(reviewer.attention().await.is_err(), "vessel teardown stops its daemon");
    }

    // Failed launch owns only its new thread. Cleanup failure must retain
    // the original launch diagnostic and cannot stop another crew's daemon.
    #[tokio::test]
    async fn failed_launch_archives_only_its_thread_and_preserves_the_original_error() {
        use flotilla_resources::TerminalBrief;
        for archive_fails in [false, true] {
            let peer = Arc::new(VesselPeer::default());
            peer.threads.lock().expect("threads").insert("other-crew".into());
            peer.archive_fails.store(archive_fails, std::sync::atomic::Ordering::SeqCst);
            let identity = CrewIdentity { crew: "new".into(), role: "coder".into(), terminal: "terminal".into(), vessel: "vessel".into() };
            let request = AgentLaunchRequest {
                role: "coder".into(),
                model: None,
                environment: Vec::new(),
                fulfilment_grants: None,
                brief: TerminalBrief { path: "/brief".into(), content: "brief".into(), artifact_digest: None, copies: Vec::new() },
            };
            let params = ThreadStart {
                history_mode: "legacy",
                cwd: "/work",
                model: &None,
                approval_policy: &None,
                sandbox: &None,
                config: BTreeMap::new(),
            };
            let error = start_thread(peer.clone(), params, &request, &identity).await.err().expect("launch failure");
            assert_eq!(error, "launch rejected");
            let threads = peer.threads.lock().expect("threads");
            assert!(threads.contains("other-crew"));
            assert_eq!(threads.contains("new"), archive_fails);
            assert!(!peer.stopped.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    // Process-boundary fake models Codex's destructive bootstrap versus
    // idempotent start. Real shell/flock collaborators exercise concurrent ensure.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn concurrent_crew_ensure_bootstraps_once_and_preserves_existing_work() {
        use crate::providers::ProcessCommandRunner;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("scratch");
        let source = root.path().join("source/crews/coder");
        let home = root.path().join("vessel");
        for role in ["coder", "reviewer"] {
            let role_home = root.path().join("source/crews").join(role);
            std::fs::create_dir_all(role_home.join("skills").join(role)).expect("role skills");
            std::fs::write(role_home.join("skills").join(role).join("SKILL.md"), "skill").expect("skill");
            std::fs::write(role_home.join("config.toml"), format!("[shell_environment_policy.set]\nMATERIAL_ROLE = '{role}'\n"))
                .expect("role config");
        }
        let binary = root.path().join("codex");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
set -eu
case "$3" in
 bootstrap)
  mkdir -p "$CODEX_HOME/packages/app-server-daemon/current/bin"
  cp "$0" "$CODEX_HOME/packages/app-server-daemon/current/bin/codex"
  printf 'bootstrap\n' >> "$CODEX_HOME/lifecycle"
  : > "$CODEX_HOME/work"
  ;;
 start) printf 'start\n' >> "$CODEX_HOME/lifecycle" ;;
 *) exit 2 ;;
esac
[ "${FLOTILLA_CREW_ID-unset}" = unset ]
printf '{"socketPath":"/private/socket"}\n'
"#,
        )
        .expect("fake CLI");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let runner = ProcessCommandRunner;
        let environment = vec![("FLOTILLA_CREW_ID".into(), "must-not-leak".into())];
        let binary = binary.to_str().expect("binary path");
        let (first, second) = tokio::join!(
            lifecycle(&runner, binary, &home, &environment, "bootstrap", Some(&source)),
            lifecycle(&runner, binary, &home, &environment, "bootstrap", Some(&source))
        );
        first.expect("first ensure");
        second.expect("concurrent ensure");
        let history = std::fs::read_to_string(home.join("lifecycle")).expect("history");
        assert_eq!(history.lines().filter(|line| *line == "bootstrap").count(), 1);
        std::fs::write(home.join("work"), "existing crew").expect("work");
        lifecycle(&runner, binary, &home, &environment, "bootstrap", Some(&source)).await.expect("later crew");
        assert_eq!(std::fs::read_to_string(home.join("work")).expect("work"), "existing crew");
        for role in ["coder", "reviewer"] {
            let identity = CrewIdentity { crew: role.into(), role: role.into(), terminal: "terminal".into(), vessel: "vessel".into() };
            let config = session_config(&runner, &home, &identity).await.expect("role overlay");
            assert_eq!(config["shell_environment_policy.set"]["MATERIAL_ROLE"], role);
            let rules = config["skills.config"].as_array().expect("skill selection");
            assert_eq!(rules.len(), 1);
            let other_role = if role == "coder" { "reviewer" } else { "coder" };
            let other =
                std::fs::canonicalize(root.path().join("source/crews").join(other_role).join("skills").join(other_role).join("SKILL.md"))
                    .expect("canonical skill");
            assert_eq!(rules[0]["path"], other.to_string_lossy().as_ref());
            assert_eq!(rules[0]["enabled"], false);
        }
    }

    // Canonical file paths span empty, partial, and complete role selections;
    // configured name-based exclusions survive every selection.
    #[hegel::test]
    fn role_skill_selection_preserves_configured_rules_and_disables_only_other_roles(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<u8>().min_value(0).max_value(8));
        let available = (0..count).map(|index| format!("/skills/{index}/SKILL.md")).collect::<Vec<_>>();
        let selected = available.iter().filter(|_| tc.draw(gs::booleans())).cloned().collect::<Vec<_>>();
        let configured = toml::from_str("[[skills.config]]\nname = 'operator-disabled'\nenabled = false").expect("configuration");
        let mut result = BTreeMap::new();
        selected_skills(&mut result, &configured, &available, &selected).expect("selection");
        let rules = result["skills.config"].as_array().expect("rules");
        assert_eq!(rules[0], serde_json::json!({"name":"operator-disabled","enabled":false}));
        for path in &available {
            assert_eq!(rules.iter().any(|rule| rule["path"] == *path && rule["enabled"] == false), !selected.contains(path));
        }
        assert_eq!(rules.len(), 1 + available.len() - selected.len());
    }

    #[hegel::test]
    fn thread_identity_preserves_configured_environment_and_filters(tc: hegel::TestCase) {
        let crew = tc.draw(gs::integers::<u64>());
        let identity =
            CrewIdentity { crew: format!("crew-{crew}"), role: "coder".into(), terminal: "terminal".into(), vessel: "vessel".into() };
        let filtered = tc.draw(gs::booleans());
        let configured: toml::Value = toml::from_str(if filtered {
            "[shell_environment_policy]\ninclude_only = ['PATH']\n[shell_environment_policy.set]\nKEEP = 'configured'\nFLOTILLA_CREW_ID = 'stale'"
        } else {
            "[shell_environment_policy.set]\nKEEP = 'configured'\nFLOTILLA_CREW_ID = 'stale'"
        }).expect("configuration");
        let config = identity_config(&identity, &configured).expect("thread config");
        assert_eq!(config["shell_environment_policy.set"]["KEEP"], "configured");
        assert_eq!(config["shell_environment_policy.set"]["FLOTILLA_CREW_ID"], identity.crew);
        assert_eq!(config["shell_environment_policy.set"]["FLOTILLA_CREW_ROLE"], "coder");
        if filtered {
            let included = config["shell_environment_policy.include_only"].as_array().expect("filter");
            assert_eq!(included.len(), 4);
            assert!(included.iter().any(|value| value == "PATH"));
            for key in identity.environment().keys() {
                assert!(included.iter().any(|value| value == key));
            }
        } else {
            assert!(!config.contains_key("shell_environment_policy.include_only"));
        }
    }

    #[test]
    fn crews_share_one_vessel_home_but_never_adopt_an_ambient_daemon() {
        let coder = vec![("CODEX_HOME".into(), "/config/crews/coder".into())];
        let reviewer = vec![("CODEX_HOME".into(), "/config/crews/reviewer".into())];
        assert_eq!(home(&coder, "work").expect("coder").0, home(&reviewer, "work").expect("reviewer").0);
        assert_ne!(home(&coder, "work").expect("work").0, home(&coder, "other").expect("other").0);
        assert_eq!(
            home(&vec![("HOME".into(), "/user".into())], "work").expect("host direct").0,
            Path::new("/user/.codex/flotilla-vessels/work")
        );
        assert!(home(&coder, "../escape").is_err());
    }
}
