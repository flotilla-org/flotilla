use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
use flotilla_daemon::server::test_support::{
    apply_convoy_replica_feed, seed_trusted_remote_convoy_project, spawn_in_memory_request_topology,
    spawn_in_memory_request_topology_stateful, spawn_in_memory_request_topology_stateful_with_surface,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    test_support::TestIssue,
    Command, CommandAction, CommandValue, ConvoyStartIntent, DaemonEvent, HostName, Issue, IssueChangeset, IssueRef, IssueSource, NodeInfo,
    PeerConnectionState, PrincipalRef, RepoSelector, ResourceRef, SurfaceCharacter, SurfaceDeclaration,
};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta, PlacementPolicy, Regard, Resource,
    ResourceError, WorkPhase as ResourceWorkPhase, WorkState, WorkflowTemplate, WorkflowTemplateSpec,
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

#[tokio::test]
async fn ambient_surface_observations_do_not_create_regards_over_the_client_protocol() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful_with_surface(Arc::clone(&leader), follower, SurfaceDeclaration {
        principal_ref: PrincipalRef::implicit_for_namespace("flotilla"),
        character: SurfaceCharacter::Ambient,
    })
    .await
    .expect("spawn ambient client topology");

    topology
        .client
        .observe_focus(uuid::Uuid::nil(), vec![ResourceRef::new(
            api_version(Convoy::API_PATHS),
            Convoy::API_PATHS.kind,
            "flotilla",
            "ambient-demo",
        )])
        .await
        .expect("report ambient focus");

    assert!(leader.resource_backend().using::<Regard>("flotilla").list().await.expect("list regards").items.is_empty());
}

#[tokio::test]
async fn default_focal_surface_uses_the_daemons_provisioning_principal() {
    let leader = empty_daemon_named("leader").await;
    leader.set_provisioning_namespace("dev".to_string()).await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(Arc::clone(&leader), follower).await.expect("spawn default client topology");

    topology
        .client
        .observe_focus(uuid::Uuid::nil(), vec![ResourceRef::new(
            api_version(Convoy::API_PATHS),
            Convoy::API_PATHS.kind,
            "dev",
            "focused-demo",
        )])
        .await
        .expect("report focal focus");

    let regards = leader.resource_backend().using::<Regard>("dev").list().await.expect("list regards");
    assert_eq!(regards.items.len(), 1);
    assert_eq!(regards.items[0].spec.principal_ref, PrincipalRef::implicit_for_namespace("dev"));
}

#[tokio::test]
async fn convoy_creation_attributes_provenance_and_regard_to_the_surface_principal() {
    let leader = empty_daemon_named("leader").await;
    leader
        .resource_backend()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("empty".to_string()).build(), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
        .await
        .expect("create workflow");
    let follower = empty_daemon_named("follower").await;
    let principal_ref = PrincipalRef { namespace: "flotilla".to_string(), name: "alice".to_string() };
    let topology = spawn_in_memory_request_topology_stateful_with_surface(Arc::clone(&leader), follower, SurfaceDeclaration {
        principal_ref: principal_ref.clone(),
        character: SurfaceCharacter::Focal,
    })
    .await
    .expect("spawn named focal client topology");
    let mut events = leader.subscribe();

    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "alice-dispatch".to_string(),
                workflow_ref: "empty".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
                adopted_checkout: None,
            },
        })
        .await
        .expect("dispatch convoy creation");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyCreated { name: "alice-dispatch".to_string() });

    let convoy = leader.resource_backend().using::<Convoy>("flotilla").get("alice-dispatch").await.expect("created convoy");
    assert_eq!(convoy.spec.dispatching_principal_ref, principal_ref);
    let regards = leader.resource_backend().using::<Regard>("flotilla").list().await.expect("list regards");
    assert_eq!(regards.items.len(), 1);
    assert_eq!(regards.items[0].spec.principal_ref, convoy.spec.dispatching_principal_ref);
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

    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

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
    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

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
    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

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
    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

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

    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, HostName::new("feta")).await;

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

    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;
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

#[tokio::test]
async fn convoy_start_uses_live_peer_route_when_presentation_membership_is_stale() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let remote_host_id = topology.follower.local_host_id().expect("follower host identity").to_string();
    let placement_policy = format!("host-direct-{remote_host_id}");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if topology.leader.resource_backend().using::<PlacementPolicy>(namespace).get(&placement_policy).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer host summary should materialize placement policy");

    seed_trusted_remote_convoy_project(&topology.leader, namespace).await;

    apply_convoy_replica_feed(&topology.leader, namespace, "fresh-feed", topology.follower_host.clone()).await;
    topology
        .leader
        .publish_peer_connection_status(
            &NodeInfo::new(topology.follower.node_id().clone(), topology.follower_host.to_string()),
            PeerConnectionState::Disconnected,
        )
        .await;
    topology.leader.set_peer_host_summaries(HashMap::new()).await;
    assert_eq!(topology.leader.peer_connection_status(topology.follower.node_id()).await, PeerConnectionState::Disconnected);
    assert!(
        topology
            .leader
            .get_topology()
            .await
            .expect("leader topology")
            .routes
            .iter()
            .any(|route| route.target.node_id == *topology.follower.node_id() && route.connected),
        "peer manager route should remain live"
    );

    let mut events = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyStart {
                intent: Box::new(
                    ConvoyStartIntent::builder()
                        .project_ref("flotilla".to_string())
                        .name("remote-work".to_string())
                        .branch("fix/remote-work".to_string())
                        .placement_policy(placement_policy)
                        .build(),
                ),
            },
        })
        .await
        .expect("live peer route should admit remote placement despite stale presentation membership");

    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyStarted {
        name: "remote-work".to_string(),
        attach_command: None,
        binding: None
    });
    topology
        .follower
        .resource_backend()
        .using::<Convoy>(namespace)
        .get("remote-work")
        .await
        .expect("convoy should be created on live placement host");
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
