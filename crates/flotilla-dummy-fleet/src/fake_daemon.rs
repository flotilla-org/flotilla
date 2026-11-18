//! For tests of the crews alone: a daemon stand-in at the socket a dummy vessel's agent forwards
//! to. It speaks the daemon's wire as a crew meets it, one connection per `flotilla` call: Hello
//! then one request, a command's result as its `CommandFinished` event, and Claude Code's HTTP
//! hook posts. It answers crew commands as the test's script says and records what arrives.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_environment::environment::{PrepareOpts, ProvisionOpts};
use flotilla_protocol::{
    commands::{CommandValue, CrewPromiseOperation, CrewSupervisionAction, StallProposedDisposition},
    CommandAction, DaemonEvent, EnvironmentId, Message, NodeId, RepoIdentity, Request, Response,
};
use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

use crate::{fleet::DummyFleet, LAST_TURN_MARKER};

/// The hook URL and token the stand-in gives a crew's launch environment.
pub(crate) const HOOK_URL: &str = "http://127.0.0.1:15080/v1/hooks/claude-code";
pub(crate) const HOOK_TOKEN: &str = "hook-token";

/// What the stand-in answers and records.
pub(crate) struct FakeDaemon {
    /// Every submitted change-request reference starts with this.
    pub(crate) reference_prefix: String,
    /// Handoffs to refuse before accepting them.
    pub(crate) handoff_refusals: Mutex<usize>,
    /// The crew commands and publications, in arrival order.
    commands: mpsc::UnboundedSender<String>,
    /// Hook events: an HTTP post's `<event>`, or `command:<native hook>` for a command hook.
    pub(crate) hooks: Mutex<Vec<String>>,
    /// Each connection's Hello claims.
    pub(crate) claims: Mutex<Vec<BTreeMap<String, String>>>,
    _directory: tempfile::TempDir,
}

impl FakeDaemon {
    /// A stand-in serving the vessel `environment` the fleet's container vendor realises, whose
    /// agent forwards the daemon endpoint to it, and the receiver of the commands it records.
    pub(crate) async fn serve(
        fleet: &DummyFleet,
        environment: &str,
        reference_prefix: &str,
        handoff_refusals: usize,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let (commands, recorded) = mpsc::unbounded_channel();
        let daemon = Arc::new(Self {
            reference_prefix: reference_prefix.into(),
            handoff_refusals: Mutex::new(handoff_refusals),
            commands,
            hooks: Mutex::default(),
            claims: Mutex::default(),
            _directory: directory,
        });
        realise_vessel(fleet, environment, &socket).await;
        let serving = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Some(daemon) = serving.upgrade() else { return };
                tokio::spawn(async move { daemon.connection(stream).await });
            }
        });
        (daemon, recorded)
    }

    async fn connection(self: Arc<Self>, mut stream: UnixStream) {
        let mut preface = [0u8; 5];
        if stream.read_exact(&mut preface).await.is_err() {
            return;
        }
        if &preface == b"POST " {
            return self.http(stream).await;
        }
        let session = flotilla_transport::message::unix_message_session_with_prefix(stream, preface.to_vec());
        let Ok(Some(Message::Hello { display_name, crew_claims, .. })) = session.read().await else { return };
        self.claims.lock().expect("claims").push(crew_claims);
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("fake-daemon"),
            // The same build answers, as the fleet rolls as a unit.
            display_name,
            session_id: uuid::Uuid::new_v4(),
            connection_role: Some(flotilla_protocol::ConnectionRole::Client),
            surface: None,
            crew_claims: BTreeMap::new(),
        };
        if session.write(hello).await.is_err() {
            return;
        }
        while let Ok(Some(Message::Request { id, request })) = session.read().await {
            let reply = match request {
                Request::Execute { command, .. } if command.action.is_query() => {
                    let value = CommandValue::Error { message: "the stand-in explains nothing".into() };
                    vec![Message::ok_response(id, Response::QueryResult { command_id: id, value })]
                }
                Request::Execute { command, .. } => {
                    let finished = DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: NodeId::new("fake-daemon"),
                        repo_identity: RepoIdentity { authority: String::new(), path: String::new() },
                        repo: None,
                        result: self.command(command.action),
                    };
                    vec![Message::ok_response(id, Response::Execute { command_id: id }), Message::Event { event: Box::new(finished) }]
                }
                Request::AgentHook { event } => {
                    self.hooks.lock().expect("hooks").push(format!("command:{}", event.hook_event.unwrap_or_default()));
                    vec![Message::ok_response(id, Response::AgentHook)]
                }
                Request::ArtifactPut { kind, .. } => {
                    assert_eq!(kind, "decision-ledger");
                    self.commands.send("ledger".into()).expect("test listens");
                    let response = Response::ArtifactPut { address: "artifact/ledger".into(), digest: "digest".into(), view_url: None };
                    vec![Message::ok_response(id, response)]
                }
                other => panic!("dummy crews send only crew requests, not {other:?}"),
            };
            for message in reply {
                if session.write(message).await.is_err() {
                    return;
                }
            }
        }
    }

    fn command(&self, action: CommandAction) -> CommandValue {
        let (entry, value) = match action {
            CommandAction::CrewPromise { operation: CrewPromiseOperation::Submit { kind, .. }, .. } if kind == "decision-ledger" => {
                ("submit-ledger".to_string(), CommandValue::Ok)
            }
            CommandAction::CrewPromise { operation: CrewPromiseOperation::Submit { reference, .. }, .. } => {
                assert!(reference.starts_with(&self.reference_prefix), "{reference}");
                ("submit".to_string(), CommandValue::Ok)
            }
            CommandAction::CrewHandoff { target, message, .. } => {
                let mut refusals = self.handoff_refusals.lock().expect("refusals");
                if *refusals > 0 {
                    *refusals -= 1;
                    ("handoff-refused".to_string(), CommandValue::Error { message: "resource conflict".into() })
                } else {
                    let last = if message.contains(LAST_TURN_MARKER) { "!" } else { "" };
                    (format!("handoff:{target}{last}"), CommandValue::Ok)
                }
            }
            CommandAction::CrewStall { proposed_disposition, .. } => {
                let proposal = if proposed_disposition == Some(StallProposedDisposition::Fail) { "fail" } else { "resume" };
                (format!("stall:{proposal}"), CommandValue::Ok)
            }
            CommandAction::CrewSupervise { convoy, vessel, role, operation: CrewSupervisionAction::Resume, .. } => {
                (format!("resume:{convoy}/{vessel}/{role}"), CommandValue::Ok)
            }
            other => panic!("dummy crews send only crew commands, not {other:?}"),
        };
        self.commands.send(entry).expect("test listens");
        value
    }

    /// One Claude Code hook post: authorised by the launch's token, answered with no content.
    async fn http(&self, mut stream: UnixStream) {
        let mut request = b"POST ".to_vec();
        let mut buffer = [0u8; 4096];
        let head_end = loop {
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end;
            }
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_string();
        let path = head.split_whitespace().nth(1).unwrap_or_default();
        let event = path.strip_prefix("/v1/hooks/claude-code/").unwrap_or_else(|| panic!("hook path {path}"));
        assert!(head.contains(&format!("Authorization: Bearer {HOOK_TOKEN}")), "{head}");
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|length| length.trim().parse::<usize>().ok())
            .unwrap_or_default();
        while request.len() < head_end + 4 + length {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let body: serde_json::Value = serde_json::from_slice(&request[head_end + 4..]).expect("a JSON hook payload");
        assert!(body["session_id"].is_string() && body["cwd"].is_string(), "{body}");
        self.hooks.lock().expect("hooks").push(event.to_string());
        let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").await;
    }
}

/// Realise `environment` with the fleet's container vendor, and have its agent forward the
/// vessel's daemon endpoint to `socket`, as the daemon asks when it places a crew there.
async fn realise_vessel(fleet: &DummyFleet, environment: &str, socket: &Path) {
    use flotilla_discovery_api::Factory;
    let vendor = &fleet.vendor.factories[0];
    let config_dir = tempfile::tempdir().expect("config directory");
    let config = flotilla_config::ConfigStore::with_base(config_dir.path().to_path_buf());
    let runner: Arc<dyn flotilla_provider_runtime::CommandRunner> =
        Arc::new(flotilla_environment::environment::dummy::DummyCommandRunner::default());
    let root = flotilla_paths::path_context::ExecutionEnvironmentPath::new("/");
    let provider =
        vendor.probe(&flotilla_discovery_api::EnvironmentBag::new(), &config, &root, runner).await.expect("the container vendor");
    let spec = EnvironmentSpec {
        cloud_sandbox: None,
        provider_address: None,
        host_direct: None,
        docker: Some(DockerEnvironmentSpec {
            host_ref: "host".into(),
            image: "dummy/crew:latest".into(),
            image_build_ref: None,
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            mounts: Vec::new(),
            env: Default::default(),
        }),
    };
    // A vessel the previous process's fleet realised is adopted; otherwise it is realised now.
    let id = EnvironmentId::new(environment);
    let handle = match provider.inspect(&id).await.expect("inspect") {
        Some(handle) => handle,
        None => {
            let prepared = provider.prepare(&spec, &PrepareOpts::default()).await.expect("prepare");
            provider.provision(id, &prepared, ProvisionOpts::default()).await.expect("provision")
        }
    };
    handle.agent().forward_controller_socket(Path::new("/run/flotilla.sock"), None, socket).await.expect("forward");
    assert_eq!(fleet.vendor.forward_target(environment), Some(PathBuf::from(socket)));
}
