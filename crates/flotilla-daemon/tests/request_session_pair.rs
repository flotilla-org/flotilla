use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{
        discovery::test_support::{fake_discovery, fake_discovery_with_provider_set, init_git_repo_with_remote, FakeDiscoveryProviders},
        issue_tracker::IssueProvider,
    },
};
use flotilla_daemon::server::test_support::{spawn_in_memory_request_topology, spawn_in_memory_request_topology_stateful};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    result_set::{ConvoyPhase as RowConvoyPhase, ConvoyRow},
    test_support::TestIssue,
    Command, CommandAction, CommandValue, DaemonEvent, HostName, Issue, IssueChangeset, IssueRef, IssueSource, NodeInfo,
    PeerConnectionState, RepoSelector, ResourceRef,
};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta, Resource, ResourceError,
    WorkPhase as ResourceWorkPhase, WorkState,
};

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

fn convoy_spec(workflow_ref: &str) -> ConvoySpec {
    ConvoySpec::builder().workflow_ref(workflow_ref.to_string()).build()
}

fn convoy_ref(namespace: &str, name: &str, host: HostName) -> ResourceRef {
    ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(host)
}

async fn seed_convoy_projection(daemon: &InProcessDaemon, namespace: &str, name: &str, home: HostName) {
    let resource = convoy_ref(namespace, name, home.clone());
    let row = ConvoyRow::builder().resource(resource.clone()).name(name).workflow_ref("scratch").phase(RowConvoyPhase::Pending).build();
    daemon
        .aggregator_projection_state()
        .await
        .write()
        .await
        .replace_replica_rows(HashMap::from([(home, HashMap::from([(resource, row)]))]));
}

async fn await_command_result(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id: id, result, .. } = rx.recv().await.expect("daemon event") {
                if id == command_id {
                    return result;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for command result")
}

// ---------------------------------------------------------------------------
// MockIssueProvider — returns a fixed result for assertions
// ---------------------------------------------------------------------------

struct MockIssueProvider;

#[async_trait]
impl IssueProvider for MockIssueProvider {
    fn supports(&self, _source: &IssueSource) -> bool {
        true
    }

    async fn query(&self, _source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
        Ok(IssueResultPage { items: vec![TestIssue::new("Test issue").id("1").build()], total: Some(1), has_more: false })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        Err(format!("issue {} not found", reference.id))
    }

    async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
        Ok(IssueChangeset { updated: vec![], closed: vec![], has_more: false })
    }

    async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
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

#[tokio::test]
async fn hostless_convoy_delete_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "remote-only";

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    follower_convoys
        .create(&InputMeta::builder().name(convoy_name.to_string()).build(), &convoy_spec("scratch"))
        .await
        .expect("create remote-homed convoy");

    seed_convoy_projection(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

    let mut rx = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true },
        })
        .await
        .expect("dispatch hostless convoy delete");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    assert!(
        matches!(follower_convoys.get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "remote-homed convoy should be deleted from follower store"
    );
    assert!(
        matches!(topology.leader.resource_backend().using::<Convoy>(namespace).get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "dispatch host should not create or own the convoy"
    );
}

#[tokio::test]
async fn mistargeted_convoy_delete_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "mistargeted";

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    follower_convoys
        .create(&InputMeta::builder().name(convoy_name.to_string()).build(), &convoy_spec("scratch"))
        .await
        .expect("create remote-homed convoy");
    seed_convoy_projection(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

    let mut rx = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: Some(topology.leader.node_id().clone()),
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true },
        })
        .await
        .expect("dispatch mistargeted convoy delete");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    assert!(
        matches!(follower_convoys.get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "convoy operation should be rerouted to the row's home even when the incoming command has a stale node target"
    );
}

#[tokio::test]
async fn hostless_convoy_abandon_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "remote-abandon";

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    follower_convoys
        .create(&InputMeta::builder().name(convoy_name.to_string()).build(), &convoy_spec("scratch"))
        .await
        .expect("create remote-homed convoy");
    seed_convoy_projection(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

    let mut rx = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyAbandon {
                namespace: Some(namespace.to_string()),
                name: convoy_name.to_string(),
                reason: "accepted loss".to_string(),
            },
        })
        .await
        .expect("dispatch hostless convoy abandon");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    assert!(
        matches!(follower_convoys.get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "remote-homed convoy should be abandoned and deleted from follower store"
    );
}

#[tokio::test]
async fn hostless_convoy_work_complete_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "remote-work";
    let work_name = "implement";

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    let created = follower_convoys
        .create(&InputMeta::builder().name(convoy_name.to_string()).build(), &convoy_spec("scratch"))
        .await
        .expect("create remote-homed convoy");
    follower_convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            work: BTreeMap::from([(work_name.to_string(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
            ..Default::default()
        })
        .await
        .expect("seed remote work status");
    seed_convoy_projection(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

    let mut rx = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyWorkForceComplete {
                convoy: convoy_name.to_string(),
                work: work_name.to_string(),
                message: Some("done".to_string()),
            },
        })
        .await
        .expect("dispatch hostless work completion");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    let status = follower_convoys.get(convoy_name).await.expect("remote convoy").status.expect("remote convoy status");
    let work = status.work.get(work_name).expect("work status");
    assert_eq!(work.phase, ResourceWorkPhase::Complete);
    assert_eq!(work.message.as_deref(), Some("done"));
}

#[tokio::test]
async fn hostless_convoy_command_explains_missing_home_route() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "stranded";

    seed_convoy_projection(&topology.leader, namespace, convoy_name, HostName::new("feta")).await;

    let message = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyAbandon {
                namespace: Some(namespace.to_string()),
                name: convoy_name.to_string(),
                reason: "lost host".to_string(),
            },
        })
        .await
        .expect_err("unreachable convoy home should reject dispatch");

    assert_eq!(message, "connect to feta for convoy stranded: no routed node address found for host");
}

#[tokio::test]
async fn hostless_convoy_delete_uses_live_peer_route_when_connection_status_is_stale() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "offline-home";

    seed_convoy_projection(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;
    topology
        .leader
        .publish_peer_connection_status(
            &NodeInfo::new(topology.follower.node_id().clone(), topology.follower_host.to_string()),
            PeerConnectionState::Disconnected,
        )
        .await;

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    follower_convoys
        .create(&InputMeta::builder().name(convoy_name.to_string()).build(), &convoy_spec("scratch"))
        .await
        .expect("create remote-homed convoy");

    let mut rx = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true },
        })
        .await
        .expect("live peer route should take precedence over stale connection status");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    assert!(
        matches!(follower_convoys.get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "remote-homed convoy should be deleted through the live peer route"
    );
}

/// A stateless remote issue query should return results end-to-end.
#[tokio::test]
async fn remote_issue_query_returns_results() {
    let mock_service = Arc::new(MockIssueProvider);

    let follower_tmp = tempfile::tempdir().expect("tempdir");
    let follower_repo = follower_tmp.path().join("repo");
    init_git_repo_with_remote(&follower_repo, "git@github.com:owner/repo.git");
    let follower_config = test_config_store(follower_tmp.path().join("config"));
    let follower_discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_tracker(Arc::clone(&mock_service) as Arc<dyn IssueProvider>),
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
            assert_eq!(page.items[0].title, "Test issue");
        }
        other => panic!("expected IssuePage, got {other:?}"),
    }
}
