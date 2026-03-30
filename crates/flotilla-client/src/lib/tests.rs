use std::time::Duration;

use flotilla_protocol::{HostName, RepoDelta, RepoIdentity, RepoSnapshot};
use flotilla_transport::message::{message_session_pair, MessageSession};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
};

use super::*;

type SharedSession = Arc<MessageSession>;
type SharedPending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>;

struct RequestHarness {
    session: SharedSession,
    pending: SharedPending,
    next_id: Arc<AtomicU64>,
    remote: MessageSession,
}

struct SessionHarness {
    daemon: Arc<SocketDaemon>,
    session: MessageSession,
}

fn repo_identity() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn make_snapshot(repo: &Path, seq: u64) -> RepoSnapshot {
    RepoSnapshot {
        seq,
        repo_identity: repo_identity(),
        repo: repo.to_path_buf(),
        host_name: flotilla_protocol::HostName::new("test-host"),
        work_items: vec![],
        providers: flotilla_protocol::ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    }
}

fn make_delta(repo: &Path, prev_seq: u64, seq: u64) -> RepoDelta {
    RepoDelta {
        seq,
        prev_seq,
        repo_identity: repo_identity(),
        repo: repo.to_path_buf(),
        changes: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    }
}

fn request_harness() -> RequestHarness {
    let (client, server) = message_session_pair();
    RequestHarness {
        session: Arc::new(client),
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(1)),
        remote: server,
    }
}

fn session_harness() -> SessionHarness {
    let (client, server) = message_session_pair();
    let daemon = SocketDaemon::from_session(client).expect("build session-backed daemon");
    SessionHarness { daemon, session: server }
}

fn broken_request_harness() -> (SharedSession, SharedPending, Arc<AtomicU64>) {
    let (client, server) = message_session_pair();
    drop(server);
    (Arc::new(client), Arc::new(Mutex::new(HashMap::new())), Arc::new(AtomicU64::new(1)))
}

/// Returns a writer/pending/next_id triple for tests that call `handle_event`.
/// Also returns the remote half so the session stays open during the test.
fn event_harness() -> (SharedSession, SharedPending, Arc<AtomicU64>, MessageSession) {
    let (client, server) = message_session_pair();
    (Arc::new(client), Arc::new(Mutex::new(HashMap::new())), Arc::new(AtomicU64::new(1)), server)
}

async fn read_request(session: &MessageSession) -> (u64, Request) {
    match session.read().await.expect("read request") {
        Some(Message::Request { id, request }) => (id, request),
        other => panic!("expected request, got {other:?}"),
    }
}

#[tokio::test]
async fn session_backed_daemon_sends_requests_and_receives_responses() {
    let harness = session_harness();

    let daemon = Arc::clone(&harness.daemon);
    let request_task = tokio::spawn(async move { daemon.get_topology().await });

    let (id, request) = read_request(&harness.session).await;
    assert_eq!(request, Request::GetTopology);

    harness
        .session
        .write(Message::Response {
            id,
            response: Box::new(ResponseResult::Ok {
                response: Box::new(Response::GetTopology(TopologyResponse { local_host: HostName::new("local"), routes: vec![] })),
            }),
        })
        .await
        .expect("write response");

    let topology = request_task.await.expect("join request task").expect("get_topology");
    assert!(topology.routes.is_empty());
}

#[tokio::test]
async fn session_backed_daemon_streams_events_to_subscribers() {
    let harness = session_harness();
    let mut event_rx = harness.daemon.subscribe();
    let repo_identity = repo_identity();
    let repo = PathBuf::from("/tmp/session-backed-repo");

    harness
        .session
        .write(Message::Event {
            event: Box::new(DaemonEvent::CommandStarted {
                command_id: 99,
                host: HostName::new("remote"),
                repo_identity: repo_identity.clone(),
                repo: repo.clone(),
                description: "from session".into(),
            }),
        })
        .await
        .expect("write event");

    let event = event_rx.recv().await.expect("receive event");
    assert!(matches!(
        event,
        DaemonEvent::CommandStarted { command_id: 99, repo_identity: actual_identity, repo: actual_repo, ref description, .. }
            if actual_identity == repo_identity && actual_repo == repo && description == "from session"
    ));
}

#[tokio::test]
async fn send_request_writes_message_and_returns_pending_response() {
    let harness = request_harness();

    let request_session = Arc::clone(&harness.session);
    let request_pending = Arc::clone(&harness.pending);
    let request_next_id = Arc::clone(&harness.next_id);
    let request_task =
        tokio::spawn(async move { send_request(request_session.as_ref(), &request_pending, &request_next_id, Request::ListRepos).await });

    let (id, request) = read_request(&harness.remote).await;
    assert_eq!(request, Request::ListRepos);

    let tx = harness.pending.lock().await.remove(&id).expect("pending sender should exist");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ListRepos(vec![])) }).expect("send response");

    let response = request_task.await.expect("join").expect("send_request");
    assert!(matches!(response, ResponseResult::Ok { response } if matches!(&*response, Response::ListRepos(repos) if repos.is_empty())));
}

#[tokio::test]
async fn dropping_socket_daemon_closes_connection_promptly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind listener");

    let accept_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept client");
        stream
    });

    let daemon = SocketDaemon::connect(&socket_path).await.expect("connect socket daemon");
    let server_stream = accept_task.await.expect("join accept task");

    drop(daemon);

    let mut server_lines = BufReader::new(server_stream).lines();
    let eof = tokio::time::timeout(Duration::from_millis(100), server_lines.next_line())
        .await
        .expect("client drop should close connection promptly")
        .expect("read server EOF");

    assert!(eof.is_none(), "server should observe EOF after SocketDaemon drop");
}

#[tokio::test]
async fn send_request_returns_cancelled_when_sender_is_dropped() {
    let harness = request_harness();

    let request_session = Arc::clone(&harness.session);
    let request_pending = Arc::clone(&harness.pending);
    let request_next_id = Arc::clone(&harness.next_id);
    let task =
        tokio::spawn(async move { send_request(request_session.as_ref(), &request_pending, &request_next_id, Request::GetTopology).await });

    let (id, request) = read_request(&harness.remote).await;
    assert_eq!(request, Request::GetTopology);

    harness.pending.lock().await.remove(&id);
    let err = task.await.expect("join").expect_err("dropping sender should cancel request");
    assert!(err.contains("cancelled"));
}

#[tokio::test]
async fn send_request_cleans_pending_on_write_error() {
    let (session, pending, next_id) = broken_request_harness();

    let err = send_request(session.as_ref(), &pending, &next_id, Request::GetTopology).await.expect_err("closed peer should fail writes");

    assert!(err.contains("closed"));
    assert!(pending.lock().await.is_empty());
}

#[tokio::test]
async fn send_request_cleans_pending_on_cancelled_response() {
    let harness = request_harness();

    let request_session = Arc::clone(&harness.session);
    let request_pending = Arc::clone(&harness.pending);
    let request_next_id = Arc::clone(&harness.next_id);
    let task =
        tokio::spawn(async move { send_request(request_session.as_ref(), &request_pending, &request_next_id, Request::GetStatus).await });

    let (id, request) = read_request(&harness.remote).await;
    assert_eq!(request, Request::GetStatus);

    harness.pending.lock().await.remove(&id);

    let err = task.await.expect("join").expect_err("dropping sender should cancel request");
    assert!(err.contains("cancelled"));
    assert!(harness.pending.lock().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn send_request_cleans_pending_on_timeout() {
    let harness = request_harness();

    let request_session = Arc::clone(&harness.session);
    let request_pending = Arc::clone(&harness.pending);
    let request_next_id = Arc::clone(&harness.next_id);
    let task =
        tokio::spawn(async move { send_request(request_session.as_ref(), &request_pending, &request_next_id, Request::GetStatus).await });

    let (id, request) = read_request(&harness.remote).await;
    assert_eq!(id, 1);
    assert_eq!(request, Request::GetStatus);

    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;

    let err = task.await.expect("join").expect_err("missing response should time out");
    assert!(err.contains("timed out"));
    assert!(harness.pending.lock().await.is_empty());
}

#[tokio::test]
async fn handle_event_updates_local_seqs_for_full_and_matching_delta() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::RepoSnapshot(Box::new(make_snapshot(&repo, 10))),
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );
    handle_event(
        DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 10, 11))),
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let first = event_rx.recv().await.expect("event");
    assert!(matches!(first, DaemonEvent::RepoSnapshot(_)));
    let second = event_rx.recv().await.expect("event");
    assert!(matches!(second, DaemonEvent::RepoDelta(_)));
    assert_eq!(local_seqs.read().unwrap().get(&StreamKey::Repo { identity: repo_identity() }), Some(&11));
}

#[tokio::test]
async fn handle_event_buffers_delta_when_recovery_already_running() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().unwrap().insert(StreamKey::Repo { identity: repo_identity() }, 1);
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    recovering.lock().unwrap().insert(repo_identity(), vec![]);
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 99, 100))),
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let buffered = recovering.lock().unwrap().get(&repo_identity()).cloned().expect("buffer exists");
    assert_eq!(buffered.len(), 1);
    assert!(event_rx.try_recv().is_err(), "buffered delta should not dispatch");
}

#[tokio::test]
async fn recover_from_gap_requests_replay_and_applies_seqs() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().unwrap().insert(StreamKey::Repo { identity: repo_identity() }, 3);
    let (event_tx, mut event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    let replay_events = vec![DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 3, 4)))];
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender for replay");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");
    recover_task.await.expect("join recover");

    let event = event_rx.recv().await.expect("forwarded replay event");
    assert!(matches!(event, DaemonEvent::RepoDelta(_)));
    assert_eq!(local_seqs.read().unwrap().get(&StreamKey::Repo { identity: repo_identity() }), Some(&4));
}

#[test]
fn acquire_spawn_lock_waiter_blocks_then_returns_none() {
    let unique = format!(
        "flotilla-client-lock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_nanos()
    );
    let lock_path = std::env::temp_dir().join(unique).join("daemon.sock.lock");
    let holder = acquire_spawn_lock(&lock_path).expect("acquire first lock").expect("first call should become spawner");

    // Use a barrier so we know the waiter thread has started before
    // checking is_finished(). Without this, a short sleep could pass
    // vacuously on a loaded machine (thread not yet scheduled).
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let barrier_clone = Arc::clone(&barrier);
    let lock_path_clone = lock_path.clone();
    let waiter = std::thread::spawn(move || {
        barrier_clone.wait();
        acquire_spawn_lock(&lock_path_clone).unwrap()
    });
    barrier.wait();
    // Waiter has started; give it time to enter flock() (a single syscall).
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!waiter.is_finished(), "waiter should block while lock is held");

    drop(holder);
    let waiter_result = waiter.join().expect("join waiter");
    assert!(waiter_result.is_none(), "waiter should return None after spawner releases lock");
    let _ = std::fs::remove_file(&lock_path);
}

// --- Gap detection: delta for unknown repo triggers recovery ---

#[tokio::test]
async fn handle_event_starts_recovery_for_unknown_repo_delta() {
    let repo = PathBuf::from("/tmp/unknown-repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    // Delta for a repo we have no local seq for — should start recovery.
    handle_event(
        DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 0, 1))),
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    // Recovery should have been initiated: repo should appear in recovering map.
    let guard = recovering.lock().expect("recovering mutex not poisoned");
    assert!(guard.contains_key(&repo_identity()), "unknown repo delta should start recovery");
}

// --- Gap detection: delta with seq gap triggers recovery ---

#[tokio::test]
async fn handle_event_starts_recovery_on_seq_gap() {
    let repo = PathBuf::from("/tmp/gap-repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 5);
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    // Delta with prev_seq=10 but local is 5 — gap.
    handle_event(
        DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 10, 11))),
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let guard = recovering.lock().expect("recovering mutex not poisoned");
    assert!(guard.contains_key(&repo_identity()), "seq gap should start recovery");
    // The triggering delta should be buffered as the first entry.
    let buffered = guard.get(&repo_identity()).expect("buffered events");
    assert_eq!(buffered.len(), 1);
}

// --- Non-snapshot events pass through unchanged ---

#[tokio::test]
async fn handle_event_forwards_repo_added() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    let repo_info = flotilla_protocol::RepoInfo {
        identity: repo_identity(),
        path: PathBuf::from("/tmp/new-repo"),
        name: "new-repo".into(),
        labels: flotilla_protocol::RepoLabels::default(),
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
    };
    handle_event(DaemonEvent::RepoTracked(Box::new(repo_info)), &local_seqs, &recovering, &event_tx, &session, &pending, &next_id);

    let event = event_rx.try_recv().expect("should receive RepoTracked");
    assert!(matches!(event, DaemonEvent::RepoTracked(_)));
}

#[tokio::test]
async fn handle_event_forwards_command_started() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::CommandStarted {
            command_id: 42,
            host: flotilla_protocol::HostName::new("test"),
            repo: PathBuf::from("/tmp/repo"),
            repo_identity: repo_identity(),
            description: "testing".into(),
        },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let event = event_rx.try_recv().expect("should receive CommandStarted");
    assert!(matches!(event, DaemonEvent::CommandStarted { command_id: 42, .. }));
}

#[tokio::test]
async fn handle_event_forwards_command_finished() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::CommandFinished {
            command_id: 7,
            host: flotilla_protocol::HostName::new("host"),
            repo: PathBuf::from("/tmp/repo"),
            repo_identity: repo_identity(),
            result: flotilla_protocol::commands::CommandValue::Ok,
        },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let event = event_rx.try_recv().expect("should receive CommandFinished");
    assert!(matches!(event, DaemonEvent::CommandFinished { command_id: 7, .. }));
}

#[tokio::test]
async fn handle_event_forwards_command_step_update() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::CommandStepUpdate {
            command_id: 3,
            host: flotilla_protocol::HostName::new("host"),
            repo: PathBuf::from("/tmp/repo"),
            repo_identity: repo_identity(),
            step_index: 1,
            step_count: 3,
            description: "step 2".into(),
            status: flotilla_protocol::commands::StepStatus::Started,
        },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let event = event_rx.try_recv().expect("should receive CommandStepUpdate");
    assert!(matches!(event, DaemonEvent::CommandStepUpdate { command_id: 3, .. }));
}

#[tokio::test]
async fn handle_event_forwards_peer_status_changed() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::PeerStatusChanged {
            host: flotilla_protocol::HostName::new("peer-1"),
            status: flotilla_protocol::PeerConnectionState::Connected,
        },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let event = event_rx.try_recv().expect("should receive PeerStatusChanged");
    assert!(matches!(event, DaemonEvent::PeerStatusChanged { .. }));
}

#[tokio::test]
async fn handle_event_forwards_host_removed_and_tracks_seq() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();
    let host = flotilla_protocol::HostName::new("peer-1");

    handle_event(
        DaemonEvent::HostRemoved { host: host.clone(), seq: 9 },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    let event = event_rx.try_recv().expect("should receive HostRemoved");
    assert!(matches!(event, DaemonEvent::HostRemoved { seq: 9, .. }));
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Host { host_name: host }).copied(), Some(9));
}

// --- RepoRemoved evicts seq and forwards ---

#[tokio::test]
async fn handle_event_repo_removed_evicts_seq_and_forwards() {
    let repo = PathBuf::from("/tmp/removed-repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 42);
    let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (session, pending, next_id, _server) = event_harness();

    handle_event(
        DaemonEvent::RepoUntracked { path: repo.clone(), repo_identity: repo_identity() },
        &local_seqs,
        &recovering,
        &event_tx,
        &session,
        &pending,
        &next_id,
    );

    // Seq should be evicted.
    assert!(
        local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }).is_none(),
        "seq should be evicted for untracked repo"
    );
    // Event should be forwarded.
    let event = event_rx.try_recv().expect("should receive RepoUntracked");
    assert!(matches!(event, DaemonEvent::RepoUntracked { .. }));
}

// --- recover_from_gap: parse error in replay response ---

#[tokio::test]
async fn recover_from_gap_handles_parse_error_gracefully() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 3);
    let (event_tx, _event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Respond with the wrong success variant.
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Ok { response: Box::new(Response::GetStatus(flotilla_protocol::StatusResponse { repos: vec![] })) })
        .expect("send bad response");

    // Should complete without panic.
    recover_task.await.expect("recover_from_gap should not panic on unexpected response variant");
    // Local seqs should remain unchanged.
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&3));
}

// --- recover_from_gap: request write failure ---

#[tokio::test]
async fn recover_from_gap_handles_request_failure_gracefully() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 5);
    let (event_tx, _event_rx) = broadcast::channel(16);

    let (session, pending, next_id) = broken_request_harness();

    // recover_from_gap should complete without panic even when the request fails.
    recover_from_gap(&local_seqs, &event_tx, &session, &pending, &next_id).await;
    // Local seqs should remain unchanged.
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&5));
}

// --- recover_from_gap: response with ok=false ---

#[tokio::test]
async fn recover_from_gap_handles_error_response_gracefully() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 7);
    let (event_tx, _event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Respond with a protocol error.
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Err { message: "internal error".into() }).expect("send error response");

    recover_task.await.expect("recover_from_gap should not panic on error response");
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&7));
}

// --- recover_from_gap: replay with RepoSnapshot updates seqs ---

#[tokio::test]
async fn recover_from_gap_applies_full_snapshot_seqs() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 2);
    let (event_tx, mut event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Respond with a full snapshot event.
    let replay_events = vec![DaemonEvent::RepoSnapshot(Box::new(make_snapshot(&repo, 10)))];
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

    recover_task.await.expect("join recover");

    let event = event_rx.recv().await.expect("forwarded replay event");
    assert!(matches!(event, DaemonEvent::RepoSnapshot(_)));
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&10));
}

// --- recover_from_gap: monotonic seq update (live event advances while replay in flight) ---

#[tokio::test]
async fn recover_from_gap_does_not_regress_seq_from_concurrent_live_update() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 3);
    let (event_tx, _event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        // Set seq high before recovery starts to validate the monotonic guard.
        recover_local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 20);
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Replay returns events with seq=10, which is behind the live update of 20.
    let replay_events = vec![DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 3, 10)))];
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

    recover_task.await.expect("join recover");

    // Seq should not regress: should remain at 20 (the higher value).
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&20));
}

// --- recover_from_gap: replay with non-snapshot events (e.g. CommandStarted) ---

#[tokio::test]
async fn recover_from_gap_forwards_non_snapshot_replay_events() {
    let repo = PathBuf::from("/tmp/repo");
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 1);
    let (event_tx, mut event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Replay includes a non-snapshot event — it should still be forwarded.
    let replay_events = vec![DaemonEvent::CommandStarted {
        command_id: 99,
        host: flotilla_protocol::HostName::new("test"),
        repo: repo.clone(),
        repo_identity: repo_identity(),
        description: "replayed".into(),
    }];
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

    recover_task.await.expect("join recover");

    let event = event_rx.recv().await.expect("should receive non-snapshot replay event");
    assert!(matches!(event, DaemonEvent::CommandStarted { command_id: 99, .. }));
    // Seq should be unchanged since CommandStarted doesn't carry a seq.
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&1));
}

// --- recover_from_gap: empty replay events ---

#[tokio::test]
async fn recover_from_gap_handles_empty_replay() {
    let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
    local_seqs.write().expect("local_seqs write lock").insert(StreamKey::Repo { identity: repo_identity() }, 5);
    let (event_tx, mut event_rx) = broadcast::channel(16);

    let harness = request_harness();

    let recover_local_seqs = Arc::clone(&local_seqs);
    let recover_event_tx = event_tx.clone();
    let recover_session = Arc::clone(&harness.session);
    let recover_pending = Arc::clone(&harness.pending);
    let recover_next_id = Arc::clone(&harness.next_id);
    let recover_task = tokio::spawn(async move {
        recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_session, &recover_pending, &recover_next_id).await;
    });

    let (id, request) = read_request(&harness.remote).await;
    assert!(matches!(request, Request::ReplaySince { .. }));

    // Respond with empty event list.
    let replay_events: Vec<DaemonEvent> = vec![];
    let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
    tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

    recover_task.await.expect("join recover");

    // No events forwarded.
    assert!(event_rx.try_recv().is_err(), "no events should be forwarded for empty replay");
    // Seq unchanged.
    assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&StreamKey::Repo { identity: repo_identity() }), Some(&5));
}

// --- ResponseResult helper paths ---

#[test]
fn into_success_response_returns_error_for_protocol_error() {
    let err = into_success_response(ResponseResult::Err { message: "something broke".into() }).expect_err("should fail");
    assert!(err.contains("something broke"));
}

#[test]
fn into_success_response_returns_response_for_success() {
    let response = into_success_response(ResponseResult::Ok {
        response: Box::new(Response::GetTopology(TopologyResponse { local_host: HostName::new("local"), routes: vec![] })),
    })
    .expect("should succeed");
    assert!(matches!(response, Response::GetTopology(TopologyResponse { routes, .. }) if routes.is_empty()));
}
