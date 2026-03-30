use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::test_support::{fake_discovery, fake_discovery_with_provider_set, init_git_repo_with_remote, FakeDiscoveryProviders},
        issue_query::{CursorId, IssueQuery, IssueQueryService, IssueResultPage},
    },
};
use flotilla_daemon::server::test_support::{spawn_in_memory_request_topology, spawn_in_memory_request_topology_stateful};
use flotilla_protocol::{provider_data::Issue, Command, CommandAction, CommandValue, HostName, RepoSelector};
use tokio::sync::Mutex;

async fn empty_daemon_named(host_name: &str) -> Arc<InProcessDaemon> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(tmp.keep()));
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await
}

// ---------------------------------------------------------------------------
// TrackingIssueQueryService — records open/close/disconnect for assertions
// ---------------------------------------------------------------------------

struct TrackingIssueQueryService {
    open_cursors: Mutex<HashMap<CursorId, uuid::Uuid>>,
    next_id: AtomicU64,
}

impl TrackingIssueQueryService {
    fn new() -> Self {
        Self { open_cursors: Mutex::new(HashMap::new()), next_id: AtomicU64::new(0) }
    }

    async fn open_cursor_count(&self) -> usize {
        self.open_cursors.lock().await.len()
    }
}

#[async_trait]
impl IssueQueryService for TrackingIssueQueryService {
    async fn open_query(&self, _repo: &Path, _params: IssueQuery, session_id: uuid::Uuid) -> Result<CursorId, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cursor = CursorId::new(format!("test-{id}"));
        self.open_cursors.lock().await.insert(cursor.clone(), session_id);
        Ok(cursor)
    }

    async fn close_query(&self, cursor: &CursorId) {
        self.open_cursors.lock().await.remove(cursor);
    }

    async fn disconnect_session(&self, session_id: uuid::Uuid) -> Vec<CursorId> {
        let mut cursors = self.open_cursors.lock().await;
        let removed: Vec<CursorId> = cursors.iter().filter(|(_, sid)| **sid == session_id).map(|(cid, _)| cid.clone()).collect();
        for cid in &removed {
            cursors.remove(cid);
        }
        removed
    }

    async fn fetch_page(&self, _cursor: &CursorId, _count: usize) -> Result<IssueResultPage, String> {
        Ok(IssueResultPage { items: vec![], total: None, has_more: false })
    }

    async fn fetch_by_ids(&self, _repo: &Path, _ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }

    async fn open_in_browser(&self, _repo: &Path, _id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn in_memory_request_client_routes_remote_command_result() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology(leader, follower).await.expect("spawn in-memory topology");

    // Query commands return a directed QueryResult response instead of
    // broadcasting via CommandFinished, so use execute_query.
    let result = topology
        .client
        .execute_query(
            Command {
                host: Some(HostName::new("follower")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_host: "follower".into() },
            },
            uuid::Uuid::nil(),
        )
        .await
        .expect("dispatch remote host status query");

    match result {
        CommandValue::HostStatus(status) => {
            assert_eq!(status.host, HostName::new("follower"));
            // The query targets host "follower", so it must be forwarded
            // to the follower daemon and executed there — where it is local.
            assert!(status.is_local, "follower should appear as local from its own perspective");
        }
        other => panic!("expected HostStatus result, got {other:?}"),
    }
}

/// When a stateful client opens a remote issue-query cursor and then
/// disconnects, the cursor must be cleaned up on the target daemon.
#[tokio::test]
async fn remote_cursor_cleaned_up_on_client_disconnect() {
    let tracking_service = Arc::new(TrackingIssueQueryService::new());

    // Set up a follower with a tracked repo and the tracking issue query service.
    let follower_tmp = tempfile::tempdir().expect("tempdir");
    let follower_repo = follower_tmp.path().join("repo");
    init_git_repo_with_remote(&follower_repo, "git@github.com:owner/repo.git");
    let follower_config = Arc::new(ConfigStore::with_base(follower_tmp.path().join("config")));
    let follower_discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_query_service(Arc::clone(&tracking_service) as Arc<dyn IssueQueryService>),
    );
    let follower = InProcessDaemon::new(vec![follower_repo.clone()], follower_config, follower_discovery, HostName::new("follower")).await;
    follower.refresh(&RepoSelector::Path(follower_repo.clone())).await.expect("refresh follower repo");

    let leader = empty_daemon_named("leader").await;

    // Build topology with a stateful client (Hello handshake assigns session_id).
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");

    // Open a remote cursor via the client targeting the follower.
    let open_result = topology
        .client
        .execute_query(
            Command {
                host: Some(HostName::new("follower")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueOpen { repo: RepoSelector::Path(follower_repo.clone()), params: IssueQuery::default() },
            },
            uuid::Uuid::nil(), // session_id on SocketDaemon is ignored; the server uses the Hello session_id
        )
        .await
        .expect("open remote cursor");

    let cursor = match open_result {
        CommandValue::IssueQueryOpened { cursor } => cursor,
        other => panic!("expected IssueQueryOpened, got {other:?}"),
    };
    assert_eq!(tracking_service.open_cursor_count().await, 1, "cursor should be open on follower");

    // Verify the cursor works by fetching a page.
    let _page = topology
        .client
        .execute_query(
            Command {
                host: Some(HostName::new("follower")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueFetchPage { cursor: cursor.clone(), count: 10 },
            },
            uuid::Uuid::nil(),
        )
        .await
        .expect("fetch page from remote cursor");

    // Signal shutdown — the server's request loop exits and runs finish_session,
    // which closes remote cursors via dispatch_query (peer runtime is still live).
    let _ = topology.shutdown_tx.send(true);

    // Give the server time to process shutdown and forward cursor cleanup.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The cursor should have been cleaned up on the follower via explicit
    // QueryIssueClose forwarded by the leader's disconnect_session_cursors.
    assert_eq!(tracking_service.open_cursor_count().await, 0, "cursor should be cleaned up after client disconnect");
}
