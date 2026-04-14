use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::test_support::{fake_discovery, fake_discovery_with_provider_set, init_git_repo_with_remote, FakeDiscoveryProviders},
        issue_query::{IssueQuery, IssueQueryService, IssueResultPage},
    },
};
use flotilla_daemon::server::test_support::{spawn_in_memory_request_topology, spawn_in_memory_request_topology_stateful};
use flotilla_protocol::{provider_data::Issue, Command, CommandAction, CommandValue, HostName, RepoSelector};

fn test_config_store(config_dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(config_dir))
}

async fn empty_daemon_named(host_name: &str) -> Arc<InProcessDaemon> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store(tmp.keep());
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await
}

// ---------------------------------------------------------------------------
// MockIssueQueryService — returns a fixed result for assertions
// ---------------------------------------------------------------------------

struct MockIssueQueryService;

#[async_trait]
impl IssueQueryService for MockIssueQueryService {
    async fn query(&self, _repo: &Path, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
        Ok(IssueResultPage {
            items: vec![("1".into(), Issue {
                title: "Test issue".into(),
                labels: vec![],
                association_keys: vec![],
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            })],
            total: Some(1),
            has_more: false,
        })
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
    let follower_node_id = topology.follower.node_id().clone();
    let follower_environment_id = topology.follower.local_host_summary().await.environment_id;

    // Query commands return a directed QueryResult response instead of
    // broadcasting via CommandFinished, so use execute_query.
    let result = topology
        .client
        .execute_query(
            Command {
                node_id: Some(follower_node_id.clone()),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_environment_id: follower_environment_id.clone() },
            },
            uuid::Uuid::nil(),
        )
        .await
        .expect("dispatch remote host status query");

    match result {
        CommandValue::HostStatus(status) => {
            assert_eq!(status.node.node_id, follower_node_id);
            // The query targets host "follower", so it must be forwarded
            // to the follower daemon and executed there — where it is local.
            assert!(status.is_local, "follower should appear as local from its own perspective");
        }
        other => panic!("expected HostStatus result, got {other:?}"),
    }
}

/// A stateless remote issue query should return results end-to-end.
#[tokio::test]
async fn remote_issue_query_returns_results() {
    let mock_service = Arc::new(MockIssueQueryService);

    let follower_tmp = tempfile::tempdir().expect("tempdir");
    let follower_repo = follower_tmp.path().join("repo");
    init_git_repo_with_remote(&follower_repo, "git@github.com:owner/repo.git");
    let follower_config = test_config_store(follower_tmp.path().join("config"));
    let follower_discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_query_service(Arc::clone(&mock_service) as Arc<dyn IssueQueryService>),
    );
    let follower = InProcessDaemon::new(vec![follower_repo.clone()], follower_config, follower_discovery, HostName::new("follower")).await;
    follower.refresh(&RepoSelector::Path(follower_repo.clone())).await.expect("refresh follower repo");

    let leader = empty_daemon_named("leader").await;

    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let follower_node_id = topology.follower.node_id().clone();

    let result = topology
        .client
        .execute_query(
            Command {
                node_id: Some(follower_node_id),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssues {
                    repo: RepoSelector::Path(follower_repo.clone()),
                    params: IssueQuery::default(),
                    page: 1,
                    count: 10,
                },
            },
            uuid::Uuid::nil(),
        )
        .await
        .expect("remote issue query");

    match result {
        CommandValue::IssuePage(page) => {
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].1.title, "Test issue");
        }
        other => panic!("expected IssuePage, got {other:?}"),
    }
}
