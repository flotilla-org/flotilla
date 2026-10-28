//! #3618 and #3792 acceptance through a real `DaemonServer`: an SSH
//! host's crews reach the controller only through their vessel agent's guest
//! forward to a per-vessel environment socket.
//!
//! The vessel agent is the injected collaborator. `GuestForward` serves the
//! guest path and relays each connection to the host endpoint, exactly the
//! bytes the agent carries over its link (drydock's own forward is covered by
//! flotilla-environment's agent tests).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::{
    AgentEventType, AgentHarness, AgentHookEvent, AgentHookTerminalRef, AttachableId, Command, CommandAction, CommandValue,
    CrewCommandContext, DaemonEvent, EnvironmentId, Message, NodeId, Request, Response, ResponseResult, PROTOCOL_VERSION,
};
use flotilla_resources::Convoy;
use flotilla_test_support::TestSocketDir;
use flotilla_transport::message::{unix_message_session, MessageSession};
use tokio::{net::UnixListener, task::JoinHandle};

use super::super::DaemonServer;
use super::{ok_response, place_crew, read_session_message, test_config_store, PlacedCrew};
use crate::server::environment_sockets::EnvironmentSocketService;
use flotilla_discovery_testkit::fake_discovery;

/// Where the vessel's daemon forward leads: the per-vessel environment socket
/// the controller registered, or (the mutation) the controller's main socket.
#[derive(Clone)]
enum Relay {
    AsRequested,
    MainSocket(PathBuf),
}

/// Stands in for the vessel agent's guest forward.
struct GuestForward(JoinHandle<()>);

impl GuestForward {
    fn serve(guest: &Path, host: PathBuf) -> Self {
        let listener = UnixListener::bind(guest).expect("bind guest forward");
        Self(tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let host = host.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::UnixStream::connect(&host).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        }))
    }
}

impl Drop for GuestForward {
    fn drop(&mut self) {
        self.0.abort();
    }
}

const VESSEL: &str = "demo-work";

struct Fixture {
    server_task: JoinHandle<Result<(), String>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    daemon: Arc<flotilla_core::in_process::InProcessDaemon>,
    service: EnvironmentSocketService,
    environment: String,
    /// The crews' `FLOTILLA_DAEMON_SOCKET` on the host.
    guest: PathBuf,
    forward: GuestForward,
    _tmp: tempfile::TempDir,
    _sockets: TestSocketDir,
    _home: TestSocketDir,
}

impl Fixture {
    async fn new(relay: impl FnOnce(&Path) -> Relay, crews: &[PlacedCrew<'_>]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sockets = TestSocketDir::new();
        let socket_path = sockets.socket_path("main.sock");
        let config = test_config_store(tmp.path().join("config"));
        let server = DaemonServer::new(Vec::new(), config, fake_discovery(false), socket_path.clone(), std::time::Duration::from_secs(60))
            .await
            .expect("daemon server");
        let daemon = server.daemon();
        server.install_blob_store(Arc::new(crate::blob_store::TieredBlobStore::new(tmp.path(), Vec::new()))).expect("blob store");
        let service = server.environment_socket_service();
        let shutdown = server.shutdown_tx.clone();
        let server_task = tokio::spawn(server.run());
        while !socket_path.exists() {
            tokio::task::yield_now().await;
        }
        // The fixture's crews run in the daemon's own environment so the
        // artifact dispatcher can read their files through a real runner, as it
        // reads an SSH host crew's files through its vessel's agent.
        let environment = daemon.local_environment_id().to_string();
        for crew in crews {
            place_crew(&daemon, &environment, PlacedCrew { ..*crew }).await;
        }
        let home = TestSocketDir::new();
        let endpoint = service.prepare_vessel_forwarded(EnvironmentId::new(environment.clone()), VESSEL).await.expect("vessel socket");
        let target = match relay(&socket_path) {
            Relay::AsRequested => endpoint.as_path().to_path_buf(),
            Relay::MainSocket(main) => main,
        };
        let guest = home.path().join("flotilla.sock");
        let forward = GuestForward::serve(&guest, target);
        Self { server_task, shutdown, daemon, service, environment, guest, forward, _tmp: tmp, _sockets: sockets, _home: home }
    }

    /// The crew side: what `flotilla` on the host does with `FLOTILLA_DAEMON_SOCKET`.
    async fn connect(&self, socket: &Path, claims: &[(&str, &str)]) -> MessageSession {
        let session = unix_message_session(tokio::net::UnixStream::connect(socket).await.expect("connect forwarded socket"));
        session
            .write(Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                node_id: NodeId::new("client"),
                display_name: flotilla_protocol::hello_display_name(
                    "client",
                    flotilla_daemon_api::build_info::build_id(),
                    flotilla_protocol::PROTOCOL_FINGERPRINT,
                ),
                session_id: uuid::Uuid::new_v4(),
                connection_role: Some(flotilla_protocol::ConnectionRole::Client),
                surface: None,
                crew_claims: claims.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<BTreeMap<_, _>>(),
            })
            .await
            .expect("hello");
        assert!(matches!(read_session_message(&session).await, Message::Hello { .. }));
        session
    }

    async fn stop(self) {
        drop(self.forward);
        self.service.remove_vessel(&EnvironmentId::new(self.environment.clone()), VESSEL).await.expect("retire vessel socket");
        self.shutdown.send(true).expect("shutdown");
        self.server_task.await.expect("server task").expect("server stops");
    }
}

async fn request(session: &MessageSession, id: u64, request: Request) -> Message {
    session.write(Message::Request { id, request }).await.expect("request");
    read_session_message(session).await
}

fn refusal(message: Message) -> String {
    match message {
        Message::Response { response, .. } => match *response {
            ResponseResult::Err { message, .. } => message,
            ResponseResult::Ok { response } => panic!("expected refusal, got {response:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
}

/// Promise work as the connected crew; the promise lands in that crew's convoy.
async fn promise(session: &MessageSession, id: u64, promise_id: &str) {
    let Response::Execute { command_id } = ok_response(
        request(
            session,
            id,
            Request::Execute {
                command: Command::builder()
                    .action(CommandAction::CrewPromise {
                        context: CrewCommandContext::default(),
                        operation: flotilla_protocol::commands::CrewPromiseOperation::Promise { id: promise_id.into(), kind: "pr".into() },
                    })
                    .build(),
            },
        )
        .await,
        id,
    ) else {
        panic!("expected dispatched command")
    };
    assert!(matches!(read_session_message(session).await, Message::Event { event }
        if matches!(event.as_ref(), DaemonEvent::CommandFinished { command_id: finished, result: CommandValue::Ok, .. } if *finished == command_id)));
}

async fn promised(daemon: &flotilla_core::in_process::InProcessDaemon, convoy: &str, promise_id: &str) -> bool {
    let convoy = daemon.resource_backend().using::<Convoy>("flotilla").get(convoy).await.expect("convoy");
    flotilla_resources::promises::owned(convoy.status.as_ref().expect("status"), "work", "coder")
        .iter()
        .any(|promise| promise.id == promise_id)
}

const OWN: PlacedCrew<'static> = PlacedCrew { convoy: "own", vessel: "own-vessel", session: "own-session", crew: "own-crew" };
const SECOND: PlacedCrew<'static> =
    PlacedCrew { convoy: "second", vessel: "second-vessel", session: "second-session", crew: "second-crew" };

/// The scenario the mutation check targets: nothing outside the crew
/// allowlist crosses the forwarded socket.
async fn non_crew_requests_refused(fixture: &Fixture, socket: &Path) -> Result<(), String> {
    let session = fixture.connect(socket, &[]).await;
    for (id, request) in [(1, Request::GetStatus), (2, Request::Shutdown)] {
        session.write(Message::Request { id, request }).await.expect("request");
        match read_session_message(&session).await {
            Message::Response { response, .. } if matches!(response.as_ref(), ResponseResult::Err { .. }) => {}
            other => return Err(format!("request {id} was not refused: {other:?}")),
        }
    }
    Ok(())
}

// #3618 acceptance: one crew on an SSH host. Its hook, crew command and
// artifact put reach the controller and are attributed to it; requests that
// are not crew-scoped are refused.
#[tokio::test]
async fn single_crew_acts_through_the_forward() {
    let fixture = Fixture::new(|_| Relay::AsRequested, &[OWN]).await;

    // No claims at all: the single placed crew is the caller, even though the
    // kernel peer of the vessel socket is the controller's own agent client.
    let session = fixture.connect(&fixture.guest, &[]).await;
    let hook = AgentHookEvent::builder()
        .attachable_id(AttachableId::new("forged"))
        .harness(AgentHarness::ClaudeCode)
        .event_type(AgentEventType::Active)
        .terminal(AgentHookTerminalRef { namespace: "flotilla".into(), session_name: OWN.session.into() })
        .build();
    assert!(matches!(ok_response(request(&session, 1, Request::AgentHook { event: hook }).await, 1), Response::AgentHook));
    promise(&session, 2, "own-promise").await;
    assert!(promised(&fixture.daemon, OWN.convoy, "own-promise").await);

    let evidence = fixture._home.path().join("evidence.md");
    std::fs::write(&evidence, "# review\n").expect("evidence");
    let artifact = request(
        &session,
        3,
        Request::ArtifactPut {
            kind: "review-round".into(),
            subject: "round-1".into(),
            summary: Default::default(),
            media_type: "text/markdown".into(),
            source_path: evidence.clone(),
        },
    )
    .await;
    assert!(matches!(ok_response(artifact, 3), Response::ArtifactPut { .. }));
    let artifacts = fixture.daemon.resource_backend().using::<flotilla_resources::Artifact>("flotilla").list().await.expect("artifacts");
    assert!(artifacts.items.iter().any(|artifact| artifact.spec.convoy == OWN.convoy && artifact.spec.kind == "review-round"));

    non_crew_requests_refused(&fixture, &fixture.guest).await.expect("not crew-scoped requests are refused");
    drop(session);
    fixture.stop().await;
}

// #3618 acceptance: two crews in the one host-wide Environment each act as
// themselves through in-band claims; without claims the caller is ambiguous,
// and a claim naming a crew not placed there is refused.
#[tokio::test]
async fn two_crews_on_one_host_are_distinguished_by_in_band_claims() {
    let fixture = Fixture::new(|_| Relay::AsRequested, &[OWN, SECOND]).await;

    let anonymous = fixture.connect(&fixture.guest, &[]).await;
    assert!(refusal(request(&anonymous, 1, Request::ArtifactList { convoy: None, kind: None, subject: None }).await)
        .starts_with("CallerIdentityAmbiguous:"));

    let own = fixture.connect(&fixture.guest, &[("FLOTILLA_CREW_ID", OWN.crew), ("FLOTILLA_CONVOY", OWN.convoy)]).await;
    let second = fixture.connect(&fixture.guest, &[("FLOTILLA_TERMINAL_SESSION", SECOND.session)]).await;
    promise(&own, 2, "from-own").await;
    promise(&second, 3, "from-second").await;
    assert!(promised(&fixture.daemon, OWN.convoy, "from-own").await);
    assert!(!promised(&fixture.daemon, OWN.convoy, "from-second").await);
    assert!(promised(&fixture.daemon, SECOND.convoy, "from-second").await);
    assert!(!promised(&fixture.daemon, SECOND.convoy, "from-own").await);

    for claims in [[("FLOTILLA_CREW_ID", "foreign-crew")], [("FLOTILLA_CONVOY", "foreign-convoy")]] {
        let foreign = fixture.connect(&fixture.guest, &claims).await;
        assert!(refusal(request(&foreign, 4, Request::ArtifactList { convoy: None, kind: None, subject: None }).await)
            .starts_with("CallerIdentityMismatch:"));
    }
    let _ = &fixture.environment;
    drop((anonymous, own, second));
    fixture.stop().await;
}

// #3618 mutation check: routing the forward to the main socket instead of the
// vessel's environment socket makes the "not crew-scoped is refused" scenario fail.
#[tokio::test]
async fn forward_to_the_main_socket_fails_the_refusal_scenario() {
    let fixture = Fixture::new(|main| Relay::MainSocket(main.to_path_buf()), &[OWN]).await;
    let error = non_crew_requests_refused(&fixture, &fixture.guest).await.expect_err("main socket admits GetStatus");
    assert!(error.contains("request 1 was not refused"), "{error}");
    fixture.stop().await;
}
