use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
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
    spawn_in_memory_request_topology_stateful, spawn_in_memory_request_topology_stateful_with_surface, InMemoryRequestTopology,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    test_support::TestIssue,
    Command, CommandAction, CommandValue, ConvoyStartIntent, DaemonEvent, HostName, Issue, IssueChangeset, IssueRef, IssueSource, NodeInfo,
    PeerConnectionState, PrincipalRef, RepoSelector, ResourceRef, SurfaceCharacter, SurfaceDeclaration,
};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus, CredentialConsumer, CredentialGrant,
    CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec,
    CredentialSpecSpec, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, Host, HostSpec, HostStatus, InputMeta, PlacementPolicy,
    PlacementPolicySpec, Regard, Resource, ResourceBackend, ResourceError, ResourceProvenance, WorkPhase as ResourceWorkPhase, WorkState,
    WorkflowTemplate, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, GENERATION_LABEL, HELD_CREDENTIALS_CAPABILITY, PROJECT_LABEL,
    ROLE_LABEL,
};

async fn convoy_record_name(backend: &ResourceBackend, role: &str) -> String {
    backend
        .using::<Convoy>("flotilla")
        .list_matching_labels(&BTreeMap::from([(ROLE_LABEL.to_string(), role.to_string())]))
        .await
        .expect("list convoys by role")
        .items
        .into_iter()
        .next()
        .expect("convoy should exist")
        .metadata
        .name
}

fn test_config_store(config_dir: std::path::PathBuf) -> Arc<ConfigStore> {
    test_config_store_with_floor(config_dir, None)
}

fn test_config_store_with_floor(config_dir: std::path::PathBuf, free_space_floor_gib: Option<u64>) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let floor = free_space_floor_gib.map(|floor| format!("\n[admission]\nfree_space_floor_gib = {floor}\n")).unwrap_or_default();
    std::fs::write(config_dir.join("daemon.toml"), format!("machine_id = \"test-machine\"\n{floor}")).expect("write daemon config");
    Arc::new(ConfigStore::with_base(config_dir))
}

async fn empty_daemon_named(host_name: &str) -> Arc<InProcessDaemon> {
    empty_daemon_named_with_floor(host_name, None).await
}

async fn empty_daemon_named_with_floor(host_name: &str, free_space_floor_gib: Option<u64>) -> Arc<InProcessDaemon> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = test_config_store_with_floor(tmp.keep(), free_space_floor_gib);
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await
}

async fn seed_host_capacity(daemon: &Arc<InProcessDaemon>, free_bytes: u64, floor_bytes: u64) {
    let host_id = daemon.local_host_id().expect("host identity").to_string();
    let hosts = daemon.resource_backend().using::<Host>("flotilla");
    let host = hosts
        .create(&InputMeta::builder().name(host_id.clone()).build(), &HostSpec::default())
        .await
        .expect("create host capacity resource");
    hosts
        .update_status(&host_id, &host.metadata.resource_version, &HostStatus {
            ready: true,
            disk_free_bytes: Some(free_bytes),
            admission_free_space_floor_bytes: Some(floor_bytes),
            ..HostStatus::default()
        })
        .await
        .expect("publish host capacity");
}

async fn await_host_capacity(daemon: &Arc<InProcessDaemon>, host_id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let capacity_available = daemon
                .resource_backend()
                .including_replicas::<Host>("flotilla")
                .list()
                .await
                .expect("list federated hosts")
                .items
                .into_iter()
                .any(|source| {
                    source.object.metadata.name == host_id
                        && source.object.status.is_some_and(|status| status.admission_free_space_floor_bytes.is_some())
                });
            if capacity_available {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host capacity should replicate");
}

async fn seed_target_placement_policy(topology: &InMemoryRequestTopology, namespace: &str, policy_name: &str) {
    let policy =
        topology.leader.resource_backend().using::<PlacementPolicy>(namespace).get(policy_name).await.expect("origin placement policy");
    topology
        .follower
        .resource_backend()
        .using::<PlacementPolicy>(namespace)
        .create(&InputMeta::builder().name(policy_name.to_string()).build(), &policy.spec)
        .await
        .expect("placement host should register its local placement policy");
}

fn convoy_spec(workflow_ref: &str, role: &str) -> ConvoySpec {
    ConvoySpec::builder()
        .workflow_ref(workflow_ref.to_string())
        .project_ref("flotilla".to_string())
        .role(role.to_string())
        .generation(1)
        .build()
}

fn convoy_meta(record_name: &str, role: &str) -> InputMeta {
    InputMeta::builder()
        .name(record_name.to_string())
        .labels(BTreeMap::from([
            (PROJECT_LABEL.to_string(), "flotilla".to_string()),
            (ROLE_LABEL.to_string(), role.to_string()),
            (GENERATION_LABEL.to_string(), "1".to_string()),
        ]))
        .build()
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
    seed_host_capacity(&follower, 2 * 1024 * 1024 * 1024, 1024 * 1024 * 1024).await;
    let follower_host_id = follower.local_host_id().expect("follower host identity").to_string();
    let principal_ref = PrincipalRef { namespace: "flotilla".to_string(), name: "alice".to_string() };
    let topology = spawn_in_memory_request_topology_stateful_with_surface(Arc::clone(&leader), follower, SurfaceDeclaration {
        principal_ref: principal_ref.clone(),
        character: SurfaceCharacter::Focal,
    })
    .await
    .expect("spawn named focal client topology");
    await_host_capacity(&leader, &follower_host_id).await;
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

    let backend = leader.resource_backend();
    let convoy =
        backend.using::<Convoy>("flotilla").get(&convoy_record_name(&backend, "alice-dispatch").await).await.expect("created convoy");
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
async fn resource_mutations_targeting_a_peer_modify_only_the_peer_store() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let follower_node_id = topology.follower.node_id().clone();
    let namespace = "flotilla";
    let name = "remote-template";
    let leader_templates = topology.leader.resource_backend().using::<WorkflowTemplate>(namespace);
    let follower_templates = topology.follower.resource_backend().using::<WorkflowTemplate>(namespace);

    let mut events = topology.leader.subscribe();
    let apply_id = topology
        .client
        .execute(Command {
            node_id: Some(follower_node_id.clone()),
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ResourceApply {
                namespace: namespace.to_string(),
                document: serde_json::json!({
                    "kind": "WorkflowTemplate",
                    "metadata": {"name": name},
                    "spec": {"vessels": []},
                }),
            },
        })
        .await
        .expect("dispatch peer resource apply");

    assert!(matches!(await_command_result(&mut events, apply_id).await, CommandValue::ResourceObject(_)));
    assert!(
        matches!(leader_templates.get(name).await, Err(ResourceError::NotFound { .. })),
        "peer-targeted apply must not create the resource in the caller's store"
    );
    follower_templates.get(name).await.expect("peer-targeted apply should create the resource in the peer store");

    let delete_id = topology
        .client
        .execute(Command {
            node_id: Some(follower_node_id),
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ResourceDelete {
                namespace: namespace.to_string(),
                kind: "workflowtemplates".to_string(),
                name: name.to_string(),
                replica_origin: None,
            },
        })
        .await
        .expect("dispatch peer resource delete");

    assert!(matches!(await_command_result(&mut events, delete_id).await, CommandValue::ResourceDeleted(_)));
    assert!(
        matches!(follower_templates.get(name).await, Err(ResourceError::NotFound { .. })),
        "peer-targeted delete should remove the resource from the peer store"
    );
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
        .create(&convoy_meta(convoy_name, convoy_name), &convoy_spec("scratch", convoy_name))
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
        .create(&convoy_meta(convoy_name, convoy_name), &convoy_spec("scratch", convoy_name))
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
        .create(&convoy_meta(convoy_name, convoy_name), &convoy_spec("scratch", convoy_name))
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
    let status = follower_convoys
        .get(convoy_name)
        .await
        .expect("remote-homed convoy should be retained")
        .status
        .expect("remote-homed convoy status");
    assert_eq!(status.phase, ResourceConvoyPhase::Abandoned);
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
        .create(&convoy_meta(convoy_name, convoy_name), &convoy_spec("scratch", convoy_name))
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
        .create(&convoy_meta(convoy_name, convoy_name), &convoy_spec("scratch", convoy_name))
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
async fn convoy_start_routes_to_placement_host_when_presentation_membership_is_stale() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    seed_host_capacity(&follower, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
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
    await_host_capacity(&topology.leader, &remote_host_id).await;
    seed_target_placement_policy(&topology, namespace, &placement_policy).await;

    seed_trusted_remote_convoy_project(&topology.follower, namespace).await;

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
                        .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                        .build(),
                ),
            },
        })
        .await
        .expect("origin should route remote placement despite stale presentation membership");

    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyStarted {
        name: "remote-work@flotilla".to_string(),
        attach_plan: None,
        binding: None
    });
    let origin_convoys = topology
        .leader
        .resource_backend()
        .using::<Convoy>(namespace)
        .list_matching_labels(&BTreeMap::from([(ROLE_LABEL.to_string(), "remote-work".to_string())]))
        .await
        .expect("list origin convoys");
    assert!(origin_convoys.items.is_empty(), "the origin must not own an always-on convoy record");
    let record_name = convoy_record_name(&topology.follower.resource_backend(), "remote-work").await;
    topology.follower.resource_backend().using::<Convoy>(namespace).get(&record_name).await.expect("placement host should own the convoy");
}

#[tokio::test]
async fn cross_host_convoy_start_uses_placement_hosts_credential_self_report() {
    let leader = empty_daemon_named("kiwi").await;
    let follower = empty_daemon_named("feta").await;
    seed_host_capacity(&follower, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["claude-code".to_string()]), &["cleat".to_string()]).await;
    let remote_host_id = follower.local_host_id().expect("follower host identity").to_string();
    let follower_hosts = follower.resource_backend().using::<Host>("flotilla");
    let remote_host = follower_hosts.get(&remote_host_id).await.expect("feta self-report");
    let mut remote_status = remote_host.status.expect("feta status");
    remote_status.capabilities.extend([
        (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
        (HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"])),
    ]);
    remote_status.heartbeat_at = Some(Utc::now());
    remote_status.daemon_generation = Some("feta-fresh-generation".to_string());
    remote_status.daemon_started_at = Some(Utc::now() - chrono::Duration::minutes(1));
    follower_hosts
        .update_status(&remote_host_id, &remote_host.metadata.resource_version, &remote_status)
        .await
        .expect("publish feta credential self-report");

    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let placement_policy = format!("host-direct-{remote_host_id}");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sources = topology
                .leader
                .resource_backend()
                .including_replicas::<Host>(namespace)
                .list()
                .await
                .expect("list kiwi host view")
                .items
                .into_iter()
                .filter(|source| source.object.metadata.name == remote_host_id)
                .collect::<Vec<_>>();
            let stale_local =
                sources.iter().any(|source| {
                    matches!(source.provenance, ResourceProvenance::Local)
                        && source.object.status.as_ref().is_some_and(|status| {
                            status.held_credentials().expect("decode local held credentials").is_empty() && status.ready
                        })
                });
            let fresh_replica = sources.iter().any(|source| {
                matches!(source.provenance, ResourceProvenance::Replica { .. })
                    && source.object.status.as_ref().is_some_and(|status| {
                        status.daemon_generation.as_deref() == Some("feta-fresh-generation")
                            && status.held_credentials().expect("decode replica held credentials").contains("claude-max")
                    })
            });
            if stale_local
                && fresh_replica
                && topology.leader.resource_backend().using::<PlacementPolicy>(namespace).get(&placement_policy).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("kiwi should reproduce the stale local and fresh feta Host rows");
    seed_target_placement_policy(&topology, namespace, &placement_policy).await;

    seed_trusted_remote_convoy_project(&topology.follower, namespace).await;
    let workflows = topology.follower.resource_backend().using::<WorkflowTemplate>(namespace);
    let workflow = workflows.get("remote-workflow").await.expect("remote workflow");
    let mut workflow_spec = workflow.spec;
    let flotilla_resources::CrewSource::Agent { selector, .. } = &mut workflow_spec.vessels[0].crew[0].source else {
        panic!("remote workflow crew should be an agent");
    };
    selector.adapter = Some("claude-code".to_string());
    workflows
        .update(&InputMeta::from(&workflow.metadata), &workflow.metadata.resource_version, &workflow_spec)
        .await
        .expect("select claude-code for the remote workflow");
    topology
        .follower
        .resource_backend()
        .definitions::<CredentialSpec>(namespace)
        .create(&InputMeta::builder().name("claude-max".to_string()).build(), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "crew@example.com".to_string() },
            source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    topology
        .follower
        .resource_backend()
        .definitions::<CredentialGrant>(namespace)
        .create(
            &InputMeta::builder().name("claude-max-trusted".to_string()).build(),
            &CredentialGrantSpec::builder()
                .selector(
                    CredentialGrantSelector::builder()
                        .stance(flotilla_resources::Stance::Trusted)
                        .projects(BTreeSet::from(["flotilla".to_string()]))
                        .build(),
                )
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("grant Claude credential to trusted workflow");

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
                        .name("kiwi-to-feta".to_string())
                        .branch("fix/kiwi-to-feta".to_string())
                        .placement_policy(placement_policy)
                        .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                        .build(),
                ),
            },
        })
        .await
        .expect("dispatch kiwi-to-feta convoy start");

    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyStarted {
        name: "kiwi-to-feta@flotilla".to_string(),
        attach_plan: None,
        binding: None
    });
}

#[tokio::test]
async fn routed_convoy_start_enforces_placement_host_capacity_before_persistence() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named_with_floor("follower", Some(1_000_000)).await;
    seed_host_capacity(&follower, 0, 1_000_000 * 1024 * 1024 * 1024).await;
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
    await_host_capacity(&topology.leader, &remote_host_id).await;
    seed_target_placement_policy(&topology, namespace, &placement_policy).await;
    seed_trusted_remote_convoy_project(&topology.follower, namespace).await;

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
                        .name("remote-disk-hungry".to_string())
                        .branch("fix/remote-disk-hungry".to_string())
                        .placement_policy(placement_policy)
                        .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                        .build(),
                ),
            },
        })
        .await
        .expect("admitting store should evaluate the convoy");

    let result = await_command_result(&mut events, command_id).await;
    let CommandValue::Error { message } = result else {
        panic!("expected target free-space refusal, got {result:?}");
    };
    assert!(message.contains("host `follower`"), "{message}");
    assert!(message.contains("free is below the 1000000.0 GiB floor"), "{message}");
    assert!(message.contains("reap settled convoys"), "{message}");
    assert!(message.contains("scripts/prune-target.sh"), "{message}");
    assert!(message.contains("pick another host"), "{message}");
    assert!(
        matches!(
            topology.leader.resource_backend().using::<Convoy>(namespace).get("remote-disk-hungry").await,
            Err(ResourceError::NotFound { .. })
        ),
        "refused admission must not create a convoy on the admitting store"
    );
    assert!(
        matches!(
            topology.follower.resource_backend().using::<Convoy>(namespace).get("remote-disk-hungry").await,
            Err(ResourceError::NotFound { .. })
        ),
        "refused admission must not create a convoy on the placement host"
    );
}

#[tokio::test]
async fn remote_docker_admission_fails_closed_without_target_capacity() {
    let daemon = empty_daemon_named_with_floor("leader", Some(0)).await;
    let namespace = "flotilla";
    seed_trusted_remote_convoy_project(&daemon, namespace).await;

    let hosts = daemon.resource_backend().using::<Host>(namespace);
    let host = hosts
        .create(&InputMeta::builder().name("remote-docker-host".to_string()).build(), &HostSpec {
            display_name: "remote-docker".to_string(),
        })
        .await
        .expect("create remote Docker host");
    hosts
        .update_status("remote-docker-host", &host.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"]))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("mark remote Docker host ready without capacity");
    daemon
        .resource_backend()
        .using::<PlacementPolicy>(namespace)
        .create(
            &InputMeta::builder().name("remote-docker".to_string()).build(),
            &PlacementPolicySpec::builder()
                .pool("cleat".to_string())
                .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                    host_ref: "remote-docker-host".to_string(),
                    image: "crew:latest".to_string(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("create remote Docker placement");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyStart {
                intent: Box::new(
                    ConvoyStartIntent::builder()
                        .project_ref("flotilla".to_string())
                        .name("remote-docker-work".to_string())
                        .branch("fix/remote-docker-work".to_string())
                        .placement_policy("remote-docker".to_string())
                        .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                        .build(),
                ),
            },
        })
        .await
        .expect("dispatch remote Docker admission");

    let result = await_command_result(&mut events, command_id).await;
    let CommandValue::Error { message } = result else {
        panic!("expected missing-capacity refusal, got {result:?}");
    };
    assert_eq!(message, "placement refused on host `remote-docker`: admission free-space floor is unavailable");
    assert!(
        matches!(daemon.resource_backend().using::<Convoy>(namespace).get("remote-docker-work").await, Err(ResourceError::NotFound { .. })),
        "missing remote Docker capacity must fail before convoy persistence"
    );

    let legacy_command_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "legacy-remote-docker-work".to_string(),
                workflow_ref: "remote-workflow".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: Some("remote-docker".to_string()),
                adopted_checkout: None,
            },
        })
        .await
        .expect("dispatch legacy remote Docker admission");
    let legacy_result = await_command_result(&mut events, legacy_command_id).await;
    let CommandValue::Error { message } = legacy_result else {
        panic!("expected legacy missing-capacity refusal, got {legacy_result:?}");
    };
    assert_eq!(message, "placement refused on host `remote-docker`: admission free-space floor is unavailable");
    assert!(
        matches!(
            daemon.resource_backend().using::<Convoy>(namespace).get("legacy-remote-docker-work").await,
            Err(ResourceError::NotFound { .. })
        ),
        "legacy missing remote Docker capacity must fail before convoy persistence"
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
