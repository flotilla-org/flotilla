use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration as StdDuration,
};

use flotilla_core::{
    agents::AgentEntry,
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, git_process_discovery, init_git_repo_with_remote},
};
use flotilla_protocol::{
    AgentEventType, AgentHarness, AgentHookEvent, AgentStatus, AttachableId, Checkout, CheckoutTarget, Command, CommandAction,
    CommandPeerEvent, CommandValue, ConfigLabel, DaemonEvent, HostName, HostPath, HostSummary, Message, PeerConnectionState, PeerDataKind,
    PeerDataMessage, PeerWireMessage, ProviderData, RepoIdentity, RepoSelector, Request, Response, ResponseResult, RoutedPeerMessage,
    StepAction, StepHost, StepStatus, StreamKey, VectorClock, PROTOCOL_VERSION,
};
use indexmap::IndexMap;
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    sync::{mpsc, oneshot, watch, Mutex, Notify},
    time::Duration,
};

use super::{
    handle_client,
    peer_runtime::{
        disconnect_peer_and_rebuild, forward_with_keepalive_for_test, handle_remote_restart_if_needed, relay_peer_data, send_local_to_peer,
        should_send_local_version, ForwardResult,
    },
    remote_commands::{
        extract_command_repo_identity, ForwardedCommand, ForwardedCommandMap, ForwardedCommandState, PendingRemoteCancelMap,
        PendingRemoteCommand, PendingRemoteCommandMap, RemoteCommandRouter,
    },
    request_dispatch::RequestDispatcher,
    shared::{sync_peer_query_state, write_message, SocketPeerSender},
    DaemonServer, PeerConnectedNotice,
};
use crate::peer::{
    test_support::{ensure_test_connection_generation, handle_test_peer_data, wait_for_command_result, BlockingPeerSender, MockPeerSender},
    InboundPeerEnvelope, PeerManager, PeerSender,
};

fn ok_response(msg: Message, expected_id: u64) -> Response {
    match msg {
        Message::Response { id, response } => {
            assert_eq!(id, expected_id);
            match *response {
                ResponseResult::Ok { response } => *response,
                ResponseResult::Err { message } => panic!("expected ok response, got error: {message}"),
            }
        }
        other => panic!("expected response, got {other:?}"),
    }
}

fn assert_error_response(msg: Message, expected_id: u64, needle: &str) {
    match msg {
        Message::Response { id, response } => {
            assert_eq!(id, expected_id);
            match *response {
                ResponseResult::Err { message } => {
                    assert!(message.contains(needle), "unexpected error payload: {message}");
                }
                other => panic!("expected error response, got {:?}", other),
            }
        }
        other => panic!("expected response, got {other:?}"),
    }
}

async fn empty_daemon() -> (tempfile::TempDir, Arc<InProcessDaemon>) {
    empty_daemon_named("local").await
}

async fn empty_daemon_named(host_name: &str) -> (tempfile::TempDir, Arc<InProcessDaemon>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await;
    (tmp, daemon)
}

type RoutingState = (
    Arc<Mutex<PeerManager>>,
    Arc<Mutex<HashMap<u64, PendingRemoteCommand>>>,
    Arc<Mutex<HashMap<u64, ForwardedCommand>>>,
    Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>,
    Arc<AtomicU64>,
);

fn empty_routing_state() -> RoutingState {
    (
        Arc::new(Mutex::new(PeerManager::new(HostName::new("local")))),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(AtomicU64::new(1 << 62)),
    )
}

fn make_remote_command_router(
    daemon: &Arc<InProcessDaemon>,
    peer_manager: &Arc<Mutex<PeerManager>>,
    pending_remote_commands: &PendingRemoteCommandMap,
    forwarded_commands: &ForwardedCommandMap,
    pending_remote_cancels: &PendingRemoteCancelMap,
    next_remote_command_id: &Arc<AtomicU64>,
) -> RemoteCommandRouter {
    RemoteCommandRouter::new(
        Arc::clone(daemon),
        Arc::clone(peer_manager),
        Arc::clone(pending_remote_commands),
        Arc::clone(forwarded_commands),
        Arc::clone(pending_remote_cancels),
        Arc::clone(next_remote_command_id),
    )
}

fn empty_remote_command_router(daemon: &Arc<InProcessDaemon>, peer_manager: &Arc<Mutex<PeerManager>>) -> RemoteCommandRouter {
    make_remote_command_router(
        daemon,
        peer_manager,
        &Arc::new(Mutex::new(HashMap::new())),
        &Arc::new(Mutex::new(HashMap::new())),
        &Arc::new(Mutex::new(HashMap::new())),
        &Arc::new(AtomicU64::new(1 << 62)),
    )
}

async fn dispatch_request_test(daemon: &Arc<InProcessDaemon>, id: u64, request: Request) -> Message {
    dispatch_request_with_state(daemon, &flotilla_core::agents::shared_in_memory_agent_state_store(), id, request).await
}

async fn dispatch_request_with_state(
    daemon: &Arc<InProcessDaemon>,
    agent_state_store: &flotilla_core::agents::SharedAgentStateStore,
    id: u64,
    request: Request,
) -> Message {
    let (peer_manager, pending_remote_commands, forwarded_commands, pending_remote_cancels, next_remote_command_id) = empty_routing_state();
    let remote_command_router = make_remote_command_router(
        daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    let request_dispatcher = RequestDispatcher::new(daemon, &remote_command_router, agent_state_store);
    request_dispatcher.dispatch(id, request).await
}

fn checkout(branch: &str) -> Checkout {
    Checkout {
        branch: branch.to_string(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
        environment_id: None,
    }
}

fn peer_snapshot(host: &str, repo_identity: &RepoIdentity, repo_path: &Path, checkout_path: &str, branch: &str) -> PeerDataMessage {
    PeerDataMessage {
        origin_host: HostName::new(host),
        repo_identity: repo_identity.clone(),
        repo_path: repo_path.to_path_buf(),
        clock: VectorClock::default(),
        kind: PeerDataKind::Snapshot {
            data: Box::new(ProviderData {
                checkouts: IndexMap::from([(HostPath::new(HostName::new(host), checkout_path), checkout(branch))]),
                ..Default::default()
            }),
            seq: 1,
        },
    }
}

#[tokio::test]
async fn write_message_writes_json_line() {
    let (a, b) = tokio::net::UnixStream::pair().expect("pair");
    let (_read_half, write_half) = a.into_split();
    let writer = tokio::sync::Mutex::new(BufWriter::new(write_half));

    let msg = Message::ok_response(9, Response::Refresh);
    write_message(&writer, &msg).await.expect("write_message");

    let mut lines = BufReader::new(b).lines();
    let line = lines.next_line().await.expect("read line").expect("line");
    let parsed: Message = serde_json::from_str(&line).expect("parse line as message");
    match parsed {
        Message::Response { id, response } => {
            assert_eq!(id, 9);
            assert!(matches!(*response, ResponseResult::Ok { response } if matches!(*response, Response::Refresh)));
        }
        other => panic!("expected response, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_request_handles_error_response_for_untracked_repo() {
    let (_tmp, daemon) = empty_daemon().await;

    let missing_repo = dispatch_request_test(&daemon, 2, Request::GetState { repo: PathBuf::from("/tmp/missing") }).await;
    assert_error_response(missing_repo, 2, "repo not tracked");
}

#[tokio::test]
async fn dispatch_add_list_remove_repo_round_trip() {
    let (tmp, daemon) = empty_daemon().await;
    let repo_path = tmp.path().join("repo-a");
    std::fs::create_dir_all(&repo_path).expect("create repo directory");

    let add = dispatch_request_test(&daemon, 10, Request::AddRepo { path: repo_path.clone() }).await;
    assert!(matches!(ok_response(add, 10), Response::AddRepo));

    let list = dispatch_request_test(&daemon, 11, Request::ListRepos).await;
    let listed = match ok_response(list, 11) {
        Response::ListRepos(repos) => repos,
        other => panic!("expected list repos response, got {:?}", other),
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, repo_path);

    let remove = dispatch_request_test(&daemon, 12, Request::RemoveRepo { path: listed[0].path.clone() }).await;
    assert!(matches!(ok_response(remove, 12), Response::RemoveRepo));
}

#[tokio::test]
async fn dispatch_replay_since_with_empty_last_seen_returns_only_host_snapshots() {
    let (_tmp, daemon) = empty_daemon().await;

    let replay = dispatch_request_test(&daemon, 30, Request::ReplaySince { last_seen: vec![] }).await;
    match ok_response(replay, 30) {
        Response::ReplaySince(events) => {
            // With no repos, we should only get the local HostSnapshot
            let repo_events: Vec<_> =
                events.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_) | DaemonEvent::RepoDelta(_))).collect();
            assert!(repo_events.is_empty(), "should have no repo events");
            let host_events: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::HostSnapshot(_))).collect();
            assert!(!host_events.is_empty(), "should have at least one HostSnapshot for local host");
        }
        other => panic!("expected replay response, got {:?}", other),
    };
}

#[tokio::test]
async fn dispatch_host_query_methods_round_trip() {
    let (_tmp, daemon) = empty_daemon().await;
    let local_host = daemon.host_name().to_string();
    daemon
        .set_topology_routes(vec![flotilla_protocol::TopologyRoute {
            target: HostName::new("remote"),
            next_hop: HostName::new("relay"),
            direct: false,
            connected: true,
            fallbacks: vec![],
        }])
        .await;

    // Host queries now go through execute() via CommandAction::Query* variants.
    // Test the internal methods directly to validate the data path.
    let hosts = daemon.list_hosts_internal().await.expect("list hosts");
    assert!(hosts.hosts.iter().any(|entry| entry.host == *daemon.host_name()));

    let status = daemon.get_host_status_internal(&local_host).await.expect("host status");
    assert!(status.is_local);

    let providers = daemon.get_host_providers_internal(&daemon.host_name().to_string()).await.expect("host providers");
    assert_eq!(providers.summary.host_name, *daemon.host_name());

    // Topology still has a dedicated Request variant.
    let topology = dispatch_request_test(&daemon, 43, Request::GetTopology).await;
    match ok_response(topology, 43) {
        Response::GetTopology(parsed) => {
            assert_eq!(parsed.routes.len(), 1);
            assert_eq!(parsed.routes[0].next_hop, HostName::new("relay"));
        }
        other => panic!("expected topology response, got {:?}", other),
    }
}

#[tokio::test]
async fn dispatch_repo_query_methods_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::new("local")).await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let repo_name = repo.file_name().expect("repo file name").to_string_lossy().to_string();

    // GetStatus still has a dedicated Request variant.
    let status = dispatch_request_test(&daemon, 1, Request::GetStatus).await;
    match ok_response(status, 1) {
        Response::GetStatus(parsed) => assert!(parsed.repos.iter().any(|entry| entry.path == repo)),
        other => panic!("expected status response, got {:?}", other),
    }

    // Repo queries now go through execute() via CommandAction::Query* variants.
    // Test the internal methods directly to validate the data path.
    let detail = daemon.get_repo_detail_internal(&RepoSelector::Query(repo_name.clone())).await.expect("repo detail");
    assert_eq!(detail.path, repo);

    let providers = daemon.get_repo_providers_internal(&RepoSelector::Query(repo_name.clone())).await.expect("repo providers");
    assert_eq!(providers.path, repo);

    let work = daemon.get_repo_work_internal(&RepoSelector::Query(repo_name)).await.expect("repo work");
    assert_eq!(work.path, repo);
}

#[tokio::test]
async fn dispatch_agent_hook_started_updates_existing_session_entry() {
    let (_tmp, daemon) = empty_daemon().await;
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let existing_id = AttachableId::new("att-existing");
    let incoming_id = AttachableId::new("att-new");
    let session_id = "sess-123".to_string();

    {
        let mut store = agent_state_store.lock().expect("lock agent state store");
        store.upsert(existing_id.clone(), AgentEntry {
            harness: AgentHarness::ClaudeCode,
            status: AgentStatus::Idle,
            model: Some("old-model".into()),
            session_title: Some("Existing".into()),
            session_id: Some(session_id.clone()),
            last_event_epoch_secs: 1,
        });
    }

    let response = dispatch_request_with_state(&daemon, &agent_state_store, 5, Request::AgentHook {
        event: AgentHookEvent {
            attachable_id: incoming_id.clone(),
            harness: AgentHarness::ClaudeCode,
            event_type: AgentEventType::Active,
            session_id: Some(session_id.clone()),
            model: Some("new-model".into()),
            cwd: None,
        },
    })
    .await;

    assert!(matches!(ok_response(response, 5), Response::AgentHook));
    let store = agent_state_store.lock().expect("lock agent state store");
    assert_eq!(store.lookup_by_session_id(&session_id), Some(&existing_id));
    let entry = store.get(&existing_id).expect("existing entry should be updated");
    assert_eq!(entry.status, AgentStatus::Active);
    assert_eq!(entry.model.as_deref(), Some("new-model"));
    assert!(store.get(&incoming_id).is_none(), "session remap should update the existing attachable id");
}

#[tokio::test]
async fn dispatch_agent_hook_ended_removes_existing_session_entry() {
    let (_tmp, daemon) = empty_daemon().await;
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let existing_id = AttachableId::new("att-existing");
    let session_id = "sess-456".to_string();

    {
        let mut store = agent_state_store.lock().expect("lock agent state store");
        store.upsert(existing_id.clone(), AgentEntry {
            harness: AgentHarness::ClaudeCode,
            status: AgentStatus::Active,
            model: Some("opus".into()),
            session_title: Some("Existing".into()),
            session_id: Some(session_id.clone()),
            last_event_epoch_secs: 1,
        });
    }

    let response = dispatch_request_with_state(&daemon, &agent_state_store, 6, Request::AgentHook {
        event: AgentHookEvent {
            attachable_id: AttachableId::new("att-ended"),
            harness: AgentHarness::ClaudeCode,
            event_type: AgentEventType::Ended,
            session_id: Some(session_id.clone()),
            model: None,
            cwd: None,
        },
    })
    .await;

    assert!(matches!(ok_response(response, 6), Response::AgentHook));
    let store = agent_state_store.lock().expect("lock agent state store");
    assert!(store.lookup_by_session_id(&session_id).is_none());
    assert!(store.get(&existing_id).is_none(), "ended event should remove existing session-mapped entry");
}

#[tokio::test]
async fn dispatch_agent_hook_no_change_event_is_ok_without_creating_entry() {
    let (_tmp, daemon) = empty_daemon().await;
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let attachable_id = AttachableId::new("att-no-change");

    let response = dispatch_request_with_state(&daemon, &agent_state_store, 7, Request::AgentHook {
        event: AgentHookEvent {
            attachable_id: attachable_id.clone(),
            harness: AgentHarness::ClaudeCode,
            event_type: AgentEventType::NoChange,
            session_id: Some("sess-no-change".into()),
            model: None,
            cwd: None,
        },
    })
    .await;

    assert!(matches!(ok_response(response, 7), Response::AgentHook));
    let store = agent_state_store.lock().expect("lock agent state store");
    assert!(store.get(&attachable_id).is_none(), "no-change event should not create a new entry");
}

#[tokio::test]
async fn sync_peer_query_state_mirrors_host_summaries_and_routes_into_daemon() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));

    {
        let mut pm = peer_manager.lock().await;
        pm.store_host_summary(flotilla_protocol::HostSummary {
            host_name: HostName::new("remote"),
            system: flotilla_protocol::SystemInfo {
                home_dir: Some(PathBuf::from("/home/remote")),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(4),
                memory_total_mb: Some(8192),
                environment: flotilla_protocol::HostEnvironment::Container,
            },
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        });

        ensure_test_connection_generation(&mut pm, &HostName::new("remote"), MockPeerSender::discard);
    }

    sync_peer_query_state(&peer_manager, &daemon).await;

    let hosts = daemon.list_hosts_internal().await.expect("list hosts after sync");
    assert!(hosts.hosts.iter().any(|entry| entry.host == HostName::new("remote") && entry.has_summary));

    let topology = daemon.get_topology().await.expect("topology after sync");
    assert!(topology.routes.iter().any(|route| route.target == HostName::new("remote") && route.next_hop == HostName::new("remote")));
}

#[tokio::test]
async fn dispatch_request_execute_remote_routes_command_through_peer_manager() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    let request_dispatcher = RequestDispatcher::new(&daemon, &remote_command_router, &agent_state_store);

    let response = request_dispatcher
        .dispatch(40, Request::Execute {
            command: Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "feta".into() },
            },
        })
        .await;

    let command_id = match ok_response(response, 40) {
        Response::Execute { command_id } => command_id,
        other => panic!("expected execute response, got {:?}", other),
    };

    assert!(command_id >= (1 << 62));
    assert_eq!(pending_remote_commands.lock().await.len(), 1);

    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { requester_host, target_host, command, .. }) => {
            assert_eq!(requester_host, daemon.host_name());
            assert_eq!(target_host, &HostName::new("feta"));
            assert_eq!(command.as_ref(), &Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "feta".into() }
            });
        }
        other => panic!("expected routed command request, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_command_query_requests_still_forward_whole_command() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    let request_dispatcher = RequestDispatcher::new(&daemon, &remote_command_router, &agent_state_store);

    let response = request_dispatcher
        .dispatch(401, Request::Execute {
            command: Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "feta".into() },
            },
        })
        .await;

    let command_id = match ok_response(response, 401) {
        Response::Execute { command_id } => command_id,
        other => panic!("expected execute response, got {:?}", other),
    };

    assert!(command_id >= (1 << 62));
    assert_eq!(pending_remote_commands.lock().await.len(), 1);

    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest { requester_host, target_host, command, .. }) => {
            assert_eq!(requester_host, daemon.host_name());
            assert_eq!(target_host, &HostName::new("feta"));
            assert_eq!(command.as_ref(), &Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "feta".into() },
            });
        }
        other => panic!("expected routed command request, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_command_mutations_route_remote_step_requests() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let repo_identity = init_git_repo_with_remote(&repo, "git@github.com:owner/repo.git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    let request_dispatcher = RequestDispatcher::new(&daemon, &remote_command_router, &agent_state_store);

    let response = request_dispatcher
        .dispatch(402, Request::Execute {
            command: Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Identity(repo_identity.clone()),
                    target: CheckoutTarget::FreshBranch("feat-remote-step".into()),
                    issue_ids: vec![("github".into(), "123".into())],
                },
            },
        })
        .await;

    let command_id = match ok_response(response, 402) {
        Response::Execute { command_id } => command_id,
        other => panic!("expected execute response, got {:?}", other),
    };
    assert!(command_id > 0);

    let routed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(msg) = sent.lock().expect("lock").iter().find_map(|msg| match msg {
                PeerWireMessage::Routed(msg) => Some(msg.clone()),
                _ => None,
            }) {
                return msg;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for routed message");

    match routed {
        RoutedPeerMessage::RemoteStepRequest {
            requester_host,
            target_host,
            repo_identity: identity,
            repo_path,
            step_offset,
            steps,
            ..
        } => {
            assert_eq!(requester_host, HostName::new("local"));
            assert_eq!(target_host, HostName::new("feta"));
            assert_eq!(identity, repo_identity);
            assert_eq!(repo_path, repo);
            assert_eq!(step_offset, 0);
            assert_eq!(steps.len(), 2, "checkout with issue links should batch both remote steps");
            assert!(steps.iter().all(|step| step.host == StepHost::Remote(HostName::new("feta"))));
            assert!(matches!(
                steps[0].action,
                StepAction::CreateCheckout {
                    ref branch,
                    create_branch: true,
                    ..
                } if branch == "feat-remote-step"
            ));
            assert!(matches!(steps[1].action, StepAction::LinkIssuesToBranch { .. }));
        }
        other => panic!("expected remote step request, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_command_remote_step_events_remap_to_presentation_command_id_and_global_indices() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let repo_identity = init_git_repo_with_remote(&repo, "git@github.com:owner/repo.git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let mut rx = daemon.subscribe();
    let command_id = remote_command_router
        .dispatch_execute(Command {
            host: Some(HostName::new("feta")),
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Identity(repo_identity.clone()),
                target: CheckoutTarget::FreshBranch("feat-remap".into()),
                issue_ids: vec![("github".into(), "321".into())],
            },
        })
        .await
        .expect("dispatch execute");

    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) = sent.lock().expect("lock").iter().find_map(|msg| match msg {
                PeerWireMessage::Routed(RoutedPeerMessage::RemoteStepRequest { request_id, .. }) => Some(*request_id),
                _ => None,
            }) {
                return request_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for remote step request");

    remote_command_router
        .emit_remote_step_event(
            request_id,
            HostName::new("feta"),
            0,
            2,
            "Create checkout for branch feat-remap".into(),
            StepStatus::Started,
        )
        .await;
    remote_command_router
        .emit_remote_step_event(
            request_id,
            HostName::new("feta"),
            0,
            2,
            "Create checkout for branch feat-remap".into(),
            StepStatus::Succeeded,
        )
        .await;
    remote_command_router
        .emit_remote_step_event(request_id, HostName::new("feta"), 1, 2, "Link issues to branch".into(), StepStatus::Started)
        .await;
    remote_command_router
        .emit_remote_step_event(request_id, HostName::new("feta"), 1, 2, "Link issues to branch".into(), StepStatus::Succeeded)
        .await;
    remote_command_router.complete_remote_step(request_id, HostName::new("feta"), vec![]).await;

    let observed: Vec<_> = tokio::time::timeout(Duration::from_secs(2), async {
        let mut events = Vec::new();
        while events.len() < 4 {
            match rx.recv().await.expect("broadcast channel should stay open") {
                DaemonEvent::CommandStepUpdate { command_id: id, host, step_index, step_count, description, status, .. }
                    if id == command_id && host == HostName::new("feta") =>
                {
                    events.push((step_index, step_count, description, status));
                }
                _ => {}
            }
        }
        events
    })
    .await
    .expect("timeout waiting for remapped step updates");

    assert_eq!(observed, vec![
        (0, 3, "Create checkout for branch feat-remap".into(), StepStatus::Started),
        (0, 3, "Create checkout for branch feat-remap".into(), StepStatus::Succeeded),
        (1, 3, "Link issues to branch".into(), StepStatus::Started),
        (1, 3, "Link issues to branch".into(), StepStatus::Succeeded),
    ]);
}

#[tokio::test]
async fn dispatch_request_execute_remote_does_not_hold_peer_manager_lock_across_send() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> =
        Arc::new(BlockingPeerSender { started: Arc::clone(&started), release: Arc::clone(&release), sent: Arc::clone(&sent) });
    peer_manager.lock().await.register_sender(HostName::new("feta"), sender);

    let daemon_for_task = Arc::clone(&daemon);
    let peer_manager_for_task = Arc::clone(&peer_manager);
    let pending_remote_commands_for_task = Arc::clone(&pending_remote_commands);
    let forwarded_commands_for_task = Arc::clone(&forwarded_commands);
    let pending_remote_cancels_for_task = Arc::clone(&pending_remote_cancels);
    let next_remote_command_id_for_task = Arc::clone(&next_remote_command_id);
    let dispatch_task = tokio::spawn(async move {
        let agent_state_store = flotilla_core::agents::shared_in_memory_agent_state_store();
        let remote_command_router = make_remote_command_router(
            &daemon_for_task,
            &peer_manager_for_task,
            &pending_remote_commands_for_task,
            &forwarded_commands_for_task,
            &pending_remote_cancels_for_task,
            &next_remote_command_id_for_task,
        );
        let request_dispatcher = RequestDispatcher::new(&daemon_for_task, &remote_command_router, &agent_state_store);
        request_dispatcher
            .dispatch(140, Request::Execute {
                command: Command {
                    host: Some(HostName::new("feta")),
                    context_repo: None,
                    action: CommandAction::QueryHostStatus { target_host: "feta".into() },
                },
            })
            .await
    });

    started.notified().await;
    let _guard = tokio::time::timeout(Duration::from_millis(100), peer_manager.lock())
        .await
        .expect("peer manager lock should remain available while remote command send is blocked");

    release.notify_waiters();
    let response = dispatch_task.await.expect("dispatch task");
    assert!(matches!(ok_response(response, 140), Response::Execute { .. }));
}

#[tokio::test]
async fn dispatch_request_cancel_remote_routes_cancel_and_waits_for_reply() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));

    pending_remote_commands.lock().await.insert(91, PendingRemoteCommand {
        command_id: 1u64 << 62,
        target_host: HostName::new("feta"),
        repo_identity: None,
        repo: None,
        finished_via_event: false,
    });

    let daemon_for_task = Arc::clone(&daemon);
    let peer_manager_for_task = Arc::clone(&peer_manager);
    let pending_remote_commands_for_task = Arc::clone(&pending_remote_commands);
    let forwarded_commands_for_task = Arc::clone(&forwarded_commands);
    let pending_remote_cancels_for_task = Arc::clone(&pending_remote_cancels);
    let next_remote_command_id_for_task = Arc::clone(&next_remote_command_id);
    let agent_state_store_for_task = flotilla_core::agents::shared_in_memory_agent_state_store();
    let response = tokio::spawn(async move {
        let remote_command_router = make_remote_command_router(
            &daemon_for_task,
            &peer_manager_for_task,
            &pending_remote_commands_for_task,
            &forwarded_commands_for_task,
            &pending_remote_cancels_for_task,
            &next_remote_command_id_for_task,
        );
        let request_dispatcher = RequestDispatcher::new(&daemon_for_task, &remote_command_router, &agent_state_store_for_task);
        request_dispatcher.dispatch(41, Request::Cancel { command_id: 1u64 << 62 }).await
    });

    let cancel_id = tokio::time::timeout(StdDuration::from_secs(2), async {
        loop {
            let cancel_id = {
                let sent = sent.lock().expect("lock");
                sent.iter().find_map(|msg| match msg {
                    PeerWireMessage::Routed(RoutedPeerMessage::CommandCancelRequest {
                        cancel_id,
                        requester_host,
                        target_host,
                        command_request_id,
                        ..
                    }) => {
                        assert_eq!(requester_host, daemon.host_name());
                        assert_eq!(target_host, &HostName::new("feta"));
                        assert_eq!(*command_request_id, 91);
                        Some(*cancel_id)
                    }
                    _ => None,
                })
            };
            if let Some(cancel_id) = cancel_id {
                if pending_remote_cancels.lock().await.contains_key(&cancel_id) {
                    return cancel_id;
                }
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for routed cancel request");

    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    remote_command_router.complete_remote_cancel(cancel_id, None).await;

    assert!(matches!(ok_response(response.await.expect("cancel task"), 41), Response::Cancel));
}

#[tokio::test]
async fn cancel_active_remote_segment_routes_remote_step_cancel_and_finishes_command_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let repo_identity = init_git_repo_with_remote(&repo, "git@github.com:owner/repo.git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let mut rx = daemon.subscribe();
    let command_id = remote_command_router
        .dispatch_execute(Command {
            host: Some(HostName::new("feta")),
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Identity(repo_identity.clone()),
                target: CheckoutTarget::FreshBranch("feat-cancel-active-remote".into()),
                issue_ids: vec![("github".into(), "123".into())],
            },
        })
        .await
        .expect("dispatch execute");

    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) = sent.lock().expect("lock").iter().find_map(|msg| match msg {
                PeerWireMessage::Routed(RoutedPeerMessage::RemoteStepRequest { request_id, .. }) => Some(*request_id),
                _ => None,
            }) {
                return request_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for remote step request");

    let daemon_for_task = Arc::clone(&daemon);
    let peer_manager_for_task = Arc::clone(&peer_manager);
    let pending_remote_commands_for_task = Arc::clone(&pending_remote_commands);
    let forwarded_commands_for_task = Arc::clone(&forwarded_commands);
    let pending_remote_cancels_for_task = Arc::clone(&pending_remote_cancels);
    let next_remote_command_id_for_task = Arc::clone(&next_remote_command_id);
    let agent_state_store_for_task = flotilla_core::agents::shared_in_memory_agent_state_store();
    let cancel_response = tokio::spawn(async move {
        let remote_command_router = make_remote_command_router(
            &daemon_for_task,
            &peer_manager_for_task,
            &pending_remote_commands_for_task,
            &forwarded_commands_for_task,
            &pending_remote_cancels_for_task,
            &next_remote_command_id_for_task,
        );
        let request_dispatcher = RequestDispatcher::new(&daemon_for_task, &remote_command_router, &agent_state_store_for_task);
        request_dispatcher.dispatch(403, Request::Cancel { command_id }).await
    });

    let cancel_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(cancel_id) = sent.lock().expect("lock").iter().find_map(|msg| match msg {
                PeerWireMessage::Routed(RoutedPeerMessage::RemoteStepCancelRequest { cancel_id, remote_step_request_id, .. })
                    if *remote_step_request_id == request_id =>
                {
                    Some(*cancel_id)
                }
                PeerWireMessage::Routed(RoutedPeerMessage::CommandCancelRequest { .. }) => {
                    panic!("whole-command cancel should not be routed for an active remote step batch");
                }
                _ => None,
            }) {
                return cancel_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for remote step cancel request");

    remote_command_router.complete_remote_step_cancel(cancel_id, None).await;
    assert!(matches!(ok_response(cancel_response.await.expect("cancel task"), 403), Response::Cancel));

    remote_command_router.complete_remote_step(request_id, HostName::new("feta"), vec![]).await;

    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("broadcast channel should stay open") {
                DaemonEvent::CommandFinished { command_id: id, result, .. } if id == command_id => return result,
                DaemonEvent::CommandStepUpdate { command_id: id, description, status, .. }
                    if id == command_id && description == "Create workspace" =>
                {
                    panic!("local workspace step should not run after cancellation, saw {status:?}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timeout waiting for cancelled command to finish");

    assert_eq!(finished, CommandValue::Cancelled);
}

#[tokio::test]
async fn cancel_disconnect_of_active_remote_segment_finishes_pending_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let repo_identity = init_git_repo_with_remote(&repo, "git@github.com:owner/repo.git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("feta"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let mut rx = daemon.subscribe();
    let command_id = remote_command_router
        .dispatch_execute(Command {
            host: Some(HostName::new("feta")),
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Identity(repo_identity.clone()),
                target: CheckoutTarget::FreshBranch("feat-cancel-disconnect".into()),
                issue_ids: vec![],
            },
        })
        .await
        .expect("dispatch execute");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if sent.lock().expect("lock").iter().any(|msg| {
                matches!(msg, PeerWireMessage::Routed(RoutedPeerMessage::RemoteStepRequest { target_host, .. }) if *target_host == HostName::new("feta"))
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for remote step request");

    remote_command_router.fail_pending_remote_steps_for_host(&HostName::new("feta")).await;

    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("broadcast channel should stay open") {
                DaemonEvent::CommandFinished { command_id: id, result, .. } if id == command_id => return result,
                _ => {}
            }
        }
    })
    .await
    .expect("timeout waiting for disconnected command to finish");

    match finished {
        CommandValue::Error { message } => {
            assert!(message.contains("disconnected"), "unexpected disconnect error: {message}");
        }
        other => panic!("expected disconnect error, got {other:?}"),
    }
}

#[tokio::test]
async fn handle_inbound_command_request_does_not_hold_peer_manager_lock_across_send() {
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> =
        Arc::new(BlockingPeerSender { started: Arc::clone(&started), release: Arc::clone(&release), sent: Arc::clone(&sent) });

    {
        let mut pm = peer_manager.lock().await;
        pm.register_sender(HostName::new("relay"), sender);
    }

    let handle_task = tokio::spawn({
        let peer_manager = Arc::clone(&peer_manager);
        async move {
            let mut pm = peer_manager.lock().await;
            let connection_peer = HostName::new("desktop");
            let generation = ensure_test_connection_generation(&mut pm, &connection_peer, MockPeerSender::discard);
            let _ = pm
                .handle_inbound(InboundPeerEnvelope {
                    msg: PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest {
                        request_id: 7,
                        requester_host: HostName::new("desktop"),
                        target_host: HostName::new("relay"),
                        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                        command: Box::new(Command {
                            host: Some(HostName::new("relay")),
                            context_repo: None,
                            action: CommandAction::Refresh { repo: None },
                        }),
                    }),
                    connection_generation: generation,
                    connection_peer,
                })
                .await;
            let pending_sends = pm.take_pending_sends();
            drop(pm);
            crate::peer::dispatch_pending_sends(pending_sends).await;
        }
    });

    started.notified().await;
    let _guard = tokio::time::timeout(Duration::from_millis(100), peer_manager.lock())
        .await
        .expect("peer manager lock should remain available while routed send is blocked");

    release.notify_waiters();
    handle_task.await.expect("handle task should finish");
}

#[test]
fn extract_command_repo_identity_uses_context_repo_for_prepare_terminal() {
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() };
    let command = Command {
        host: Some(HostName::new("remote")),
        context_repo: Some(RepoSelector::Identity(identity.clone())),
        action: CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/tmp/repo.checkout"), commands: vec![] },
    };

    assert_eq!(extract_command_repo_identity(&command), Some(identity));
}

#[tokio::test]
async fn cancel_forwarded_command_waits_for_launching_registration() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(77, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });

    let router_for_task = remote_command_router.clone();
    let handle = tokio::spawn(async move {
        router_for_task.cancel_forwarded_command_for_test(11, HostName::new("desktop"), HostName::new("relay"), 77).await;
    });

    tokio::time::sleep(StdDuration::from_millis(50)).await;
    assert!(sent.lock().expect("lock").is_empty(), "cancel should wait for launch registration");

    if let Some(entry) = forwarded_commands.lock().await.get_mut(&77) {
        entry.state = ForwardedCommandState::Running { command_id: 123 };
    }
    ready.notify_waiters();

    handle.await.expect("cancel task");

    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Routed(RoutedPeerMessage::CommandCancelResponse { cancel_id, requester_host, responder_host, error, .. }) => {
            assert_eq!(*cancel_id, 11);
            assert_eq!(requester_host, &HostName::new("desktop"));
            assert_eq!(responder_host, daemon.host_name());
            assert_eq!(error.as_deref(), Some("no matching active command"));
        }
        other => panic!("expected routed command cancel response, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_forwarded_command_proxies_lifecycle_and_response() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::new("local")).await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(7, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
    remote_command_router
        .execute_forwarded_command_for_test(
            7,
            HostName::new("desktop"),
            HostName::new("relay"),
            Command { host: Some(daemon.host_name().clone()), context_repo: None, action: CommandAction::Refresh { repo: None } },
            ready,
        )
        .await;

    {
        let sent = sent.lock().expect("lock");
        assert!(sent.len() >= 3, "expected started event, finished event, and response");

        let mut saw_started = false;
        let mut saw_finished = false;
        let mut saw_response = false;

        for msg in sent.iter() {
            match msg {
                PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent { request_id, requester_host, responder_host, event, .. }) => {
                    assert_eq!(*request_id, 7);
                    assert_eq!(requester_host, &HostName::new("desktop"));
                    assert_eq!(responder_host, daemon.host_name());
                    match event.as_ref() {
                        CommandPeerEvent::Started { repo: event_repo, description, .. } => {
                            assert_eq!(event_repo, &repo);
                            assert_eq!(description, "Refreshing...");
                            saw_started = true;
                        }
                        CommandPeerEvent::Finished { repo: event_repo, result, .. } => {
                            assert_eq!(event_repo, &repo);
                            assert_eq!(result, &CommandValue::Refreshed { repos: vec![repo.clone()] });
                            saw_finished = true;
                        }
                        CommandPeerEvent::StepUpdate { .. } => {}
                    }
                }
                PeerWireMessage::Routed(RoutedPeerMessage::CommandResponse {
                    request_id, requester_host, responder_host, result, ..
                }) => {
                    assert_eq!(*request_id, 7);
                    assert_eq!(requester_host, &HostName::new("desktop"));
                    assert_eq!(responder_host, daemon.host_name());
                    assert_eq!(result.as_ref(), &CommandValue::Refreshed { repos: vec![repo.clone()] });
                    saw_response = true;
                }
                other => panic!("unexpected proxied message: {other:?}"),
            }
        }

        assert!(saw_started);
        assert!(saw_finished);
        assert!(saw_response);
    }
    assert!(forwarded_commands.lock().await.is_empty(), "forwarded command should be retired after completion");
}

#[tokio::test]
async fn execute_forwarded_prepare_terminal_returns_terminal_prepared() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("remote-root").join("repo");
    let repo_identity = init_git_repo_with_remote(&repo, "git@github.com:owner/repo.git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&flotilla_protocol::RepoSelector::Path(repo.clone())).await.expect("refresh repo");

    let mut setup_rx = daemon.subscribe();
    let checkout_id = daemon
        .execute(Command {
            host: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Identity(repo_identity.clone()),
                target: CheckoutTarget::FreshBranch("feat-remote".into()),
                issue_ids: vec![],
            },
        })
        .await
        .expect("dispatch checkout");
    let checkout_result = wait_for_command_result(&mut setup_rx, checkout_id, StdDuration::from_secs(5)).await;
    match checkout_result {
        CommandValue::CheckoutCreated { branch, path } => {
            assert_eq!(branch, "feat-remote");
            assert!(path.ends_with("repo.feat-remote"), "unexpected checkout path: {}", path.display());
        }
        other => panic!("expected checkout creation, got {other:?}"),
    };
    let checkout_path = tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            let snapshot = daemon.get_state(&flotilla_protocol::RepoSelector::Path(repo.clone())).await.expect("get state");
            if let Some((path, _checkout)) = snapshot.providers.checkouts.iter().find(|(_, checkout)| checkout.branch == "feat-remote") {
                return path.path.clone();
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for checkout path from state");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(8, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
    remote_command_router
        .execute_forwarded_command_for_test(
            8,
            HostName::new("desktop"),
            HostName::new("relay"),
            Command {
                host: Some(daemon.host_name().clone()),
                context_repo: Some(RepoSelector::Identity(repo_identity.clone())),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path: checkout_path.clone(), commands: vec![] },
            },
            ready,
        )
        .await;

    let sent = sent.lock().expect("lock");
    let mut saw_preparing = false;
    let mut saw_finished = false;
    let mut saw_response = false;

    for msg in sent.iter() {
        match msg {
            PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent { request_id, requester_host, responder_host, event, .. }) => {
                assert_eq!(*request_id, 8);
                assert_eq!(requester_host, &HostName::new("desktop"));
                assert_eq!(responder_host, daemon.host_name());
                match event.as_ref() {
                    CommandPeerEvent::Started { repo_identity: event_identity, repo: event_repo, description } => {
                        assert_eq!(event_identity, &repo_identity);
                        assert_eq!(event_repo, &repo);
                        assert_eq!(description, "Preparing terminal...");
                        saw_preparing = true;
                    }
                    CommandPeerEvent::Finished { repo_identity: event_identity, repo: event_repo, result } => {
                        assert_eq!(event_identity, &repo_identity);
                        assert_eq!(event_repo, &repo);
                        match result {
                            CommandValue::TerminalPrepared {
                                repo_identity: result_identity,
                                target_host,
                                branch,
                                checkout_path: returned_path,
                                attachable_set_id,
                                commands,
                            } => {
                                assert_eq!(result_identity, &repo_identity);
                                assert_eq!(target_host, daemon.host_name());
                                assert_eq!(branch, "feat-remote");
                                assert_eq!(returned_path, &checkout_path);
                                assert!(attachable_set_id.is_some(), "prepared terminal should include an attachable set id");
                                assert!(!commands.is_empty(), "prepared terminal should include commands");
                            }
                            other => panic!("expected TerminalPrepared finish event, got {other:?}"),
                        }
                        saw_finished = true;
                    }
                    CommandPeerEvent::StepUpdate { repo_identity: event_identity, .. } => {
                        assert_eq!(event_identity, &repo_identity);
                    }
                }
            }
            PeerWireMessage::Routed(RoutedPeerMessage::CommandResponse { request_id, requester_host, responder_host, result, .. }) => {
                assert_eq!(*request_id, 8);
                assert_eq!(requester_host, &HostName::new("desktop"));
                assert_eq!(responder_host, daemon.host_name());
                match result.as_ref() {
                    CommandValue::TerminalPrepared {
                        repo_identity: result_identity,
                        target_host,
                        branch,
                        checkout_path: returned_path,
                        attachable_set_id,
                        commands,
                    } => {
                        assert_eq!(result_identity, &repo_identity);
                        assert_eq!(target_host, daemon.host_name());
                        assert_eq!(branch, "feat-remote");
                        assert_eq!(returned_path, &checkout_path);
                        assert!(attachable_set_id.is_some(), "prepared terminal response should include an attachable set id");
                        assert!(!commands.is_empty(), "prepared terminal should include commands");
                    }
                    other => panic!("expected TerminalPrepared response, got {other:?}"),
                }
                saw_response = true;
            }
            other => panic!("unexpected proxied message: {other:?}"),
        }
    }

    assert!(saw_preparing);
    assert!(saw_finished);
    assert!(saw_response);
}

#[tokio::test]
async fn execute_forwarded_checkout_resolves_repo_identity_across_different_roots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote_repo = tmp.path().join("remote-root").join("repo");
    let requester_repo = tmp.path().join("requester-root").join("repo");
    let repo_identity = init_git_repo_with_remote(&remote_repo, "git@github.com:owner/repo.git");
    init_git_repo_with_remote(&requester_repo, "git@github.com:owner/repo.git");

    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![remote_repo.clone()], config, git_process_discovery(false), HostName::new("local")).await;
    daemon.refresh(&flotilla_protocol::RepoSelector::Path(remote_repo.clone())).await.expect("refresh repo");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );

    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(9, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });
    remote_command_router
        .execute_forwarded_command_for_test(
            9,
            HostName::new("desktop"),
            HostName::new("relay"),
            Command {
                host: Some(daemon.host_name().clone()),
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Identity(repo_identity.clone()),
                    target: CheckoutTarget::FreshBranch("feat-routed".into()),
                    issue_ids: vec![],
                },
            },
            ready,
        )
        .await;

    let sent = sent.lock().expect("lock");
    assert!(sent.iter().any(|msg| matches!(
        msg,
        PeerWireMessage::Routed(RoutedPeerMessage::CommandResponse { result, .. })
            if matches!(result.as_ref(), CommandValue::CheckoutCreated { branch, .. } if branch == "feat-routed")
    )));
}

#[tokio::test]
async fn take_peer_data_rx_returns_some_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let mut server = DaemonServer::new(vec![], config, fake_discovery(false), tmp.path().join("test.sock"), Duration::from_secs(60))
        .await
        .expect("create daemon server");

    assert!(server.take_peer_data_rx().is_some(), "first call should return Some");
    assert!(server.take_peer_data_rx().is_none(), "second call should return None");
}

#[tokio::test]
async fn daemon_server_replays_configured_hosts_as_disconnected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("config");
    std::fs::create_dir_all(&base).expect("create config directory");
    std::fs::write(base.join("daemon.toml"), "host_name = \"local\"\n").expect("write daemon config");
    std::fs::write(
            base.join("hosts.toml"),
            "[hosts.udder]\nhostname = \"udder\"\ndaemon_socket = \"/tmp/udder.sock\"\n\n[hosts.feta]\nhostname = \"feta\"\ndaemon_socket = \"/tmp/feta.sock\"\n",
        )
        .expect("write hosts config");

    let config = Arc::new(ConfigStore::with_base(&base));
    let server = DaemonServer::new(vec![], config, fake_discovery(false), tmp.path().join("test.sock"), Duration::from_secs(60))
        .await
        .expect("create daemon server");

    let events = server.daemon.replay_since(&HashMap::new()).await.expect("replay events");
    let mut statuses: Vec<(HostName, PeerConnectionState)> = events
        .into_iter()
        .filter_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) => Some((snap.host_name.clone(), snap.connection_status.clone())),
            _ => None,
        })
        .collect();
    // Filter out the local host entry — we only care about configured peers
    statuses.retain(|(host, _)| host != server.daemon.host_name());
    statuses.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(statuses, vec![
        (HostName::new("feta"), PeerConnectionState::Disconnected),
        (HostName::new("udder"), PeerConnectionState::Disconnected),
    ]);
}

fn test_peer_msg(host: &str) -> PeerDataMessage {
    PeerDataMessage {
        origin_host: HostName::new(host),
        repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repo_path: PathBuf::from("/tmp/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::RequestResync { since_seq: 0 },
    }
}

fn host_seq_for(events: &[DaemonEvent], host_name: &HostName) -> Option<u64> {
    events.iter().find_map(|event| match event {
        DaemonEvent::HostSnapshot(snap) if snap.host_name == *host_name => Some(snap.seq),
        _ => None,
    })
}

#[tokio::test]
async fn handle_client_forwards_peer_data_and_registers_peer() {
    let (_tmp, daemon) = empty_daemon().await;
    let expected_local_host = daemon.host_name().clone();
    let daemon_events = daemon.subscribe();
    let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

    // Spawn handle_client on the server side
    let pm = Arc::clone(&peer_manager);
    let count_ref = Arc::clone(&client_count);
    let notify_ref = Arc::clone(&client_notify);
    let daemon_for_task = Arc::clone(&daemon);
    let handle = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon_for_task, &pm);
        handle_client(
            server_stream,
            daemon_for_task,
            shutdown_rx,
            peer_data_tx,
            pm,
            remote_command_router,
            count_ref,
            notify_ref,
            peer_connected_tx,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });

    let (read_half, write_half) = client_stream.into_split();
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);

    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_name: HostName::new("remote-host"),
        session_id: uuid::Uuid::nil(),
        environment_id: None,
    };
    flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

    let line = reader.next_line().await.expect("read hello response").expect("hello line");
    let hello_back: Message = serde_json::from_str(&line).expect("parse hello");
    match hello_back {
        Message::Hello { protocol_version, host_name, .. } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(host_name, expected_local_host);
        }
        other => panic!("expected hello response, got {other:?}"),
    }

    let mut daemon_events = daemon_events;

    // Wait for the PeerStatusChanged(Connected) event, draining any HostSnapshot events
    let connected_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match daemon_events.recv().await.expect("recv") {
                DaemonEvent::PeerStatusChanged { host, status } => break (host, status),
                DaemonEvent::HostSnapshot(_) => continue,
                other => panic!("expected peer status or host snapshot event, got {other:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for peer status");
    assert_eq!(connected_event.0, HostName::new("remote-host"));
    assert_eq!(connected_event.1, PeerConnectionState::Connected);

    // Send a peer message from the client side
    let peer_msg = test_peer_msg("remote-host");
    let wire_msg = Message::Peer(Box::new(PeerWireMessage::Data(peer_msg.clone())));
    flotilla_protocol::framing::write_message_line(&mut writer, &wire_msg).await.expect("write peer message");

    // The server should forward the peer envelope
    let received = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
        .await
        .expect("timeout waiting for peer data")
        .expect("channel closed");
    assert_eq!(received.connection_peer, HostName::new("remote-host"));
    assert_eq!(received.connection_generation, 1);
    match received.msg {
        PeerWireMessage::Data(msg) => {
            assert_eq!(msg.origin_host, HostName::new("remote-host"));
        }
        other => panic!("expected data message, got {other:?}"),
    }

    {
        let pm = peer_manager.lock().await;
        assert_eq!(pm.current_generation(&HostName::new("remote-host")), Some(1));
    }
    assert_eq!(client_count.load(Ordering::SeqCst), 1, "active peer connection should suppress idle shutdown");

    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // Wait for the PeerStatusChanged(Disconnected) event, draining any HostSnapshot events
    let disconnected_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match daemon_events.recv().await.expect("recv") {
                DaemonEvent::PeerStatusChanged { host, status } => break (host, status),
                DaemonEvent::HostSnapshot(_) => continue,
                other => panic!("expected peer disconnect or host snapshot event, got {other:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for peer disconnect");
    assert_eq!(disconnected_event.0, HostName::new("remote-host"));
    assert_eq!(disconnected_event.1, PeerConnectionState::Disconnected);
    assert_eq!(client_count.load(Ordering::SeqCst), 0, "peer disconnect should release idle-shutdown accounting");

    let pm = peer_manager.lock().await;
    assert!(pm.current_generation(&HostName::new("remote-host")).is_none(), "peer should be disconnected after socket close");
}

#[tokio::test]
async fn handle_client_does_not_advance_host_cursor_for_duplicate_host_summary() {
    let (_tmp, daemon) = empty_daemon().await;
    let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");
    let daemon_for_task = Arc::clone(&daemon);
    let pm = Arc::clone(&peer_manager);
    let count_ref = Arc::clone(&client_count);
    let notify_ref = Arc::clone(&client_notify);
    let handle = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon_for_task, &pm);
        handle_client(
            server_stream,
            daemon_for_task,
            shutdown_rx,
            peer_data_tx,
            pm,
            remote_command_router,
            count_ref,
            notify_ref,
            peer_connected_tx,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });

    let (read_half, write_half) = client_stream.into_split();
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);
    let remote_host = HostName::new("remote-host");

    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_name: remote_host.clone(),
        session_id: uuid::Uuid::nil(),
        environment_id: None,
    };
    flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");
    let line = reader.next_line().await.expect("read hello response").expect("hello line");
    let hello_back: Message = serde_json::from_str(&line).expect("parse hello");
    assert!(matches!(hello_back, Message::Hello { .. }), "expected hello response");

    let summary = HostSummary {
        host_name: remote_host.clone(),
        system: Default::default(),
        inventory: Default::default(),
        providers: vec![],
        environments: vec![],
    };

    flotilla_protocol::framing::write_message_line(&mut writer, &Message::Peer(Box::new(PeerWireMessage::HostSummary(summary.clone()))))
        .await
        .expect("write first host summary");
    flotilla_protocol::framing::write_message_line(
        &mut writer,
        &Message::Peer(Box::new(PeerWireMessage::Data(test_peer_msg("remote-host")))),
    )
    .await
    .expect("write first barrier");
    let _ = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
        .await
        .expect("timeout waiting for first barrier")
        .expect("first barrier channel closed");

    let initial_replay = daemon.replay_since(&HashMap::new()).await.expect("initial replay");
    let host_seq = host_seq_for(&initial_replay, &remote_host).expect("host snapshot after first summary");

    flotilla_protocol::framing::write_message_line(&mut writer, &Message::Peer(Box::new(PeerWireMessage::HostSummary(summary))))
        .await
        .expect("write duplicate host summary");
    flotilla_protocol::framing::write_message_line(
        &mut writer,
        &Message::Peer(Box::new(PeerWireMessage::Data(test_peer_msg("remote-host")))),
    )
    .await
    .expect("write second barrier");
    let _ = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
        .await
        .expect("timeout waiting for second barrier")
        .expect("second barrier channel closed");

    let replay =
        daemon.replay_since(&HashMap::from([(StreamKey::Host { host_name: remote_host.clone() }, host_seq)])).await.expect("replay_since");
    assert!(host_seq_for(&replay, &remote_host).is_none(), "duplicate host summary should not advance the host cursor");

    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn handle_client_streams_daemon_events_to_request_clients() {
    let (_tmp, daemon) = empty_daemon().await;
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");
    let daemon_for_task = Arc::clone(&daemon);
    let pm = Arc::clone(&peer_manager);
    let count_ref = Arc::clone(&client_count);
    let notify_ref = Arc::clone(&client_notify);
    let handle = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon_for_task, &pm);
        handle_client(
            server_stream,
            daemon_for_task,
            shutdown_rx,
            peer_data_tx,
            pm,
            remote_command_router,
            count_ref,
            notify_ref,
            peer_connected_tx,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });

    let (read_half, write_half) = client_stream.into_split();
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);

    let request = Message::Request { id: 1, request: Request::ListRepos };
    flotilla_protocol::framing::write_message_line(&mut writer, &request).await.expect("write request");

    let response_line = reader.next_line().await.expect("read response").expect("response line");
    let response_msg: Message = serde_json::from_str(&response_line).expect("parse response");
    match ok_response(response_msg, 1) {
        Response::ListRepos(_) => {}
        other => panic!("expected ListRepos response, got {other:?}"),
    }
    assert_eq!(client_count.load(Ordering::SeqCst), 1, "request client should be tracked while connected");

    let repo_identity = RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() };
    let repo = PathBuf::from("/tmp/repo");
    daemon.send_event(DaemonEvent::CommandStarted {
        command_id: 77,
        host: daemon.host_name().clone(),
        repo_identity: repo_identity.clone(),
        repo: repo.clone(),
        description: "streamed event".to_string(),
    });

    let event_line = tokio::time::timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("timeout waiting for event")
        .expect("read event line")
        .expect("event payload");
    let event_msg: Message = serde_json::from_str(&event_line).expect("parse event");
    match event_msg {
        Message::Event { event } => match *event {
            DaemonEvent::CommandStarted { command_id, repo_identity: event_identity, repo: event_repo, description, .. } => {
                assert_eq!(command_id, 77);
                assert_eq!(event_identity, repo_identity);
                assert_eq!(event_repo, repo);
                assert_eq!(description, "streamed event");
            }
            other => panic!("expected CommandStarted event, got {other:?}"),
        },
        other => panic!("expected event message, got {other:?}"),
    }

    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert_eq!(client_count.load(Ordering::SeqCst), 0, "request client should be removed after disconnect");
}

#[tokio::test]
async fn send_local_to_peer_sends_host_summary_for_empty_daemon() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer = HostName::new("remote-host");
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    let generation = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &peer, || Arc::clone(&sender))
    };
    let mut clock = VectorClock::default();
    let host_name = daemon.host_name().clone();

    let sent_any = send_local_to_peer(&daemon, &peer_manager, &host_name, &mut clock, &peer, generation).await;

    assert!(sent_any, "host summary should count as initial peer sync");
    let sent = sent.lock().expect("lock");
    assert!(matches!(&sent[0], PeerWireMessage::HostSummary(summary) if summary.host_name == host_name));
}

#[tokio::test]
async fn forward_with_keepalive_times_out_after_silence() {
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(4);
    let (_inbound_tx, mut inbound_rx) = mpsc::channel(4);
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        forward_with_keepalive_for_test(
            &peer_data_tx,
            &mut inbound_rx,
            &HostName::new("remote-host"),
            1,
            sender,
            Duration::from_millis(10),
            Duration::from_millis(30),
        ),
    )
    .await
    .expect("keepalive task should finish before the outer timeout");
    assert!(matches!(result, ForwardResult::KeepaliveTimeout));
    let sent = sent.lock().expect("lock");
    assert!(sent.iter().any(|msg| matches!(msg, PeerWireMessage::Ping { .. })), "keepalive loop should send ping messages");
}

#[tokio::test]
async fn relay_peer_data_does_not_hold_peer_manager_lock_across_send() {
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("leader"))));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> =
        Arc::new(BlockingPeerSender { started: Arc::clone(&started), release: Arc::clone(&release), sent: Arc::clone(&sent) });

    {
        let mut pm = peer_manager.lock().await;
        pm.register_sender(HostName::new("follower-b"), sender);
    }

    let msg = peer_snapshot(
        "follower-a",
        &RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        Path::new("/tmp/repo"),
        "/tmp/repo",
        "feature",
    );
    let started_wait = started.notified();

    let relay_task = tokio::spawn({
        let peer_manager = Arc::clone(&peer_manager);
        async move {
            relay_peer_data(&peer_manager, &HostName::new("follower-a"), &msg).await;
        }
    });

    started_wait.await;
    let _guard = tokio::time::timeout(Duration::from_millis(100), peer_manager.lock())
        .await
        .expect("peer manager lock should remain available while relay send is blocked");

    release.notify_waiters();
    relay_task.await.expect("relay task should finish");

    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1, "relay should eventually send one message");
}

#[test]
fn should_send_local_version_dedupes_by_repo_identity() {
    let identity = RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() };
    let mut last_sent_versions = HashMap::new();

    assert!(should_send_local_version(&last_sent_versions, &identity, 1));
    last_sent_versions.insert(identity.clone(), 1);

    // Different local roots for the same repo identity should share one dedup entry.
    assert!(!should_send_local_version(&last_sent_versions, &identity, 1));
    assert!(should_send_local_version(&last_sent_versions, &identity, 2));
}

#[tokio::test]
async fn handle_remote_restart_if_needed_clears_stale_remote_only_peer_state() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let repo_identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    let repo_path = PathBuf::from("/srv/remote-only");

    {
        let mut pm = peer_manager.lock().await;
        assert_eq!(
            handle_test_peer_data(
                &mut pm,
                peer_snapshot("peer-a", &repo_identity, &repo_path, "/srv/peer-a/remote-only", "feature-a"),
                || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
            )
            .await,
            crate::peer::HandleResult::Updated(repo_identity.clone())
        );
        assert_eq!(
            handle_test_peer_data(
                &mut pm,
                peer_snapshot("peer-b", &repo_identity, &repo_path, "/srv/peer-b/remote-only", "feature-b"),
                || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
            )
            .await,
            crate::peer::HandleResult::Updated(repo_identity.clone())
        );
        pm.store_host_summary(flotilla_protocol::HostSummary {
            host_name: HostName::new("peer-a"),
            system: flotilla_protocol::SystemInfo {
                home_dir: None,
                os: None,
                arch: None,
                cpu_count: None,
                memory_total_mb: None,
                environment: flotilla_protocol::HostEnvironment::Unknown,
            },
            inventory: Default::default(),
            providers: vec![],
            environments: vec![],
        });
    }

    let synthetic = crate::peer::synthetic_repo_path(&HostName::new("peer-a"), &repo_path);
    daemon
        .add_virtual_repo(
            repo_identity.clone(),
            synthetic.clone(),
            vec![
                (HostName::new("peer-a"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only"), checkout("feature-a"))]),
                    ..Default::default()
                }),
                (HostName::new("peer-b"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only"), checkout("feature-b"))]),
                    ..Default::default()
                }),
            ],
            0,
        )
        .await
        .expect("add virtual repo");
    let old_session_id = uuid::Uuid::new_v4();
    let new_session_id = uuid::Uuid::new_v4();
    {
        let mut pm = peer_manager.lock().await;
        pm.register_remote_repo(repo_identity.clone(), synthetic.clone());
        let peer = HostName::new("peer-a");
        let previous_generation = pm.current_generation(&peer).expect("peer-a should already have an active test connection");
        let second_sender = MockPeerSender::discard();
        match pm.activate_connection_with_session(
            peer.clone(),
            second_sender,
            crate::peer::ConnectionMeta {
                direction: crate::peer::ConnectionDirection::Outbound,
                config_label: Some(ConfigLabel("peer-a".into())),
                expected_peer: Some(peer.clone()),
                config_backed: true,
            },
            Some(new_session_id),
        ) {
            crate::peer::ActivationResult::Accepted { displaced, .. } => {
                assert_eq!(displaced, Some(previous_generation));
            }
            crate::peer::ActivationResult::Rejected { reason } => panic!("expected accepted replacement connection, got {reason:?}"),
        }
    }

    let current_session_id = handle_remote_restart_if_needed(&peer_manager, &daemon, &HostName::new("peer-a"), Some(old_session_id)).await;

    assert_eq!(current_session_id, Some(new_session_id), "current session id should update to the reconnected peer session");
    let snapshot =
        daemon.get_state(&flotilla_protocol::RepoSelector::Path(synthetic.clone())).await.expect("remote-only repo should remain");
    assert!(
        !snapshot.providers.checkouts.contains_key(&HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only")),
        "restart cleanup should remove stale peer-a checkout"
    );
    assert_eq!(snapshot.providers.checkouts[&HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only")].branch, "feature-b");

    let pm = peer_manager.lock().await;
    assert!(
        !pm.get_peer_data().get(&HostName::new("peer-a")).is_some_and(|repos| repos.contains_key(&repo_identity)),
        "restart cleanup should clear stale cached repo data for the restarted peer"
    );
    assert!(
        !pm.get_peer_host_summaries().contains_key(&HostName::new("peer-a")),
        "restart cleanup should clear stale host summary for the restarted peer"
    );
}

#[tokio::test]
async fn peer_manager_initialized_from_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("config");
    std::fs::create_dir_all(&base).expect("create config directory");

    // Write daemon config with a custom host name
    std::fs::write(base.join("daemon.toml"), "host_name = \"test-host\"\n").expect("write daemon config");

    // Write hosts config with one peer
    std::fs::write(
        base.join("hosts.toml"),
        "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = \"remote\"\ndaemon_socket = \"/tmp/daemon.sock\"\n",
    )
    .expect("write hosts config");

    let config = Arc::new(ConfigStore::with_base(&base));
    let server = DaemonServer::new(vec![], config, fake_discovery(false), tmp.path().join("test.sock"), Duration::from_secs(60))
        .await
        .expect("create daemon server");

    // PeerManager should be initialized and accessible
    let pm = server.peer_manager.lock().await;
    // peer_data is empty since no data has been received yet
    assert!(pm.get_peer_data().is_empty());
}

#[tokio::test]
async fn peer_manager_default_when_no_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let server = DaemonServer::new(vec![], config, fake_discovery(false), tmp.path().join("test.sock"), Duration::from_secs(60))
        .await
        .expect("create daemon server");

    // Should still have a PeerManager with no peers
    let pm = server.peer_manager.lock().await;
    assert!(pm.get_peer_data().is_empty());
}

#[tokio::test]
async fn daemon_server_new_returns_error_for_invalid_hosts_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("config");
    std::fs::create_dir_all(&base).expect("create config directory");
    std::fs::write(
        base.join("hosts.toml"),
        "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = [\ndaemon_socket = \"/tmp/daemon.sock\"\n",
    )
    .expect("write invalid hosts config");

    let config = Arc::new(ConfigStore::with_base(&base));
    let result = DaemonServer::new(vec![], config, fake_discovery(false), tmp.path().join("test.sock"), Duration::from_secs(60)).await;

    match result {
        Ok(_) => panic!("invalid hosts config should return startup error"),
        Err(err) => assert!(err.contains("failed to parse")),
    }
}

#[tokio::test]
async fn handle_client_relays_outbound_peer_messages() {
    let (_tmp, daemon) = empty_daemon().await;
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

    // Spawn handle_client on the server side
    let pm = Arc::clone(&peer_manager);
    let count_ref = Arc::clone(&client_count);
    let notify_ref = Arc::clone(&client_notify);
    let handle = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon, &pm);
        handle_client(
            server_stream,
            daemon,
            shutdown_rx,
            peer_data_tx,
            pm,
            remote_command_router,
            count_ref,
            notify_ref,
            peer_connected_tx,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });

    let (read_half, write_half) = client_stream.into_split();
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);

    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_name: HostName::new("relay-target"),
        session_id: uuid::Uuid::nil(),
        environment_id: None,
    };
    flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");
    let _ = reader.next_line().await.expect("read hello").expect("line");

    tokio::time::sleep(Duration::from_millis(100)).await;

    {
        let pm = peer_manager.lock().await;
        pm.send_to(&HostName::new("relay-target"), PeerWireMessage::Data(test_peer_msg("other-host"))).await.expect("send relay");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut found_relay = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), reader.next_line()).await {
            Ok(Ok(Some(line))) => {
                let msg: Message = serde_json::from_str(&line).expect("parse");
                if let Message::Peer(peer_msg) = msg {
                    if let PeerWireMessage::Data(peer_msg) = *peer_msg {
                        assert_eq!(peer_msg.origin_host, HostName::new("other-host"));
                        found_relay = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(found_relay, "should have received relayed peer message");

    // Clean up
    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn duplicate_inbound_peer_receives_goodbye_on_rejection() {
    let (_tmp, daemon) = empty_daemon().await;
    let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

    let (client_stream_a, server_stream_a) = tokio::net::UnixStream::pair().expect("pair a");
    let (client_stream_b, server_stream_b) = tokio::net::UnixStream::pair().expect("pair b");

    let expected_server_host = daemon.host_name().clone();
    let daemon_a = Arc::clone(&daemon);
    let daemon_b = Arc::clone(&daemon);
    let pm_a = Arc::clone(&peer_manager);
    let pm_b = Arc::clone(&peer_manager);
    let tx_a = peer_data_tx.clone();
    let tx_b = peer_data_tx.clone();
    let count_a = Arc::clone(&client_count);
    let count_b = Arc::clone(&client_count);
    let notify_a = Arc::clone(&client_notify);
    let notify_b = Arc::clone(&client_notify);
    let shutdown_rx_a = shutdown_rx.clone();
    let shutdown_rx_b = shutdown_rx.clone();
    let peer_connected_tx_a = peer_connected_tx.clone();
    let peer_connected_tx_b = peer_connected_tx.clone();

    let handle_a = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon_a, &pm_a);
        handle_client(
            server_stream_a,
            daemon_a,
            shutdown_rx_a,
            tx_a,
            pm_a,
            remote_command_router,
            count_a,
            notify_a,
            peer_connected_tx_a,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });
    let handle_b = tokio::spawn(async move {
        let remote_command_router = empty_remote_command_router(&daemon_b, &pm_b);
        handle_client(
            server_stream_b,
            daemon_b,
            shutdown_rx_b,
            tx_b,
            pm_b,
            remote_command_router,
            count_b,
            notify_b,
            peer_connected_tx_b,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
        )
        .await;
    });

    async fn send_peer_hello(
        stream: tokio::net::UnixStream,
        expected_server_host: &HostName,
    ) -> (tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>, tokio::io::BufWriter<tokio::net::unix::OwnedWriteHalf>)
    {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half).lines();
        let mut writer = BufWriter::new(write_half);
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            host_name: HostName::new("peer"),
            session_id: uuid::Uuid::nil(),
            environment_id: None,
        };
        flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

        let line = reader.next_line().await.expect("read hello response").expect("hello response line");
        let msg: Message = serde_json::from_str(&line).expect("parse hello response");
        match msg {
            Message::Hello { host_name, .. } => {
                assert_eq!(host_name, expected_server_host.clone())
            }
            other => panic!("expected hello response, got {other:?}"),
        }

        (reader, writer)
    }

    let (_reader_a, writer_a) = send_peer_hello(client_stream_a, &expected_server_host).await;
    let (mut reader_b, writer_b) = send_peer_hello(client_stream_b, &expected_server_host).await;

    let goodbye = tokio::time::timeout(Duration::from_secs(2), reader_b.next_line())
        .await
        .expect("timeout waiting for goodbye")
        .expect("read goodbye line")
        .expect("goodbye line");
    let goodbye_msg: Message = serde_json::from_str(&goodbye).expect("parse goodbye");
    match goodbye_msg {
        Message::Peer(inner) => match *inner {
            PeerWireMessage::Goodbye { reason: flotilla_protocol::GoodbyeReason::Superseded } => {}
            other => panic!("expected superseded goodbye, got {other:?}"),
        },
        other => panic!("expected peer goodbye, got {other:?}"),
    }

    drop(writer_a);
    drop(writer_b);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle_a).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), handle_b).await;
}

#[tokio::test]
async fn clear_peer_data_rebuilds_remote_only_repo_without_stale_first_event() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let repo_identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
    let repo_path = PathBuf::from("/srv/remote-only");

    {
        let mut pm = peer_manager.lock().await;
        assert_eq!(
            handle_test_peer_data(
                &mut pm,
                peer_snapshot("peer-a", &repo_identity, &repo_path, "/srv/peer-a/remote-only", "feature-a",),
                || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
            )
            .await,
            crate::peer::HandleResult::Updated(repo_identity.clone())
        );
        assert_eq!(
            handle_test_peer_data(
                &mut pm,
                peer_snapshot("peer-b", &repo_identity, &repo_path, "/srv/peer-b/remote-only", "feature-b",),
                || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
            )
            .await,
            crate::peer::HandleResult::Updated(repo_identity.clone())
        );
    }

    let synthetic = crate::peer::synthetic_repo_path(&HostName::new("peer-a"), &repo_path);
    daemon
        .add_virtual_repo(
            repo_identity.clone(),
            synthetic.clone(),
            vec![
                (HostName::new("peer-a"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only"), checkout("feature-a"))]),
                    ..Default::default()
                }),
                (HostName::new("peer-b"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only"), checkout("feature-b"))]),
                    ..Default::default()
                }),
            ],
            0,
        )
        .await
        .expect("add virtual repo");
    {
        let mut pm = peer_manager.lock().await;
        pm.register_remote_repo(repo_identity.clone(), synthetic.clone());
    }

    let mut rx = daemon.subscribe();
    let gen_a = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &HostName::new("peer-a"), || {
            Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(Some(mpsc::channel(1).0)) }) as Arc<dyn PeerSender>
        })
    };

    disconnect_peer_and_rebuild(&peer_manager, &daemon, &HostName::new("peer-a"), gen_a).await;

    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await.expect("broadcast channel should stay open") {
                DaemonEvent::HostRemoved { .. } => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("timeout waiting for first repo event");

    let stale_key = HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only");
    let remaining_key = HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only");
    match event {
        DaemonEvent::RepoSnapshot(snapshot) => {
            assert_eq!(snapshot.repo, synthetic);
            assert!(
                !snapshot.providers.checkouts.contains_key(&stale_key),
                "first snapshot after disconnect should not include stale peer-a checkout"
            );
            assert_eq!(snapshot.providers.checkouts[&remaining_key].branch, "feature-b");
        }
        DaemonEvent::RepoDelta(delta) => {
            assert_eq!(delta.repo, synthetic);
            assert!(
                delta.changes.iter().any(|change| matches!(
                    change,
                    flotilla_protocol::Change::Checkout {
                        key,
                        op: flotilla_protocol::EntryOp::Removed
                    } if *key == stale_key
                )),
                "first delta after disconnect should remove stale peer-a checkout"
            );
        }
        other => panic!("expected snapshot event, got {other:?}"),
    }
}

/// Verifies the fix for the cancel race: when the `Launching` entry is
/// pre-inserted (as the dispatch loop now does), a cancel that arrives
/// before `execute_forwarded_command` transitions to `Running` will wait
/// for the transition rather than failing with "remote command not found".
#[tokio::test]
async fn cancel_before_execute_registration_finds_entry() {
    let (_tmp, daemon) = empty_daemon().await;
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    let next_remote_command_id = Arc::new(AtomicU64::new(1 << 62));
    let remote_command_router = make_remote_command_router(
        &daemon,
        &peer_manager,
        &pending_remote_commands,
        &forwarded_commands,
        &pending_remote_cancels,
        &next_remote_command_id,
    );
    let sent = Arc::new(StdMutex::new(Vec::new()));
    peer_manager.lock().await.register_sender(HostName::new("relay"), Arc::new(MockPeerSender { sent: Arc::clone(&sent) }));

    // Pre-insert the Launching entry, mirroring the dispatch-loop fix.
    let ready = Arc::new(Notify::new());
    forwarded_commands.lock().await.insert(99, ForwardedCommand { state: ForwardedCommandState::Launching { ready: Arc::clone(&ready) } });

    // Spawn cancel — it should wait on the Launching state instead of
    // returning "remote command not found".
    let router_for_task = remote_command_router.clone();
    let handle = tokio::spawn(async move {
        router_for_task.cancel_forwarded_command_for_test(42, HostName::new("desktop"), HostName::new("relay"), 99).await;
    });

    tokio::time::sleep(StdDuration::from_millis(50)).await;

    // Transition to Running and notify — cancel should now proceed.
    if let Some(entry) = forwarded_commands.lock().await.get_mut(&99) {
        entry.state = ForwardedCommandState::Running { command_id: 456 };
    }
    ready.notify_waiters();

    handle.await.expect("cancel task");

    let sent = sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Routed(RoutedPeerMessage::CommandCancelResponse { error, .. }) => {
            assert!(
                !error.as_deref().unwrap_or("").contains("remote command not found"),
                "cancel should not fail with 'not found', got: {error:?}"
            );
        }
        other => panic!("expected cancel response, got {other:?}"),
    }
}

#[tokio::test]
async fn set_peer_providers_rejects_stale_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("create .git");
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::new("local")).await;

    let fresh_peers = vec![(HostName::new("hostB"), ProviderData {
        checkouts: IndexMap::from([(HostPath::new(HostName::new("hostB"), "/b/repo"), checkout("fresh"))]),
        ..Default::default()
    })];
    let stale_peers = vec![(HostName::new("hostB"), ProviderData {
        checkouts: IndexMap::from([(HostPath::new(HostName::new("hostB"), "/b/repo"), checkout("stale"))]),
        ..Default::default()
    })];

    // Apply version 5 first, then try to apply version 3 — should be rejected
    daemon.set_peer_providers(&repo, fresh_peers.clone(), 5).await;
    daemon.set_peer_providers(&repo, stale_peers, 3).await;

    let identity = daemon.tracked_repo_identity_for_path(&repo).await.expect("identity");
    let pp = daemon.peer_providers_for_test(&identity).await;
    let branch = pp[0].1.checkouts.values().next().expect("checkout").branch.as_str();
    assert_eq!(branch, "fresh", "stale version should have been rejected");
}
