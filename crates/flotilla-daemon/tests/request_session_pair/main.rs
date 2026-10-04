use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    convoy_ensure::EnsureReconciler, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, PreparedCheckout,
};
use flotilla_core::{
    command_target::TargetHost,
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::{InProcessDaemon, WorkCredentialReconciler},
    providers::{
        discovery::{
            test_support::{fake_discovery, fake_discovery_with_provider_set, FakeDiscoveryProviders},
            EnvironmentBag,
        },
        environment::{ProvisionedEnvironment, ProvisionedMount},
        issue_tracker::IssueProvider,
        ChannelLabel, CommandOutput, CommandRunner,
    },
};
use flotilla_daemon::{
    blob_store::TieredBlobStore,
    runtime::{DaemonRuntime, RuntimeOptions},
    server::test_support::{
        apply_convoy_replica_feed, seed_trusted_remote_convoy_project, spawn_in_memory_request_mesh,
        spawn_in_memory_request_mesh_with_replication_kinds, spawn_in_memory_request_topology, spawn_in_memory_request_topology_stateful,
        spawn_in_memory_request_topology_stateful_with_caller, spawn_in_memory_request_topology_stateful_with_caller_and_blob_store,
        spawn_in_memory_request_topology_stateful_with_surface, InMemoryRequestMesh, InMemoryRequestTopology,
    },
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    qualified_path::HostId,
    test_support::TestIssue,
    CallerCrew, CallerProcess, Command, CommandAction, CommandCaller, CommandValue, ConvoyStartIntent, CrewCommandContext, DaemonEvent,
    EnvironmentId, EnvironmentStatus, HostName, ImageId, Issue, IssueChangeset, IssueRef, IssueSource, NodeInfo, PeerConnectionState,
    PrincipalRef, RepoSelector, ResourceRef, SurfaceCharacter, SurfaceDeclaration,
};
use flotilla_resources::{
    api_version, controller::ControllerLoop, list_resource_kind, Artifact, ArtifactSubjectBinding, Checkout, CheckoutPhase, CheckoutSpec,
    CheckoutStatus, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoyReconciler, ConvoySpec, ConvoyStatus, CredentialConsumer,
    CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource,
    CredentialSpec, CredentialSpecSpec, CrewCompletionExpectation, CrewSessionStatus, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState,
    DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, FreshCloneCheckoutSpec, FulfilmentKind, FulfilmentKindSpec, Host,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, InMemoryBackend, InputMeta, LifecycleAuthority,
    OwnerGarbageCollector, PlacementPolicy, PlacementPolicySpec, Regard, RepositoryKey, Resource, ResourceBackend, ResourceError,
    ResourceProvenance, Selector, SystemClock, TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionSource,
    TerminalSessionSpec, TerminalSessionStatus, Vessel, VesselRequirement, VesselSpec, WorkCompletionAuthority,
    WorkPhase as ResourceWorkPhase, WorkState, WorkflowSnapshot, WorkflowTemplate, WorkflowTemplateSpec, ACTUATOR_SOURCE_ROOT_ANNOTATION,
    AGENT_ADAPTERS_CAPABILITY, CONVOY_LABEL, GENERATION_LABEL, HELD_CREDENTIALS_CAPABILITY, PROJECT_LABEL, REGISTERED_RESOURCE_KINDS,
    ROLE_LABEL, VESSEL_LABEL,
};
use hegel::generators as gs;

struct TeardownCheckoutRuntime;

#[async_trait]
impl CheckoutRuntime for TeardownCheckoutRuntime {
    async fn create_worktree(&self, _: &str, _: &str, _: Option<&str>, _: &str) -> Result<PreparedCheckout, String> {
        Err("unexpected provisioning".into())
    }

    async fn create_fresh_clone(&self, _: &str, _: &str, _: Option<&str>, _: &str) -> Result<PreparedCheckout, String> {
        Err("unexpected provisioning".into())
    }

    async fn inspect_integration(
        &self,
        _: &flotilla_resources::ResourceObject<flotilla_resources::Checkout>,
        _: Option<&flotilla_resources::ResourceObject<Convoy>>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        Ok(Default::default())
    }

    async fn remove_checkout(&self, _: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

#[tokio::test]
async fn deleting_mis_homed_convoy_finalizes_checkout_at_its_home() {
    use flotilla_core::command_target::TargetHost;
    use flotilla_resources::{
        Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, FreshCloneCheckoutSpec, LifecycleAuthority, ACTUATOR_SOURCE_ROOT_ANNOTATION,
        CONVOY_LABEL,
    };

    let topology =
        spawn_in_memory_request_topology_stateful(empty_daemon_named("convoy-home").await, empty_daemon_named("checkout-home").await)
            .await
            .expect("spawn two-home router");
    let namespace = "flotilla";
    let parent_backend = topology.leader.resource_backend();
    let child_backend = topology.follower.resource_backend();
    let parent_root = topology.leader.node_id().clone();
    let child_root = topology.follower.node_id().clone();
    let convoys = parent_backend.clone().using::<Convoy>(namespace);
    convoys
        .create(
            &InputMeta::builder().name("old-convoy".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".into()]).build(),
            &convoy_spec("scratch", "old-convoy"),
        )
        .await
        .expect("create parent on dispatcher host");
    let checkouts = child_backend.clone().using::<Checkout>(namespace);
    let child = checkouts
        .create(
            &InputMeta::builder()
                .name("old-child".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "old-convoy".to_string())]))
                .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), parent_root.to_string())]))
                .finalizers(vec!["flotilla.work/checkout-cleanup".into()])
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
                env_ref: "host-direct-checkout-home".into(),
                r#ref: "feature/old-child".into(),
                base_ref: Some("main".into()),
                target_path: "/checkouts/old-child".into(),
                url: "https://github.com/flotilla-org/flotilla".into(),
            }),
        )
        .await
        .expect("create child at placement home");
    checkouts
        .update_status("old-child", &child.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("settle child before teardown");
    child_backend
        .replica_writer::<Convoy>(parent_root.clone(), namespace)
        .replace(&convoys.list().await.expect("list parent"), Utc::now())
        .await
        .expect("replicate parent to child home");
    parent_backend
        .replica_writer::<Checkout>(child_root.clone(), namespace)
        .replace(&checkouts.list().await.expect("list child"), Utc::now())
        .await
        .expect("replicate child to parent home");
    let child_delete = CommandAction::ResourceDelete {
        namespace: namespace.into(),
        kind: "Checkout".into(),
        name: "old-child".into(),
        replica_origin: None,
    };
    assert_eq!(
        topology.leader.resolve_command_target(&child_delete, None).await.expect("resolve child home").host,
        TargetHost::Node(child_root.clone()),
        "explicit child deletion routes to the checkout's home, not its parent's home",
    );
    let child_controller = tokio::spawn(
        ControllerLoop {
            primary: checkouts.clone(),
            secondaries: CheckoutReconciler::<TeardownCheckoutRuntime>::federated_secondary_watches(&child_backend, namespace),
            reconciler: CheckoutReconciler::new(Arc::new(TeardownCheckoutRuntime), child_backend.clone(), namespace)
                .with_federated_convoys(&child_backend, namespace),
            resync_interval: Duration::from_secs(3600),
            backend: child_backend.clone(),
        }
        .run(),
    );
    let parent_controller = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: ConvoyReconciler::federated_secondary_watches(&parent_backend, namespace),
            reconciler: ConvoyReconciler::new(parent_backend.definitions::<WorkflowTemplate>(namespace))
                .with_federated_checkouts(parent_backend.including_replicas::<Checkout>(namespace)),
            resync_interval: Duration::from_secs(3600),
            backend: parent_backend.clone(),
        }
        .run(),
    );
    let mut events = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.into()), name: "old-convoy".into(), force: true })
                .build(),
        )
        .await
        .expect("route parent delete");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::Ok);
    child_backend
        .replica_writer::<Convoy>(parent_root, namespace)
        .replace(&convoys.list().await.expect("list deleting parent"), Utc::now())
        .await
        .expect("replicate deletion to child home");
    eventually(Duration::from_secs(3), Duration::from_millis(10), "child home runs checkout finalizer", || async {
        matches!(checkouts.get("old-child").await, Err(ResourceError::NotFound { .. }))
    })
    .await;
    parent_backend
        .replica_writer::<Checkout>(child_root, namespace)
        .replace(&checkouts.list().await.expect("list drained child"), Utc::now())
        .await
        .expect("replicate drained child to parent home");
    eventually(Duration::from_secs(3), Duration::from_millis(10), "parent finalizer completes after child home cleanup", || async {
        matches!(convoys.get("old-convoy").await, Err(ResourceError::NotFound { .. }))
    })
    .await;
    child_controller.abort();
    parent_controller.abort();
}

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
    let daemon = InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host_name)).await;
    daemon
        .install_convoy_ensure_reconciler(Arc::new(
            EnsureReconciler::builder().resource_backend(daemon.resource_backend()).clock(Arc::new(SystemClock)).build(),
        ))
        .await;
    daemon
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
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"]))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            daemon_generation: Some("test-generation".to_string()),
            daemon_started_at: Some(Utc::now()),
            disk_free_bytes: Some(free_bytes),
            admission_free_space_floor_bytes: Some(floor_bytes),
            ..HostStatus::default()
        })
        .await
        .expect("publish host capacity");
}

async fn await_host_capacity(daemon: &Arc<InProcessDaemon>, host_id: &str) {
    eventually(Duration::from_secs(5), Duration::from_millis(10), "host capacity should replicate", || async {
        daemon.resource_backend().including_replicas::<Host>("flotilla").list().await.expect("list federated hosts").items.into_iter().any(
            |source| {
                source.object.metadata.name == host_id
                    && source.object.status.is_some_and(|status| status.admission_free_space_floor_bytes.is_some())
            },
        )
    })
    .await;
}

async fn await_placement_workflow(topology: &InMemoryRequestTopology, name: &str, adapter: Option<&str>) {
    eventually(Duration::from_secs(5), Duration::from_millis(10), "placement host should see the admitted workflow", || async {
        let workflow = topology.follower.resource_backend().definitions::<WorkflowTemplate>("flotilla").get(name).await;
        let project_ready =
            topology.follower.resource_backend().definitions::<flotilla_resources::Project>("flotilla").get("flotilla").await.is_ok();
        project_ready
            && workflow.is_ok_and(|workflow| {
                matches!(
                    &workflow.spec.vessels[0].crew[0].source,
                    flotilla_resources::CrewSource::Agent { selector, .. } if selector.adapter.as_deref() == adapter
                )
            })
    })
    .await;
}

async fn seed_target_placement_policy(topology: &InMemoryRequestTopology, namespace: &str, policy_name: &str) {
    let host_ref = topology.follower.local_host_id().expect("placement host identity").to_string();
    let policies = topology.follower.resource_backend().using::<PlacementPolicy>(namespace);
    let policy = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref, checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    policies
        .create(&InputMeta::builder().name(policy_name.to_string()).build(), &policy)
        .await
        .expect("placement host should register its local placement policy");
    topology
        .follower
        .resource_backend()
        .using::<FulfilmentKind>(namespace)
        .create(
            &InputMeta::builder().name(policy_name.to_string()).build(),
            &FulfilmentKindSpec::from_policy(&policy, "linux").expect("kind"),
        )
        .await
        .expect("placement host should register its fulfilment kind");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "home-authored placement policy should replicate to origin", || async {
        topology.leader.resource_backend().including_replicas::<PlacementPolicy>(namespace).get(policy_name).await.is_ok()
            && topology.leader.resource_backend().including_replicas::<FulfilmentKind>(namespace).get(policy_name).await.is_ok()
    })
    .await;
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
    await_command_finished(rx, command_id).await.1
}

async fn await_command_finished(
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    command_id: u64,
) -> (flotilla_protocol::NodeId, CommandValue) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id: id, node_id, result, .. } = rx.recv().await.expect("daemon event") {
                if id == command_id {
                    return (node_id, result);
                }
            }
        }
    })
    .await
    .expect("timed out waiting for command result")
}

async fn eventually<F, Fut>(timeout: Duration, interval: Duration, failure_message: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(timeout, async {
        loop {
            if predicate().await {
                break;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{failure_message}"));
}

async fn assert_no_orphaned_finalizers(hosts: &[Arc<InProcessDaemon>]) {
    for (host_index, host) in hosts.iter().enumerate() {
        let backend = host.resource_backend();
        for kind in REGISTERED_RESOURCE_KINDS {
            let listed = list_resource_kind(&backend, "flotilla", kind.plural).await.expect("list registered kind");
            for item in listed.value["items"].as_array().expect("resource list items") {
                let metadata = &item["metadata"];
                let deleting = !metadata["deletionTimestamp"].is_null();
                let finalizers = metadata["finalizers"].as_array();
                assert!(
                    !deleting || finalizers.is_none_or(Vec::is_empty),
                    "orphaned finalizers on host {host_index}, {} {}: {metadata}",
                    kind.kind,
                    metadata["name"],
                );
            }
        }
    }
}

async fn wait_for_host_resource_visibility(hosts: &[Arc<InProcessDaemon>], name: &str, should_exist: bool) {
    let failure_message = format!("Host {name} did not become {} on every host", if should_exist { "visible" } else { "absent" });
    eventually(Duration::from_secs(5), Duration::from_millis(10), &failure_message, || async {
        for host in hosts {
            let read = host.resource_backend().including_replicas::<Host>("flotilla").get(name).await;
            let matches_expected = if should_exist { read.is_ok() } else { matches!(read, Err(ResourceError::NotFound { .. })) };
            if !matches_expected {
                return false;
            }
        }
        true
    })
    .await;
}

#[tokio::test]
async fn finalizer_oracle_detects_a_stuck_registered_resource() {
    let host = empty_daemon_named("orphaned-finalizer").await;
    let hosts = vec![Arc::clone(&host)];
    let resources = host.resource_backend().using::<Host>("flotilla");
    resources
        .create(&InputMeta::builder().name("stuck".to_string()).finalizers(vec!["test/stuck".to_string()]).build(), &HostSpec::default())
        .await
        .expect("create resource with finalizer");
    resources.delete("stuck").await.expect("mark deleting");
    let detected = tokio::spawn(async move { assert_no_orphaned_finalizers(&hosts).await }).await;
    assert!(detected.is_err_and(|error| error.is_panic()), "orphaned finalizer escaped registry scan");
}

async fn paired_world_trace(home_index: usize, issuer_index: usize) -> Vec<String> {
    let hosts = vec![empty_daemon_named("pair-a").await, empty_daemon_named("pair-b").await, empty_daemon_named("pair-c").await];
    let home = Arc::clone(&hosts[home_index]);
    seed_host_capacity(&home, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    home.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("paired world mesh");
    let host_id = home.local_host_id().expect("home id").to_string();
    let policy_name = format!("host-direct-{host_id}");
    let policy = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref: host_id.clone(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    home.resource_backend()
        .using::<PlacementPolicy>("flotilla")
        .create(&InputMeta::builder().name(policy_name.clone()).build(), &policy)
        .await
        .expect("pair policy");
    home.resource_backend()
        .using::<FulfilmentKind>("flotilla")
        .create(&InputMeta::builder().name(policy_name.clone()).build(), &FulfilmentKindSpec::from_policy(&policy, "linux").expect("kind"))
        .await
        .expect("pair kind");
    for host in &mesh.hosts {
        seed_trusted_remote_convoy_project(host, "flotilla").await;
    }
    eventually(Duration::from_secs(5), Duration::from_millis(10), "pair placement visible", || async {
        let issuer = mesh.hosts[issuer_index].resource_backend();
        issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
            && issuer.including_replicas::<PlacementPolicy>("flotilla").get(&policy_name).await.is_ok()
            && issuer.including_replicas::<FulfilmentKind>("flotilla").get(&policy_name).await.is_ok()
    })
    .await;
    let mut trace = Vec::new();
    let mut events = mesh.hosts[issuer_index].subscribe();
    let id = mesh.clients[issuer_index]
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("paired-work".to_string())
                            .branch("test/paired-work".to_string())
                            .placement_policy(policy_name)
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("pair start dispatch");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *home.node_id());
    let started_name = match result {
        CommandValue::ConvoyStarted { name, attach_plan, binding } => {
            trace.push(format!("started:attach_plan={}:binding={}", attach_plan.is_some(), binding.is_some()));
            name
        }
        other => panic!("admission failed: {other:?}"),
    };
    for host in &mesh.hosts {
        let convoys = host.resource_backend().using::<Convoy>("flotilla").list().await.expect("convoys");
        trace.push(format!(
            "authored={:?}",
            convoys
                .items
                .iter()
                .map(|convoy| (
                    convoy.metadata.labels.get(ROLE_LABEL).cloned(),
                    convoy.status.as_ref().map(|status| format!("{:?}", status.phase)),
                ))
                .collect::<Vec<_>>()
        ));
    }
    let name = convoy_record_name(&home.resource_backend(), "paired-work").await;
    trace.push(format!("started-name={started_name}"));
    for (index, host) in mesh.hosts.iter().enumerate() {
        if index != home_index {
            apply_convoy_replica_feed(host, "flotilla", &name, home.host_name().clone()).await;
        }
    }
    for nudge in [false, true] {
        let mut events = mesh.hosts[issuer_index].subscribe();
        let id = mesh.clients[issuer_index]
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyResume {
                        namespace: Some("flotilla".into()),
                        name: name.clone(),
                        prompt: "continue".into(),
                        vessel: nudge.then(|| "work".to_string()),
                        role: nudge.then(|| "coder".to_string()),
                    })
                    .build(),
            )
            .await
            .expect("pair delivery dispatch");
        let (node, result) = await_command_finished(&mut events, id).await;
        assert_eq!(node, *home.node_id());
        let message = match result {
            CommandValue::Error { message } => message,
            other => panic!("delivery unexpectedly succeeded: {other:?}"),
        };
        trace.push(format!("{}: {}", if nudge { "nudge" } else { "resume" }, message.replace(&name, "<convoy>")));
    }
    let mut events = mesh.hosts[issuer_index].subscribe();
    let id = mesh.clients[issuer_index]
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: name.clone(), force: true })
                .build(),
        )
        .await
        .expect("pair convoy delete dispatch");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *home.node_id());
    assert_eq!(result, CommandValue::Ok);
    trace.push("convoy deleted".into());
    assert!(matches!(home.resource_backend().using::<Convoy>("flotilla").get(&name).await, Err(ResourceError::NotFound { .. })));
    home.resource_backend()
        .using::<Host>("flotilla")
        .create(&InputMeta::builder().name("pair-resource".to_string()).build(), &HostSpec::default())
        .await
        .expect("pair resource");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "pair resource visible", || async {
        mesh.hosts[issuer_index].resource_backend().including_replicas::<Host>("flotilla").get("pair-resource").await.is_ok()
    })
    .await;
    let mut events = mesh.hosts[issuer_index].subscribe();
    let id = mesh.clients[issuer_index]
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".into(),
                    kind: "hosts".into(),
                    name: "pair-resource".into(),
                    replica_origin: None,
                })
                .build(),
        )
        .await
        .expect("pair resource delete dispatch");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *home.node_id());
    let deleted = match result {
        CommandValue::ResourceDeleted(deleted) => deleted,
        other => panic!("resource deletion failed: {other:?}"),
    };
    trace.push(format!("resource-deleted:{}:{}:{}", deleted.kind, deleted.namespace, deleted.value["metadata"]["name"]));
    for host in &mesh.hosts {
        let remaining = host.resource_backend().using::<Convoy>("flotilla").list().await.expect("remaining convoys");
        trace.push(format!("remaining-convoys={}", remaining.items.len()));
    }
    wait_for_host_resource_visibility(&mesh.hosts, "pair-resource", false).await;
    assert_no_orphaned_finalizers(&mesh.hosts).await;
    trace
}

#[test]
fn cross_host_request_fits_one_megabyte_stack() {
    // One MiB leaves headroom below the default 2 MiB test stack on macOS.
    // If this fails on another target, measure the debug dispatch poll frames
    // before changing the budget.
    std::thread::Builder::new()
        .name("cross-host-stack-regression".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
            runtime.block_on(async { paired_world_trace(0, 1).await });
        })
        .expect("spawn reduced-stack worker")
        .join()
        .expect("cross-host request exceeded the 1 MB worker stack");
}

#[hegel::test]
fn generated_paired_world_host_independence(tc: hegel::TestCase) {
    let home = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let offset = tc.draw(gs::integers::<usize>().min_value(1).max_value(2));
    let remote = (home + offset) % 3;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    runtime.block_on(async {
        let at_home = paired_world_trace(home, home).await;
        let remote_desk = paired_world_trace(home, remote).await;
        assert_eq!(remote_desk, at_home, "home={home}, remote={remote}");
    });
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn three_host_request_mesh_connects_every_client_and_peer() {
    let hosts = vec![empty_daemon_named("desk").await, empty_daemon_named("placement").await, empty_daemon_named("other").await];
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("connect three-host mesh");
    assert_eq!(mesh.hosts.len(), 3);
    assert_eq!(mesh.clients.len(), 3);
    for (index, host) in mesh.hosts.iter().enumerate() {
        let routes = host.get_topology().await.expect("host topology").routes;
        assert_eq!(routes.iter().filter(|route| route.connected).count(), 2, "host {index} has a full mesh");
        assert_eq!(mesh.clients[index].get_topology().await.expect("client topology").routes.len(), 2);
    }
}

#[test]
fn partition_heal_and_request_runtime_restart_keep_deleted_resource_absent() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    runtime.block_on(async {
        // Dropping the mesh cuts peer sessions; rebuilding sub-meshes models a
        // stable network partition and a fresh request runtime, not packet loss.
        let hosts = vec![empty_daemon_named("home").await, empty_daemon_named("peer-b").await, empty_daemon_named("peer-c").await];
        let mesh = spawn_in_memory_request_mesh(hosts.clone()).await.expect("full mesh");
        let name = "deleted-during-partition";
        hosts[0]
            .resource_backend()
            .using::<Host>("flotilla")
            .create(&InputMeta::builder().name(name.to_string()).build(), &HostSpec::default())
            .await
            .expect("create home resource");
        wait_for_host_resource_visibility(&hosts, name, true).await;
        drop(mesh);
        let isolated = spawn_in_memory_request_mesh(vec![Arc::clone(&hosts[0])]).await.expect("isolated authority client");
        let other_component = spawn_in_memory_request_mesh(vec![Arc::clone(&hosts[1]), Arc::clone(&hosts[2])])
            .await
            .expect("connected peers behind partition");
        let mut events = hosts[0].subscribe();
        let command_id = isolated.clients[0]
            .execute(
                Command::builder()
                    .action(CommandAction::ResourceDelete {
                        namespace: "flotilla".to_string(),
                        kind: "hosts".to_string(),
                        name: name.to_string(),
                        replica_origin: None,
                    })
                    .build(),
            )
            .await
            .expect("delete at authority while partitioned");
        assert!(matches!(await_command_result(&mut events, command_id).await, CommandValue::ResourceDeleted(_)));
        drop(isolated);
        drop(other_component);
        let healed = spawn_in_memory_request_mesh(hosts.clone()).await.expect("heal full mesh");
        wait_for_host_resource_visibility(&hosts, name, false).await;
        drop(healed);
        let restarted = spawn_in_memory_request_mesh(hosts.clone()).await.expect("restart request runtimes");
        for host in &restarted.hosts {
            assert!(matches!(
                host.resource_backend().including_replicas::<Host>("flotilla").get(name).await,
                Err(ResourceError::NotFound { .. })
            ));
        }
    });
}

async fn spawn_tombstone_mesh(hosts: Vec<Arc<InProcessDaemon>>) -> Result<InMemoryRequestMesh, String> {
    // This scenario authors only Host records. Keep their real routed origin and
    // relay watches, without unrelated resource-watch fanout on every restart.
    spawn_in_memory_request_mesh_with_replication_kinds(hosts, Some(&[Host::API_PATHS.kind])).await
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tombstone_mesh_replicates_hosts_without_unrelated_watch_commands() {
    // The focused fixture must replicate its selected kind through the real
    // router, while leaving unrelated kinds local. The default mesh stays broad.
    for focused in [true, false] {
        let hosts = vec![empty_daemon_named("scope-home").await, empty_daemon_named("scope-peer").await];
        let mesh = if focused { spawn_tombstone_mesh(hosts.clone()).await } else { spawn_in_memory_request_mesh(hosts.clone()).await }
            .expect("scope mesh");
        hosts[0]
            .resource_backend()
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name("unrelated".to_string()).build(), &convoy_spec("scratch", "unrelated"))
            .await
            .expect("create unrelated kind");
        hosts[0]
            .resource_backend()
            .using::<Host>("flotilla")
            .create(&InputMeta::builder().name("selected".to_string()).build(), &HostSpec::default())
            .await
            .expect("create selected kind");
        wait_for_host_resource_visibility(&hosts, "selected", true).await;
        let convoys = hosts[1].resource_backend().including_replicas::<Convoy>("flotilla");
        if focused {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(matches!(convoys.get("unrelated").await, Err(ResourceError::NotFound { .. })));
        } else {
            eventually(Duration::from_secs(5), Duration::from_millis(10), "default mesh replicates unrelated kinds", || async {
                convoys.get("unrelated").await.is_ok()
            })
            .await;
        }
        drop(mesh);
    }
}

#[hegel::test]
fn generated_partition_heal_restart_preserves_tombstones(tc: hegel::TestCase) {
    // Each drawn step authors and deletes a new record across either a cut
    // peer session or a full-mesh runtime restart.
    let home_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let restart_before_delete = tc.draw(gs::booleans());
    let step_count = tc.draw(gs::integers::<usize>().min_value(1).max_value(2));
    let transitions = (0..step_count).map(|_| tc.draw(gs::booleans())).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    runtime.block_on(async {
        let hosts = vec![
            empty_daemon_named("transition-a").await,
            empty_daemon_named("transition-b").await,
            empty_daemon_named("transition-c").await,
        ];
        let mut mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("initial mesh");
        let name = "transition-tombstone";
        hosts[home_index]
            .resource_backend()
            .using::<Host>("flotilla")
            .create(&InputMeta::builder().name(name.to_string()).build(), &HostSpec::default())
            .await
            .expect("create at home");
        wait_for_host_resource_visibility(&hosts, name, true).await;
        if restart_before_delete {
            drop(mesh);
            mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("restart before delete");
        }
        drop(mesh);
        let isolated = spawn_tombstone_mesh(vec![Arc::clone(&hosts[home_index])]).await.expect("isolate home");
        let others = hosts.iter().enumerate().filter(|(index, _)| *index != home_index).map(|(_, host)| Arc::clone(host)).collect();
        let other_component = spawn_tombstone_mesh(others).await.expect("other component");
        let mut events = hosts[home_index].subscribe();
        let id = isolated.clients[0]
            .execute(
                Command::builder()
                    .action(CommandAction::ResourceDelete {
                        namespace: "flotilla".into(),
                        kind: "hosts".into(),
                        name: name.into(),
                        replica_origin: None,
                    })
                    .build(),
            )
            .await
            .expect("delete during partition");
        assert!(matches!(await_command_result(&mut events, id).await, CommandValue::ResourceDeleted(_)));
        drop(isolated);
        drop(other_component);
        let mut mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("heal");
        wait_for_host_resource_visibility(&hosts, name, false).await;
        for (step, partition_again) in transitions.into_iter().enumerate() {
            let step_name = format!("transition-step-{step}");
            hosts[home_index]
                .resource_backend()
                .using::<Host>("flotilla")
                .create(&InputMeta::builder().name(step_name.clone()).build(), &HostSpec::default())
                .await
                .expect("create next resource at home");
            wait_for_host_resource_visibility(&hosts, &step_name, true).await;
            drop(mesh);
            if partition_again {
                let isolated = spawn_tombstone_mesh(vec![Arc::clone(&hosts[home_index])]).await.expect("repeat partition");
                let others = hosts.iter().enumerate().filter(|(index, _)| *index != home_index).map(|(_, host)| Arc::clone(host)).collect();
                let other_component = spawn_tombstone_mesh(others).await.expect("repeat other component");
                let mut events = hosts[home_index].subscribe();
                let id = isolated.clients[0]
                    .execute(
                        Command::builder()
                            .action(CommandAction::ResourceDelete {
                                namespace: "flotilla".into(),
                                kind: "hosts".into(),
                                name: step_name.clone(),
                                replica_origin: None,
                            })
                            .build(),
                    )
                    .await
                    .expect("delete during repeated partition");
                assert!(matches!(await_command_result(&mut events, id).await, CommandValue::ResourceDeleted(_)));
                drop(isolated);
                drop(other_component);
                mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("heal repeated partition");
            } else {
                mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("restart full mesh");
                let mut events = hosts[home_index].subscribe();
                let id = mesh.clients[home_index]
                    .execute(
                        Command::builder()
                            .action(CommandAction::ResourceDelete {
                                namespace: "flotilla".into(),
                                kind: "hosts".into(),
                                name: step_name.clone(),
                                replica_origin: None,
                            })
                            .build(),
                    )
                    .await
                    .expect("delete after restart");
                let (node, result) = await_command_finished(&mut events, id).await;
                assert_eq!(node, *hosts[home_index].node_id());
                assert!(matches!(result, CommandValue::ResourceDeleted(_)));
            }
            wait_for_host_resource_visibility(&hosts, &step_name, false).await;
            wait_for_host_resource_visibility(&hosts, name, false).await;
            assert_no_orphaned_finalizers(&hosts).await;
        }
        drop(mesh);
    })
}

#[test]
fn generated_session_lookup_covers_remote_home_for_local_only_fault_model() {
    // Generator coverage control: a local-only lookup fails for a drawn remote home.
    // The request-level session rows exercise production routing separately.
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hegel::Hegel::new(|tc: hegel::TestCase| {
            let home = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
            runtime.block_on(async {
                let hosts = vec![
                    empty_daemon_named("session-a").await,
                    empty_daemon_named("session-b").await,
                    empty_daemon_named("session-c").await,
                ];
                let mesh = spawn_in_memory_request_mesh(hosts).await.expect("request mesh");
                let name = "remote-session";
                mesh.hosts[home]
                    .resource_backend()
                    .using::<TerminalSession>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.to_string()).build(),
                        &TerminalSessionSpec::builder()
                            .env_ref(mesh.hosts[home].local_environment_id().to_string())
                            .role("coder".to_string())
                            .source(TerminalSessionSource::Tool { command: "shell".to_string() })
                            .cwd("/tmp".to_string())
                            .pool("cleat".to_string())
                            .build(),
                    )
                    .await
                    .expect("home session");
                eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees federated session", || async {
                    mesh.hosts[issuer].resource_backend().including_replicas::<TerminalSession>("flotilla").get(name).await.is_ok()
                })
                .await;
                let local_only = mesh.hosts[issuer].resource_backend().using::<TerminalSession>("flotilla").get(name).await.is_ok();
                assert!(local_only, "local-only TerminalSession lookup missed a session homed on {home} from issuer {issuer}");
            });
        })
        .settings(hegel::Settings::new().test_cases(12).seed(Some(2318)))
        .run();
    }));
    assert!(failure.is_err(), "the local-only TerminalSession fault escaped the generated CI budget");
}

#[test]
fn generated_admission_covers_remote_placement_for_origin_first_fault_model() {
    // Generator coverage control: origin-first targeting differs from production
    // placement targeting for a drawn remote issuer.
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hegel::Hegel::new(|tc: hegel::TestCase| {
            let placement_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let issuer_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
            runtime.block_on(async {
                let hosts = vec![
                    empty_daemon_named("admission-a").await,
                    empty_daemon_named("admission-b").await,
                    empty_daemon_named("admission-c").await,
                ];
                let placement = Arc::clone(&hosts[placement_index]);
                seed_host_capacity(&placement, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
                let mesh = spawn_in_memory_request_mesh(hosts).await.expect("request mesh");
                let host_id = placement.local_host_id().expect("placement host id").to_string();
                let policy_name = format!("host-direct-{host_id}");
                let policy = PlacementPolicySpec::builder()
                    .pool("cleat".to_string())
                    .host_direct(HostDirectPlacementPolicySpec {
                        host_ref: host_id.clone(),
                        checkout: HostDirectPlacementPolicyCheckout::Worktree,
                    })
                    .build();
                placement
                    .resource_backend()
                    .using::<PlacementPolicy>("flotilla")
                    .create(&InputMeta::builder().name(policy_name.clone()).build(), &policy)
                    .await
                    .expect("placement policy");
                eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees placement", || async {
                    let issuer = mesh.hosts[issuer_index].resource_backend();
                    issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
                        && issuer.including_replicas::<PlacementPolicy>("flotilla").get(&policy_name).await.is_ok()
                })
                .await;
                let action = CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("ordering-fault".to_string())
                            .branch("test/ordering-fault".to_string())
                            .placement_policy(policy_name)
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                };
                let issuer = &mesh.hosts[issuer_index];
                let correct = issuer.resolve_command_target(&action, None).await.expect("placement target").host;
                let origin_first =
                    issuer.resource_mutation_origin(&action).await.expect("origin lookup").map_or(TargetHost::Local, TargetHost::Node);
                assert_eq!(origin_first, correct, "origin-first ordering lost placement {placement_index} from issuer {issuer_index}");
            });
        })
        .settings(hegel::Settings::new().test_cases(12).seed(Some(2335)))
        .run();
    }));
    assert!(failure.is_err(), "the #2335 origin-first fault escaped the generated CI budget");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn forced_convoy_teardown_cascades_to_checkout_on_another_host() {
    let hosts = vec![empty_daemon_named("home-a").await, empty_daemon_named("checkout-b").await, empty_daemon_named("desk-c").await];
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("request mesh");
    let namespace = "flotilla";
    let convoy_name = "cross-home-convoy";
    mesh.hosts[0]
        .resource_backend()
        .using::<Convoy>(namespace)
        .create(
            &{
                let mut meta = convoy_meta(convoy_name, convoy_name);
                meta.finalizers.push(flotilla_resources::CONVOY_TEARDOWN_FINALIZER.to_string());
                meta
            },
            &ConvoySpec::builder()
                .workflow_ref("scratch".to_string())
                .project_ref("flotilla".to_string())
                .role(convoy_name.to_string())
                .adopted_checkout_refs(BTreeMap::from([(RepositoryKey("test-repo".to_string()), "checkout-on-b".to_string())]))
                .build(),
        )
        .await
        .expect("convoy on A");
    mesh.hosts[1]
        .resource_backend()
        .using::<Checkout>(namespace)
        .create(
            &InputMeta::builder()
                .name("checkout-on-b".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
                .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), mesh.hosts[0].node_id().to_string())]))
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                r#ref: "test/cross-home".to_string(),
                target_path: "/tmp/cross-home".to_string(),
                repo_ref: RepositoryKey("test-repo".to_string()),
                env_ref: "host-direct-checkout-b".to_string(),
                base_ref: Some("main".to_string()),
                url: "https://example.test/repo".to_string(),
            }),
        )
        .await
        .expect("checkout on B");
    let checkout = mesh.hosts[1].resource_backend().using::<Checkout>(namespace).get("checkout-on-b").await.expect("checkout");
    mesh.hosts[1]
        .resource_backend()
        .using::<Checkout>(namespace)
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/tmp/cross-home".to_string()),
            ..Default::default()
        })
        .await
        .expect("ready checkout");
    let checkout_backend = mesh.hosts[1].resource_backend();
    let checkout_controller = ControllerLoop {
        primary: checkout_backend.using::<Checkout>(namespace),
        secondaries: CheckoutReconciler::<TeardownCheckoutRuntime>::federated_secondary_watches(&checkout_backend, namespace),
        reconciler: CheckoutReconciler::new(Arc::new(TeardownCheckoutRuntime), checkout_backend.clone(), namespace)
            .with_federated_convoys(&checkout_backend, namespace),
        resync_interval: Duration::from_millis(20),
        backend: checkout_backend,
    };
    let checkout_task = tokio::spawn(checkout_controller.run());
    let convoy_backend = mesh.hosts[0].resource_backend();
    let convoy_controller = ControllerLoop {
        primary: convoy_backend.using::<Convoy>(namespace),
        secondaries: ConvoyReconciler::federated_secondary_watches(&convoy_backend, namespace),
        reconciler: ConvoyReconciler::new(convoy_backend.definitions::<WorkflowTemplate>(namespace))
            .with_checkouts(convoy_backend.using::<Checkout>(namespace))
            .with_federated_checkouts(convoy_backend.including_replicas::<Checkout>(namespace)),
        resync_interval: Duration::from_millis(20),
        backend: convoy_backend,
    };
    let convoy_task = tokio::spawn(convoy_controller.run());
    eventually(Duration::from_secs(5), Duration::from_millis(10), "desk sees convoy", || async {
        let checkout = mesh.hosts[1].resource_backend().using::<Checkout>(namespace).get("checkout-on-b").await;
        checkout.is_ok_and(|checkout| checkout.metadata.finalizers.iter().any(|finalizer| finalizer == "flotilla.work/checkout-cleanup"))
            && mesh.hosts[2].resource_backend().including_replicas::<Convoy>(namespace).get(convoy_name).await.is_ok()
            && mesh.hosts[1].resource_backend().including_replicas::<Convoy>(namespace).get(convoy_name).await.is_ok()
            && mesh.hosts[0].resource_backend().including_replicas::<Checkout>(namespace).get("checkout-on-b").await.is_ok()
    })
    .await;
    let collector = OwnerGarbageCollector::new(mesh.hosts[1].resource_backend(), namespace);
    let collector_task = tokio::spawn(async move { collector.run(Duration::from_millis(20)).await });
    apply_convoy_replica_feed(&mesh.hosts[2], namespace, convoy_name, mesh.hosts[0].host_name().clone()).await;
    let mut events = mesh.hosts[2].subscribe();
    let command_id = mesh.clients[2]
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true })
                .build(),
        )
        .await
        .expect("dispatch forced teardown");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::Ok);
    eventually(Duration::from_secs(5), Duration::from_millis(10), "#2343: checkout on B and convoy on A must both finalize", || async {
        matches!(
            mesh.hosts[1].resource_backend().using::<Checkout>(namespace).get("checkout-on-b").await,
            Err(ResourceError::NotFound { .. })
        ) && matches!(
            mesh.hosts[0].resource_backend().using::<Convoy>(namespace).get(convoy_name).await,
            Err(ResourceError::NotFound { .. })
        )
    })
    .await;
    assert_no_orphaned_finalizers(&mesh.hosts).await;
    collector_task.abort();
    checkout_task.abort();
    convoy_task.abort();
}

#[hegel::test]
fn generated_convoy_admission_is_homed_on_placement(tc: hegel::TestCase) {
    let placement_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let issuing_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let deleting_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    runtime.block_on(async {
        let hosts = vec![empty_daemon_named("host-a").await, empty_daemon_named("host-b").await, empty_daemon_named("host-c").await];
        let placement = Arc::clone(&hosts[placement_index]);
        seed_host_capacity(&placement, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
        placement.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
        let mesh = spawn_in_memory_request_mesh(hosts).await.expect("request mesh");
        let host_id = placement.local_host_id().expect("placement host identity").to_string();
        let policy_name = format!("host-direct-{host_id}");
        let policy = PlacementPolicySpec::builder()
            .pool("cleat".to_string())
            .host_direct(HostDirectPlacementPolicySpec { host_ref: host_id.clone(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
            .build();
        placement
            .resource_backend()
            .using::<PlacementPolicy>("flotilla")
            .create(&InputMeta::builder().name(policy_name.clone()).build(), &policy)
            .await
            .expect("placement policy");
        placement
            .resource_backend()
            .using::<FulfilmentKind>("flotilla")
            .create(
                &InputMeta::builder().name(policy_name.clone()).build(),
                &FulfilmentKindSpec::from_policy(&policy, "linux").expect("kind"),
            )
            .await
            .expect("fulfilment kind");
        for host in &mesh.hosts {
            seed_trusted_remote_convoy_project(host, "flotilla").await;
        }
        eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees placement", || async {
            let issuer = mesh.hosts[issuing_index].resource_backend();
            issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
                && issuer.including_replicas::<PlacementPolicy>("flotilla").get(&policy_name).await.is_ok()
                && issuer.including_replicas::<FulfilmentKind>("flotilla").get(&policy_name).await.is_ok()
        })
        .await;
        let mut events = mesh.hosts[issuing_index].subscribe();
        let command_id = mesh.clients[issuing_index]
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyStart {
                        intent: Box::new(
                            ConvoyStartIntent::builder()
                                .project_ref("flotilla".to_string())
                                .name("generated-work".to_string())
                                .branch("test/generated-work".to_string())
                                .placement_policy(policy_name)
                                .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                                .build(),
                        ),
                    })
                    .build(),
            )
            .await
            .expect("dispatch admission");
        let result = await_command_result(&mut events, command_id).await;
        assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "admission failed: {result:?}");
        for (index, host) in mesh.hosts.iter().enumerate() {
            let count = host.resource_backend().using::<Convoy>("flotilla").list().await.expect("local convoys").items.len();
            assert_eq!(count, usize::from(index == placement_index), "issuer={issuing_index}, placement={placement_index}, host={index}");
        }
        let convoy_name = convoy_record_name(&placement.resource_backend(), "generated-work").await;
        for (index, host) in mesh.hosts.iter().enumerate() {
            if index != placement_index {
                apply_convoy_replica_feed(host, "flotilla", &convoy_name, placement.host_name().clone()).await;
            }
        }
        let delivery_steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        for _ in 0..delivery_steps {
            let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let nudge = tc.draw(gs::booleans());
            let (prompt, vessel, role) =
                if nudge { ("nudge", Some("work".to_string()), Some("coder".to_string())) } else { ("resume", None, None) };
            let mut events = mesh.hosts[issuer].subscribe();
            let command_id = mesh.clients[issuer]
                .execute(
                    Command::builder()
                        .action(CommandAction::ConvoyResume {
                            namespace: Some("flotilla".to_string()),
                            name: convoy_name.clone(),
                            prompt: prompt.to_string(),
                            vessel,
                            role,
                        })
                        .build(),
                )
                .await
                .expect("dispatch resume or nudge");
            let (node_id, result) = await_command_finished(&mut events, command_id).await;
            assert_eq!(node_id, *placement.node_id(), "{prompt} ran away from the convoy home");
            assert!(matches!(result, CommandValue::Error { .. }), "the no-crew case should refuse at home: {result:?}");
        }
        eventually(Duration::from_secs(5), Duration::from_millis(10), "all hosts see the convoy before deletion", || async {
            let mut visible_everywhere = true;
            for host in &mesh.hosts {
                visible_everywhere &= host.resource_backend().including_replicas::<Convoy>("flotilla").get(&convoy_name).await.is_ok();
            }
            visible_everywhere
        })
        .await;
        let mut delete_events = mesh.hosts[deleting_index].subscribe();
        let delete_id = mesh.clients[deleting_index]
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: convoy_name.clone(), force: true })
                    .build(),
            )
            .await
            .expect("dispatch forced delete");
        assert_eq!(await_command_result(&mut delete_events, delete_id).await, CommandValue::Ok);
        assert!(matches!(
            placement.resource_backend().using::<Convoy>("flotilla").get(&convoy_name).await,
            Err(ResourceError::NotFound { .. })
        ));
        let resource_name = "generated-resource";
        placement
            .resource_backend()
            .using::<Host>("flotilla")
            .create(&InputMeta::builder().name(resource_name.to_string()).build(), &HostSpec::default())
            .await
            .expect("create resource at home");
        eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees home resource", || async {
            mesh.hosts[deleting_index].resource_backend().including_replicas::<Host>("flotilla").get(resource_name).await.is_ok()
        })
        .await;
        let mut resource_events = mesh.hosts[deleting_index].subscribe();
        let resource_delete_id = mesh.clients[deleting_index]
            .execute(
                Command::builder()
                    .action(CommandAction::ResourceDelete {
                        namespace: "flotilla".to_string(),
                        kind: "hosts".to_string(),
                        name: resource_name.to_string(),
                        replica_origin: None,
                    })
                    .build(),
            )
            .await
            .expect("dispatch resource delete");
        let (node_id, result) = await_command_finished(&mut resource_events, resource_delete_id).await;
        assert_eq!(node_id, *placement.node_id(), "resource delete must execute at the resource home");
        assert!(matches!(result, CommandValue::ResourceDeleted(_)), "resource delete failed: {result:?}");
        assert!(matches!(
            placement.resource_backend().using::<Host>("flotilla").get(resource_name).await,
            Err(ResourceError::NotFound { .. })
        ));
        assert_no_orphaned_finalizers(&mesh.hosts).await;
    })
}

#[tokio::test]
async fn router_duplicate_create_scenarios_admit_one_generation_at_the_requested_host() {
    for (name, remote) in [("duplicate-local-create", false), ("duplicate-routed-create", true)] {
        let leader = empty_daemon_named("desk").await;
        let follower = empty_daemon_named("placement").await;
        for host in [&leader, &follower] {
            seed_host_capacity(host, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
        }
        let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("request topology");
        let home = if remote { &topology.follower } else { &topology.leader };
        home.resource_backend()
            .using::<WorkflowTemplate>("flotilla")
            .create(&InputMeta::builder().name("empty".to_string()).build(), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
            .await
            .expect("workflow at create host");
        let placement_policy = if remote {
            let host_id = home.local_host_id().expect("placement host identity").to_string();
            await_host_capacity(&topology.leader, &host_id).await;
            seed_target_placement_policy(&topology, "flotilla", "duplicate-placement").await;
            eventually(Duration::from_secs(5), Duration::from_millis(10), "origin sees workflow", || async {
                topology.leader.resource_backend().definitions::<WorkflowTemplate>("flotilla").get("empty").await.is_ok()
            })
            .await;
            Some("duplicate-placement".to_string())
        } else {
            None
        };
        let command = Command::builder()
            .node_id(home.node_id().clone())
            .action(CommandAction::ConvoyCreate {
                name: name.to_string(),
                workflow_ref: "empty".to_string(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy,
                adopted_checkout: None,
            })
            .build();
        let mut events = topology.leader.subscribe();
        let (first, second) = tokio::join!(topology.client.execute(command.clone()), topology.client.execute(command));
        let ids = [first.expect("first request"), second.expect("second request")];
        let results = tokio::time::timeout(Duration::from_secs(5), async {
            let mut results = Vec::new();
            while results.len() < 2 {
                if let DaemonEvent::CommandFinished { command_id, node_id, result, .. } = events.recv().await.expect("command event") {
                    if ids.contains(&command_id) {
                        assert_eq!(node_id, *home.node_id(), "{name} must settle on the requested host");
                        results.push(result);
                    }
                }
            }
            results
        })
        .await
        .expect("duplicate requests settle");
        assert_eq!(results.iter().filter(|result| matches!(result, CommandValue::ConvoyCreated { .. })).count(), 1, "{name}: {results:?}");
        assert_eq!(
            results.iter().filter(|result| matches!(result, CommandValue::Error { message } if message.contains("already exists"))).count(),
            1,
            "{name}: {results:?}"
        );
        for host in [&topology.leader, &topology.follower] {
            assert_eq!(
                host.resource_backend().using::<Convoy>("flotilla").list().await.expect("local convoys").items.len(),
                usize::from(host.node_id() == home.node_id()),
                "{name}: one generation on its admission host"
            );
        }
    }
}

#[tokio::test]
async fn router_homing_scenario_table_runs_mutations_at_the_record_home() {
    // Each row traverses the request dispatcher, an in-memory peer session
    // when needed, and the actual remote command router.
    #[derive(Clone, Copy)]
    enum Home {
        Desk,
        Placement,
    }
    #[derive(Clone, Copy)]
    enum RequestedTarget {
        Unspecified,
        StaleDesk,
    }
    #[derive(Clone, Copy)]
    enum Mutation {
        Delete,
        Abandon,
    }
    let scenarios = [
        ("delete-at-home", Home::Desk, RequestedTarget::Unspecified, Mutation::Delete),
        ("delete-from-desk", Home::Placement, RequestedTarget::Unspecified, Mutation::Delete),
        ("delete-with-stale-target", Home::Placement, RequestedTarget::StaleDesk, Mutation::Delete),
        ("abandon-at-home", Home::Desk, RequestedTarget::Unspecified, Mutation::Abandon),
        ("abandon-from-desk", Home::Placement, RequestedTarget::Unspecified, Mutation::Abandon),
    ];
    for (name, home, requested_target, mutation) in scenarios {
        let leader = empty_daemon_named("desk").await;
        let follower = empty_daemon_named("placement").await;
        let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("connect router scenario hosts");
        let namespace = "flotilla";
        let remote_home = matches!(home, Home::Placement);
        let home = match home {
            Home::Desk => &topology.leader,
            Home::Placement => &topology.follower,
        };
        let home_convoys = home.resource_backend().using::<Convoy>(namespace);
        home_convoys.create(&convoy_meta(name, name), &convoy_spec("scratch", name)).await.expect("seed convoy at its home");
        if remote_home {
            apply_convoy_replica_feed(&topology.leader, namespace, name, topology.follower_host.clone()).await;
        }

        let action = match mutation {
            Mutation::Abandon => {
                CommandAction::ConvoyAbandon { namespace: Some(namespace.into()), name: name.into(), reason: "accepted loss".into() }
            }
            Mutation::Delete => CommandAction::ConvoyDelete { namespace: Some(namespace.into()), name: name.into(), force: true },
        };
        let mut command = Command::builder().action(action).build();
        if matches!(requested_target, RequestedTarget::StaleDesk) {
            command.node_id = Some(topology.leader.node_id().clone());
        }
        let mut events = topology.leader.subscribe();
        let command_id = topology.client.execute(command).await.expect("dispatch scenario");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id: id, node_id, result, .. } =
                    events.recv().await.expect("command result event")
                {
                    if id == command_id {
                        assert_eq!(node_id, *home.node_id(), "{name} ran away from its home");
                        break result;
                    }
                }
            }
        })
        .await
        .expect("scenario completes");
        if matches!(mutation, Mutation::Abandon) {
            assert!(matches!(result, CommandValue::ConvoyAbandoned { .. }), "{name}: {result:?}");
            assert_eq!(
                home_convoys.get(name).await.expect("abandoned home record").status.expect("abandoned status").phase,
                ResourceConvoyPhase::Abandoned,
                "{name}"
            );
        } else {
            assert_eq!(result, CommandValue::Ok, "{name}");
            assert!(matches!(home_convoys.get(name).await, Err(ResourceError::NotFound { .. })), "{name}");
        }
    }
}

#[tokio::test]
async fn router_delivery_scenario_table_reaches_remote_convoy_authority() {
    let scenarios = [
        ("ensure-roll", CommandAction::ConvoyEnsureRoll { namespace: "flotilla".into(), name: "remote-ensure".into() }),
        ("resume", CommandAction::ConvoyResume {
            namespace: Some("flotilla".into()),
            name: "remote-work".into(),
            prompt: "continue".into(),
            vessel: None,
            role: None,
        }),
        ("nudge", CommandAction::ConvoyResume {
            namespace: Some("flotilla".into()),
            name: "remote-work".into(),
            prompt: "check in".into(),
            vessel: Some("work".into()),
            role: Some("coder".into()),
        }),
        ("supervise", CommandAction::CrewSupervise {
            namespace: Some("flotilla".into()),
            convoy: "remote-work".into(),
            vessel: "work".into(),
            role: "coder".into(),
            operation: flotilla_protocol::CrewSupervisionAction::Resume,
            message: "continue".into(),
            actor_crew_id: None,
        }),
    ];
    for (name, action) in scenarios {
        let leader = empty_daemon_named("desk").await;
        let follower = empty_daemon_named("placement").await;
        let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("connect scenario hosts");
        let mut meta = convoy_meta("remote-work", "remote-work");
        meta.annotations.insert("flotilla.work/ensured-from".into(), "remote-ensure".into());
        topology
            .follower
            .resource_backend()
            .using::<Convoy>("flotilla")
            .create(&meta, &convoy_spec("scratch", "remote-work"))
            .await
            .expect("seed remote authority");
        apply_convoy_replica_feed(&topology.leader, "flotilla", "remote-work", topology.follower_host.clone()).await;
        topology
            .leader
            .resource_backend()
            .replica_writer::<Convoy>(topology.follower.node_id().clone(), "flotilla")
            .replace(
                &topology.follower.resource_backend().using::<Convoy>("flotilla").list().await.expect("authoritative convoys"),
                Utc::now(),
            )
            .await
            .expect("replicate convoy admission and ensure ownership");

        let mut events = topology.leader.subscribe();
        let command_id = topology.client.execute(Command::builder().action(action).build()).await.expect("dispatch scenario");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id: id, node_id, result, .. } =
                    events.recv().await.expect("command result event")
                {
                    if id == command_id {
                        assert_eq!(node_id, *topology.follower.node_id(), "{name} ran away from the convoy authority");
                        // No crew session was seeded: the authority should reject
                        // the request after delivery, rather than the desk doing so.
                        assert!(matches!(result, CommandValue::Error { .. }), "{name}: {result:?}");
                        break;
                    }
                }
            }
        })
        .await
        .expect("remote authority responds");
    }
}

#[tokio::test]
async fn convoy_delete_routes_to_the_home_and_its_tombstone_does_not_resurrect() {
    let kiwi = empty_daemon_named("kiwi").await;
    let feta = empty_daemon_named("feta").await;
    let namespace = "flotilla";
    let name = "feta-homed";
    feta.resource_backend()
        .using::<Convoy>(namespace)
        .create(
            &convoy_meta(name, name),
            &ConvoySpec::builder().workflow_ref("scratch".to_string()).project_ref("flotilla".to_string()).role(name.to_string()).build(),
        )
        .await
        .expect("create feta-homed convoy");

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect kiwi and feta");
    let action = CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: name.to_string(), force: true };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "feta-homed convoy should replicate to kiwi", || async {
        kiwi.resource_backend().including_replicas::<Convoy>(namespace).get(name).await.is_ok()
    })
    .await;
    apply_convoy_replica_feed(&kiwi, namespace, name, HostName::new("feta")).await;
    assert_eq!(
        kiwi.resolve_existing_convoy_target(&action).await.expect("resolve replica home").expect("remote target").home,
        HostName::new("feta")
    );

    let mut events = topology.leader.subscribe();
    let command_id = topology.client.execute(Command::builder().action(action).build()).await.expect("dispatch convoy delete from kiwi");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::Ok);

    eventually(Duration::from_secs(5), Duration::from_millis(10), "home deletion should propagate to kiwi", || async {
        let absent_at_home =
            matches!(feta.resource_backend().using::<Convoy>(namespace).get(name).await, Err(ResourceError::NotFound { .. }));
        let absent_from_peer =
            matches!(kiwi.resource_backend().including_replicas::<Convoy>(namespace).get(name).await, Err(ResourceError::NotFound { .. }));
        absent_at_home && absent_from_peer
    })
    .await;

    drop(topology);
    let reconnected = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("reconnect kiwi and feta");
    feta.resource_backend()
        .using::<Convoy>(namespace)
        .create(
            &convoy_meta("after-reconnect", "after-reconnect"),
            &ConvoySpec::builder()
                .workflow_ref("scratch".to_string())
                .project_ref("flotilla".to_string())
                .role("after-reconnect".to_string())
                .build(),
        )
        .await
        .expect("create convergence marker");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "reconnected replication should converge", || async {
        kiwi.resource_backend().including_replicas::<Convoy>(namespace).get("after-reconnect").await.is_ok()
    })
    .await;
    assert!(
        matches!(kiwi.resource_backend().including_replicas::<Convoy>(namespace).get(name).await, Err(ResourceError::NotFound { .. })),
        "the deleted convoy must not resurrect after federation reconnects"
    );
    drop(reconnected);
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

#[test]
fn convoy_creation_attributes_provenance_and_regard_to_the_surface_principal() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("request runtime");
    runtime.block_on(convoy_creation_attribution_scenario());
}

async fn convoy_creation_attribution_scenario() {
    let leader = empty_daemon_named("leader").await;
    seed_host_capacity(&leader, 2 * 1024 * 1024 * 1024, 1024 * 1024 * 1024).await;
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "alice-dispatch".to_string(),
                    workflow_ref: "empty".to_string(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: None,
                    adopted_checkout: None,
                })
                .build(),
        )
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

async fn assert_abandon_attribution(principal_ref: PrincipalRef, expected_authority: WorkCompletionAuthority, expected_actor: &str) {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful_with_surface(Arc::clone(&leader), follower, SurfaceDeclaration {
        principal_ref,
        character: SurfaceCharacter::Focal,
    })
    .await
    .expect("spawn attributed client topology");
    let convoys = leader.resource_backend().using::<Convoy>("flotilla");
    let role = "attributed-abandon";
    let created = convoys.create(&convoy_meta("attributed-abandon-g1", role), &convoy_spec("empty", role)).await.expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            work: BTreeMap::from([("implement".to_string(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
            ..Default::default()
        })
        .await
        .expect("seed convoy status");
    let mut events = leader.subscribe();

    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyAbandon {
                namespace: Some("flotilla".to_string()),
                name: role.to_string(),
                reason: "no longer needed".to_string(),
            },
        })
        .await
        .expect("dispatch abandon");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyAbandoned {
        name: role.to_string(),
        archives: Vec::new()
    });

    let status = convoys.get(&created.metadata.name).await.expect("abandoned convoy").status.expect("convoy status");
    assert_eq!(status.work["implement"].completion_authority, expected_authority);
    assert_eq!(status.message.as_deref(), Some(format!("abandoned by {expected_actor}: no longer needed").as_str()));
}

#[tokio::test]
async fn agent_abandon_is_attributed_to_the_acting_principal() {
    let principal = PrincipalRef { namespace: "flotilla".to_string(), name: "governor agent".to_string() };
    assert_abandon_attribution(principal.clone(), WorkCompletionAuthority::Principal(principal), "governor agent").await;
}

#[tokio::test]
async fn implicit_human_abandon_remains_a_human_override() {
    assert_abandon_attribution(PrincipalRef::implicit_for_namespace("flotilla"), WorkCompletionAuthority::HumanOverride, "human override")
        .await;
}

#[tokio::test]
async fn cross_namespace_implicit_human_abandon_remains_a_human_override() {
    assert_abandon_attribution(PrincipalRef::implicit_for_namespace("people"), WorkCompletionAuthority::HumanOverride, "human override")
        .await;
}

#[tokio::test]
async fn in_memory_mutation_preserves_socket_caller_in_status_and_explain() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let principal_ref = PrincipalRef { namespace: "flotilla".into(), name: "alice".into() };
    let caller = CommandCaller {
        principal_ref: principal_ref.clone(),
        process: Some(CallerProcess::builder().pid(4242).uid(1000).executable("/usr/bin/flotilla".to_string()).build()),
        crew: Some(
            CallerCrew::builder()
                .namespace("flotilla".to_string())
                .convoy("research".to_string())
                .vessel("research-work".to_string())
                .role("coder".to_string())
                .crew_id("crew-123".to_string())
                .build(),
        ),
    };
    let topology = spawn_in_memory_request_topology_stateful_with_caller(
        Arc::clone(&leader),
        follower,
        SurfaceDeclaration { principal_ref, character: SurfaceCharacter::Focal },
        caller.clone(),
    )
    .await
    .expect("spawn caller-attributed topology");
    let convoys = leader.resource_backend().using::<Convoy>("flotilla");
    let role = "attributed-status";
    let created = convoys.create(&convoy_meta("attributed-status-g1", role), &convoy_spec("empty", role)).await.expect("create convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            ..Default::default()
        })
        .await
        .expect("seed status");
    let mut events = leader.subscribe();
    let rejected_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyResume {
                    namespace: Some("flotilla".into()),
                    name: role.into(),
                    prompt: String::new(),
                    vessel: None,
                    role: None,
                })
                .build(),
        )
        .await
        .expect("dispatch rejected resume");
    assert!(matches!(await_command_result(&mut events, rejected_id).await, CommandValue::Error { .. }));
    assert!(
        convoys.get(&created.metadata.name).await.expect("convoy").status.expect("status").lifecycle_mutations.is_empty(),
        "a rejected mutation must not enter the audit trail"
    );

    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyAbandon { namespace: Some("flotilla".into()), name: role.into(), reason: "done".into() })
                .build(),
        )
        .await
        .expect("dispatch abandon");
    let _ = await_command_result(&mut events, command_id).await;

    let status = convoys.get(&created.metadata.name).await.expect("convoy").status.expect("status");
    assert_eq!(status.lifecycle_mutations[0].caller, caller);
    let value = topology
        .client
        .execute_query(
            Command::builder().action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".into()), name: role.into() }).build(),
            uuid::Uuid::nil(),
        )
        .await
        .expect("explain convoy");
    let CommandValue::ConvoyExplanation(explanation) = value else { panic!("expected convoy explanation") };
    assert_eq!(explanation.lifecycle_mutations[0].caller.crew.as_ref().expect("crew caller").crew_id, "crew-123");
}

#[tokio::test]
async fn artifact_requests_store_body_locally_and_route_envelope_to_convoy_home() {
    let leader = empty_daemon_named("artifact-crew-host").await;
    let follower = empty_daemon_named("artifact-home").await;
    let namespace = "flotilla";
    let convoy = "artifact-demo";
    let workspace = tempfile::tempdir().expect("crew workspace");
    follower
        .resource_backend()
        .using::<Convoy>(namespace)
        .create(&InputMeta::builder().name(convoy.to_string()).build(), &ConvoySpec::builder().workflow_ref("scratch".to_string()).build())
        .await
        .expect("create home convoy");
    let sessions = leader.resource_backend().using::<TerminalSession>(namespace);
    let session = sessions
        .create(
            &InputMeta::builder().name("terminal-artifact-coder".to_string()).build(),
            &TerminalSessionSpec::builder()
                .env_ref(leader.local_environment_id().to_string())
                .role("coder".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: vec![] },
                    context: Box::new(TerminalCrewContext {
                        namespace: namespace.into(),
                        convoy: convoy.into(),
                        vessel_ref: "artifact-demo-work".into(),
                    }),
                    message: None,
                })
                .cwd(workspace.path().to_string_lossy().into_owned())
                .pool("cleat".to_string())
                .build(),
        )
        .await
        .expect("create crew terminal");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
            crew: Some(
                CrewSessionStatus::builder()
                    .id("crew-artifact".to_string())
                    .adapter("codex".to_string())
                    .stance("trusted".to_string())
                    .build(),
            ),
            ..TerminalSessionStatus::default()
        })
        .await
        .expect("mark crew session");
    let caller = CommandCaller {
        principal_ref: PrincipalRef::implicit_for_namespace(namespace),
        process: None,
        crew: Some(
            CallerCrew::builder()
                .namespace(namespace.to_string())
                .convoy(convoy.to_string())
                .vessel("artifact-demo-work".to_string())
                .role("coder".to_string())
                .crew_id("crew-artifact".to_string())
                .build(),
        ),
    };
    let state = tempfile::tempdir().expect("blob state");
    let store = Arc::new(TieredBlobStore::new(state.path(), vec![]));
    let topology = spawn_in_memory_request_topology_stateful_with_caller_and_blob_store(
        Arc::clone(&leader),
        Arc::clone(&follower),
        SurfaceDeclaration::focal_for_namespace(namespace),
        caller,
        Arc::clone(&store),
    )
    .await
    .expect("connect crew and home");
    apply_convoy_replica_feed(&leader, namespace, convoy, topology.follower_host.clone()).await;

    let bytes = vec![0, 1, 2, 127, 255];
    let source = workspace.path().join("source.bin");
    let destination = workspace.path().join("result.bin");
    tokio::fs::write(&source, &bytes).await.expect("write crew source");
    let (address, digest, view_url) = topology
        .client
        .artifact_put(
            "review-round".into(),
            "head-1".into(),
            BTreeMap::from([("approved".into(), serde_json::json!(true))]),
            "application/octet-stream".into(),
            source.clone(),
        )
        .await
        .expect("put through dispatcher and remote home");
    assert_eq!(view_url, None);
    assert_eq!(
        topology.client.artifact_get(digest.clone(), destination.clone()).await.expect("get local body"),
        (bytes.len() as u64, None)
    );
    assert_eq!(tokio::fs::read(&destination).await.expect("read crew destination"), bytes);
    assert!(leader.resource_backend().using::<Artifact>(namespace).list().await.expect("leader artifacts").items.is_empty());
    let home_artifacts = follower.resource_backend().using::<Artifact>(namespace).list().await.expect("home artifacts");
    assert_eq!(home_artifacts.items.len(), 1);
    assert_eq!(home_artifacts.items[0].spec.producer, "coder");
    leader
        .resource_backend()
        .replica_writer::<Artifact>(follower.node_id().clone(), namespace)
        .replace(&home_artifacts, Utc::now())
        .await
        .expect("deliver envelope replica");
    assert_eq!(
        topology.client.artifact_get(address.clone(), destination.clone()).await.expect("get by address"),
        (bytes.len() as u64, None)
    );
    assert_eq!(tokio::fs::read(&destination).await.expect("read crew destination"), bytes);
    assert_eq!(
        topology.client.artifact_list(Some(convoy.into()), Some("review-round".into()), None).await.expect("list artifacts").len(),
        1
    );

    let home_resolver = follower.resource_backend().using::<Artifact>(namespace);
    let current = home_resolver.get(&home_artifacts.items[0].metadata.name).await.expect("home artifact");
    home_resolver.delete(&current.metadata.name).await.expect("remove home envelope");
    let mut conflicting = current.spec.clone();
    conflicting.subject = "different-head".into();
    home_resolver.create(&InputMeta::from(&current.metadata), &conflicting).await.expect("create conflicting home address");
    let error = topology
        .client
        .artifact_put("review-round".into(), "head-1".into(), BTreeMap::new(), "text/plain".into(), source)
        .await
        .expect_err("remote apply must report address conflict");
    assert!(error.contains("artifact address cannot change"), "unexpected routed error: {error}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactEnvironment {
    LocalHostDirect,
    RemoteHostDirect,
    Provisioned,
}

// #2503: enumerate all production environment-reference variants through the
// request dispatcher. Each must transfer artifacts and admit completion only
// after storing a decision ledger. No transport concurrency is involved here.
#[tokio::test]
async fn artifact_environment_reference_contract() {
    for (environment_kind, two_repositories) in [
        (ArtifactEnvironment::LocalHostDirect, false),
        (ArtifactEnvironment::RemoteHostDirect, false),
        (ArtifactEnvironment::Provisioned, false),
        (ArtifactEnvironment::RemoteHostDirect, true),
    ] {
        let leader = empty_daemon_named("artifact-crew-host").await;
        // Install the real credential collaborator while keeping automatic
        // resource reconciliation gated; this contract drives requests itself.
        let (_startup_tx, startup_ready) = tokio::sync::watch::channel(false);
        let runtime = DaemonRuntime::start_with_options(leader.clone(), leader.config_store(), None, RuntimeOptions {
            startup_ready: Some(startup_ready),
            ..Default::default()
        })
        .await
        .expect("runtime credential controller");
        let follower = empty_daemon_named("artifact-home").await;
        let namespace = "flotilla";
        let convoy = "artifact-demo";
        let workspace = tempfile::tempdir().expect("crew workspace");
        let mut convoy_spec = ConvoySpec::builder().workflow_ref("scratch".to_string()).build();
        if two_repositories {
            for (repo, number) in [("first", 42), ("second", 43)] {
                convoy_spec.repositories.push(
                    flotilla_resources::ConvoyRepositorySpec::builder()
                        .repo_ref(RepositoryKey(repo.into()))
                        .url(format!("https://github.com/acme/{repo}.git"))
                        .source_ref("main".into())
                        .target_ref("main".into())
                        .workspace_slug(repo.into())
                        .subpaths(vec![])
                        .build(),
                );
                convoy_spec.subjects.push(flotilla_resources::DeclaredSubject {
                    subject: flotilla_protocol::Subject {
                        kind: flotilla_protocol::SubjectKind::ChangeRequest,
                        source: IssueSource { service: "github.com".into(), scope: format!("acme/{repo}") },
                        id: number.to_string(),
                    },
                    relationship: flotilla_protocol::Relationship::Produces,
                    issue: None,
                    change_request: None,
                });
            }
            leader.set_work_credential_reconciler(Arc::new(ArtifactLedgerCredentials)).await;
        }
        leader
            .resource_backend()
            .using::<Convoy>(namespace)
            .create(&InputMeta::builder().name(convoy.to_string()).build(), &convoy_spec)
            .await
            .expect("create home convoy");
        let env_ref = match environment_kind {
            ArtifactEnvironment::LocalHostDirect => format!("host-direct-{}", leader.local_host_id().expect("local host")),
            ArtifactEnvironment::RemoteHostDirect => "host-direct-remote-artifact-host".to_string(),
            ArtifactEnvironment::Provisioned => "env-artifact-vessel".to_string(),
        };
        let comments = Arc::new(Mutex::new(BTreeMap::new()));
        let runner: Arc<dyn CommandRunner> =
            Arc::new(ArtifactContractRunner { root: workspace.path().to_path_buf(), comments: comments.clone() });
        if environment_kind == ArtifactEnvironment::RemoteHostDirect {
            leader
                .register_direct_environment_for_test(
                    EnvironmentId::new(&env_ref),
                    runner.clone(),
                    EnvironmentBag::new(),
                    Some(HostId::new("remote-artifact-host")),
                )
                .expect("register remote direct environment");
        } else if environment_kind == ArtifactEnvironment::Provisioned {
            leader
                .register_provisioned_environment(
                    EnvironmentId::new(&env_ref),
                    Arc::new(ArtifactContractEnvironment { id: EnvironmentId::new(&env_ref), image: ImageId::new("test-image"), runner }),
                    EnvironmentBag::new(),
                    None,
                )
                .expect("register provisioned handle");
        }
        // The resolved identity must accompany the runner so VCS and archive
        // operations use the same environment as artifact transfers.
        let resolved = leader.resolve_environment_ref(&env_ref).expect("registered environment");
        let expected_id = match environment_kind {
            ArtifactEnvironment::LocalHostDirect => leader.local_environment_id().clone(),
            ArtifactEnvironment::RemoteHostDirect | ArtifactEnvironment::Provisioned => EnvironmentId::new(&env_ref),
        };
        assert_eq!(resolved.id, expected_id);
        let convoys = leader.resource_backend().using::<Convoy>(namespace);
        let created = convoys.get(convoy).await.expect("convoy");
        convoys
            .update_status(convoy, &created.metadata.resource_version, &ConvoyStatus {
                phase: ResourceConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                    vessels: vec![VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![CrewSpec::builder()
                            .role("coder".into())
                            .source(CrewSource::Tool { command: "test".into() })
                            .completion_conditions(vec![CrewCompletionExpectation::artifact_exists(
                                "coder",
                                "decision-ledger",
                                ArtifactSubjectBinding::Convoy,
                            )])
                            .build()])
                        .build()],
                }),
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            })
            .await
            .expect("active crew work");
        leader
            .resource_backend()
            .using::<Vessel>(namespace)
            .create(&InputMeta::builder().name("artifact-demo-work".into()).build(), &VesselSpec {
                convoy_ref: convoy.into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            })
            .await
            .expect("crew vessel");
        let sessions = leader.resource_backend().using::<TerminalSession>(namespace);
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("terminal-artifact-coder".to_string())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), convoy.into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), "coder".into()),
                    ]))
                    .build(),
                &TerminalSessionSpec::builder()
                    .env_ref(env_ref)
                    .role("coder".to_string())
                    .source(TerminalSessionSource::Agent {
                        selector: Selector::for_capability("coding"),
                        brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: vec![] },
                        context: Box::new(TerminalCrewContext {
                            namespace: namespace.into(),
                            convoy: convoy.into(),
                            vessel_ref: "artifact-demo-work".into(),
                        }),
                        message: None,
                    })
                    .cwd(if environment_kind == ArtifactEnvironment::LocalHostDirect {
                        workspace.path().to_string_lossy().into_owned()
                    } else {
                        "/crew".into()
                    })
                    .pool("cleat".to_string())
                    .build(),
            )
            .await
            .expect("create crew terminal");
        sessions
            .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
                crew: Some(
                    CrewSessionStatus::builder()
                        .id("crew-artifact".to_string())
                        .adapter("codex".to_string())
                        .stance("trusted".to_string())
                        .build(),
                ),
                ..TerminalSessionStatus::default()
            })
            .await
            .expect("mark crew session");
        let caller = CommandCaller {
            principal_ref: PrincipalRef::implicit_for_namespace(namespace),
            process: None,
            crew: Some(
                CallerCrew::builder()
                    .namespace(namespace.to_string())
                    .convoy(convoy.to_string())
                    .vessel("artifact-demo-work".to_string())
                    .role("coder".to_string())
                    .crew_id("crew-artifact".to_string())
                    .build(),
            ),
        };
        let state = tempfile::tempdir().expect("blob state");
        let store = Arc::new(TieredBlobStore::new(state.path(), vec![]));
        let topology = spawn_in_memory_request_topology_stateful_with_caller_and_blob_store(
            Arc::clone(&leader),
            Arc::clone(&follower),
            SurfaceDeclaration::focal_for_namespace(namespace),
            caller,
            Arc::clone(&store),
        )
        .await
        .expect("connect crew and home");

        // #2503: production session references must support binary artifact put/get
        // and a decision ledger must admit completion on every environment kind.
        let bytes = vec![0, 1, 2, 127, 255];
        let source = workspace.path().join("source.bin");
        let destination = workspace.path().join("result.bin");
        tokio::fs::write(&source, &bytes).await.expect("source");
        let (address, digest, _) = topology
            .client
            .artifact_put("review-round".into(), "head-1".into(), BTreeMap::new(), "application/octet-stream".into(), "source.bin".into())
            .await
            .unwrap_or_else(|error| panic!("{environment_kind:?} put: {error}"));
        for reference in [address, digest] {
            assert_eq!(topology.client.artifact_get(reference, "result.bin".into()).await.expect("get"), (bytes.len() as u64, None));
            assert_eq!(tokio::fs::read(&destination).await.expect("destination"), bytes);
        }
        let completion = || {
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some("crew-artifact".into()), ..Default::default() },
                    message: Some("finished".into()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                })
                .build()
        };
        let mut events = leader.subscribe();
        let id = topology.client.execute(completion()).await.expect("dispatch completion");
        let result = await_command_result(&mut events, id).await;
        assert!(
            matches!(&result, CommandValue::Error { message } if message.contains("decision-ledger")),
            "completion requires the ledger artifact: {result:?}"
        );
        let ledger = b"## Decision ledger\n\n1. **Brief silence:** Test setup.\n- **Choice:** Exercise every environment.\n- **Alternative:** Only local.\n- **If asking were free:** Which environment?\n";
        let source = workspace.path().join("ledger.md");
        tokio::fs::write(&source, ledger).await.expect("ledger source");
        let (address, _, _) = topology
            .client
            .artifact_put("decision-ledger".into(), String::new(), BTreeMap::new(), "text/markdown".into(), "ledger.md".into())
            .await
            .expect("put ledger");
        topology.client.artifact_get(address.clone(), "result.bin".into()).await.expect("get ledger");
        if two_repositories {
            // #2527: both repositories receive the convoy ledger; retry cannot duplicate comments.
            assert_eq!(comments.lock().expect("comments").keys().cloned().collect::<Vec<_>>(), vec![
                "repos/acme/first/issues/42/comments",
                "repos/acme/second/issues/43/comments"
            ]);
            topology
                .client
                .artifact_put("decision-ledger".into(), String::new(), BTreeMap::new(), "text/markdown".into(), "ledger.md".into())
                .await
                .expect("retry ledger put");
            assert_eq!(comments.lock().expect("comments").len(), 2);
            let artifacts = leader.resource_backend().using::<Artifact>(namespace).list().await.expect("artifacts");
            let artifact = artifacts.items.iter().find(|artifact| artifact.spec.kind == "decision-ledger").expect("stored ledger");
            assert_eq!(artifact.spec.subject, convoy);
            assert_eq!(artifact.spec.summary["projection_count"], 2);
            for comment in comments.lock().expect("comments").values() {
                assert!(comment["body"].as_str().expect("body").starts_with(std::str::from_utf8(ledger).expect("UTF-8")));
            }
        }
        // #2498: a connected operator reads crew artifacts without a calling crew session.
        let operator = spawn_in_memory_request_topology_stateful_with_caller_and_blob_store(
            leader.clone(),
            follower.clone(),
            SurfaceDeclaration::focal_for_namespace(namespace),
            CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(namespace), process: None, crew: None },
            store.clone(),
        )
        .await
        .expect("operator surface");
        let operator_destination = workspace.path().join("operator-ledger.md");
        operator.client.artifact_get(address.clone(), operator_destination.clone()).await.expect("operator get ledger");
        let record = leader
            .resource_backend()
            .using::<Artifact>(namespace)
            .get(address.strip_prefix("artifact/").expect("artifact address"))
            .await
            .expect("ledger record");
        leader
            .resource_backend()
            .using::<Artifact>("other-project-namespace")
            .create(&InputMeta::builder().name(record.metadata.name.clone()).build(), &record.spec)
            .await
            .expect("other namespace artifact");
        topology
            .client
            .artifact_get(format!("artifact/{namespace}/{}", record.metadata.name), "result.bin".into())
            .await
            .expect("crew can qualify its own namespace");
        // Review #2536: namespace-qualified reads across the fleet belong to the operator.
        let error = topology
            .client
            .artifact_get(format!("artifact/other-project-namespace/{}", record.metadata.name), "denied.bin".into())
            .await
            .expect_err("crew cannot cross namespaces");
        assert!(error.contains("cannot cross namespaces"), "{error}");
        assert!(!workspace.path().join("denied.bin").exists());
        operator
            .client
            .artifact_get(format!("artifact/other-project-namespace/{}", record.metadata.name), operator_destination.clone())
            .await
            .expect("operator get qualified artifact");
        assert_eq!(tokio::fs::read(operator_destination).await.expect("operator file"), ledger);
        assert_eq!(tokio::fs::read(destination).await.expect("ledger destination"), ledger);
        // Observe only the completion below; earlier artifact requests can fill the broadcast buffer.
        let mut events = leader.subscribe();
        let id = topology.client.execute(completion()).await.expect("dispatch completion with ledger");
        assert!(matches!(await_command_result(&mut events, id).await, CommandValue::Ok), "ledger admits completion");
        assert_eq!(
            convoys.get(convoy).await.expect("completed convoy").status.expect("status").crew_work["work"]["coder"].phase,
            CrewWorkPhase::Done
        );
        runtime.shutdown();
    }
}

// Fake handle stands in for the container process boundary; file transfer uses
// the same runner contract as a registered remote direct environment.
struct ArtifactContractEnvironment {
    id: EnvironmentId,
    image: ImageId,
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl ProvisionedEnvironment for ArtifactContractEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn image(&self) -> &ImageId {
        &self.image
    }
    fn container_name(&self) -> Option<&str> {
        Some("artifact-contract")
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        vec![]
    }
    async fn status(&self) -> Result<EnvironmentStatus, String> {
        Ok(EnvironmentStatus::Running)
    }
    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(HashMap::new())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        self.runner.clone()
    }
    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}

// Fake SSH/container file-transfer boundary: `/crew` lives in a separate
// filesystem, so resolving a remote reference to the local runner cannot pass.
struct ArtifactContractRunner {
    root: PathBuf,
    comments: Arc<Mutex<BTreeMap<String, serde_json::Value>>>,
}

// Credential delivery is a process boundary; the test runner never reads a real token.
struct ArtifactLedgerCredentials;
#[async_trait]
impl WorkCredentialReconciler for ArtifactLedgerCredentials {
    async fn reconcile(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn ledger_delivery_environment(&self, _: &str, _: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::from([("GITHUB_TOKEN_FILE".into(), "/credential/token".into())]))
    }
}

#[async_trait]
impl CommandRunner for ArtifactContractRunner {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        assert_eq!(cmd, "sh");
        let endpoint = args.iter().find(|arg| arg.starts_with("repos/")).expect("comment endpoint").split('?').next().expect("endpoint");
        let comments = self.comments.lock().expect("comments");
        Ok(serde_json::json!([comments.get(endpoint).into_iter().collect::<Vec<_>>()]).to_string())
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        assert_eq!(cmd, "sh");
        let endpoint = args.iter().find(|arg| arg.starts_with("repos/")).expect("post endpoint");
        let mut comment: serde_json::Value = serde_json::from_slice(input).expect("comment JSON");
        comment["html_url"] = format!("https://github.com/{endpoint}#issuecomment-1").into();
        let mut comments = self.comments.lock().expect("comments");
        assert!(comments.insert(endpoint.to_string(), comment.clone()).is_none(), "duplicate ledger post");
        Ok(comment.to_string())
    }
    async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        Err("unexpected subprocess".into())
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let relative = source.strip_prefix("/crew").map_err(|error| error.to_string())?;
        tokio::fs::copy(self.root.join(relative), destination).await.map(|_| ()).map_err(|error| error.to_string())
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let relative = destination.strip_prefix("/crew").map_err(|error| error.to_string())?;
        tokio::fs::copy(source, self.root.join(relative)).await.map(|_| ()).map_err(|error| error.to_string())
    }
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
            Command::builder()
                .action(CommandAction::QueryHostStatus { target_environment_id: follower_environment_id.clone() })
                .node_id(follower_node_id.clone())
                .build(),
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
        .execute(
            Command::builder()
                .action(CommandAction::ResourceApply {
                    namespace: namespace.to_string(),
                    document: serde_json::json!({
                        "kind": "WorkflowTemplate",
                        "metadata": {"name": name},
                        "spec": {"vessels": []},
                    }),
                })
                .node_id(follower_node_id.clone())
                .build(),
        )
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
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: namespace.to_string(),
                    kind: "workflowtemplates".to_string(),
                    name: name.to_string(),
                    replica_origin: None,
                })
                .node_id(follower_node_id)
                .build(),
        )
        .await
        .expect("dispatch peer resource delete");

    assert!(matches!(await_command_result(&mut events, delete_id).await, CommandValue::ResourceDeleted(_)));
    assert!(
        matches!(
            topology.follower.resource_backend().definitions::<WorkflowTemplate>(namespace).get(name).await,
            Err(ResourceError::NotFound { .. })
        ),
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true })
                .build(),
        )
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true })
                .node_id(topology.leader.node_id().clone())
                .build(),
        )
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyAbandon {
                    namespace: Some(namespace.to_string()),
                    name: convoy_name.to_string(),
                    reason: "accepted loss".to_string(),
                })
                .build(),
        )
        .await
        .expect("dispatch hostless convoy abandon");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::ConvoyAbandoned {
        name: convoy_name.to_string(),
        archives: Vec::new()
    });
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyWorkForceComplete {
                    convoy: convoy_name.to_string(),
                    work: work_name.to_string(),
                    message: Some("done".to_string()),
                })
                .build(),
        )
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
    let replica_fixture = ResourceBackend::InMemory(InMemoryBackend::default());
    replica_fixture
        .using::<Convoy>(namespace)
        .create(
            &convoy_meta(convoy_name, convoy_name),
            &ConvoySpec::builder().workflow_ref("scratch".to_string()).role(convoy_name.to_string()).build(),
        )
        .await
        .expect("create stranded replica fixture");
    let replica_snapshot = replica_fixture.using::<Convoy>(namespace).list().await.expect("list stranded replica fixture");
    topology
        .leader
        .resource_backend()
        .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("feta-root"), namespace)
        .replace(
            &replica_snapshot,
            chrono::DateTime::parse_from_rfc3339("2026-08-21T11:22:33Z").expect("fixed last-seen time").with_timezone(&Utc),
        )
        .await
        .expect("seed stranded replica provenance");

    let message = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyAbandon {
                    namespace: Some(namespace.to_string()),
                    name: convoy_name.to_string(),
                    reason: "lost host".to_string(),
                })
                .build(),
        )
        .await
        .expect_err("unreachable convoy home should reject dispatch");

    assert_eq!(
        message,
        "convoy flotilla/stranded is homed at feta, last seen 2026-08-21T11:22:33+00:00; home is unreachable: \
         no routed node address found for host. Break glass: flotilla resource delete convoys stranded --namespace flotilla --host feta"
    );
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: true })
                .build(),
        )
        .await
        .expect("live peer route should take precedence over stale connection status");

    assert_eq!(await_command_result(&mut rx, command_id).await, CommandValue::Ok);
    assert!(
        matches!(follower_convoys.get(convoy_name).await, Err(ResourceError::NotFound { .. })),
        "remote-homed convoy should be deleted through the live peer route"
    );
}

async fn assert_convoy_start_routes_through_peer_session(caller: Option<CommandCaller>) {
    let leader = empty_daemon_named("kiwi").await;
    let follower = empty_daemon_named("feta").await;
    seed_host_capacity(&follower, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let expected_principal = caller.as_ref().map(|caller| caller.principal_ref.clone()).unwrap_or_default();
    let topology = match caller {
        Some(caller) => {
            spawn_in_memory_request_topology_stateful_with_caller(
                leader,
                follower,
                SurfaceDeclaration { principal_ref: caller.principal_ref.clone(), character: SurfaceCharacter::Focal },
                caller,
            )
            .await
        }
        None => spawn_in_memory_request_topology_stateful(leader, follower).await,
    }
    .expect("connect two in-process daemons over peer sessions");
    let namespace = "flotilla";
    let remote_host_id = topology.follower.local_host_id().expect("feta host identity").to_string();
    let placement_policy = format!("host-direct-{remote_host_id}");
    seed_target_placement_policy(&topology, namespace, &placement_policy).await;
    await_host_capacity(&topology.leader, &remote_host_id).await;
    seed_trusted_remote_convoy_project(&topology.leader, namespace).await;
    await_placement_workflow(&topology, "remote-workflow", None).await;

    let mut events = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("routed-work".to_string())
                            .branch("fix/routed-work".to_string())
                            .placement_policy(placement_policy)
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("dispatch convoy start on kiwi");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id: id, node_id, result, .. } = events.recv().await.expect("command event") {
                if id == command_id {
                    assert_eq!(node_id, *topology.follower.node_id(), "admission command executes on feta");
                    break result;
                }
            }
        }
    })
    .await
    .expect("routed admission finishes");
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "placement admission failed: {result:?}");

    let dispatcher = topology.leader.resource_backend();
    let placement = topology.follower.resource_backend();
    assert!(dispatcher.using::<Convoy>(namespace).list().await.expect("kiwi Convoys").items.is_empty());
    let convoys = placement.using::<Convoy>(namespace).list().await.expect("feta Convoys");
    assert_eq!(convoys.items.len(), 1, "one Convoy is homed on feta");
    let convoy = &convoys.items[0];
    assert_eq!(convoy.spec.dispatching_principal_ref, expected_principal, "crew caller reaches admission across the peer session");

    let controller = ControllerLoop {
        primary: placement.using::<Convoy>(namespace),
        secondaries: Vec::new(),
        reconciler: ConvoyReconciler::new(placement.definitions::<WorkflowTemplate>(namespace))
            .with_hosts(placement.including_replicas::<Host>(namespace))
            .with_vessels(placement.using::<Vessel>(namespace)),
        resync_interval: Duration::from_millis(20),
        backend: placement.clone(),
    };
    let controller_task = tokio::spawn(controller.run());
    eventually(Duration::from_secs(5), Duration::from_millis(10), "feta authors its Vessel", || async {
        let vessels = placement.using::<Vessel>(namespace).list().await.expect("feta Vessels");
        if !vessels.items.is_empty() {
            assert_eq!(vessels.items.len(), 1, "one Vessel is authored at its actuation host");
            assert_eq!(vessels.items[0].spec.convoy_ref, convoy.metadata.name);
            return true;
        }
        false
    })
    .await;
    controller_task.abort();
    assert_eq!(placement.using::<Vessel>(namespace).list().await.expect("feta Vessels").items.len(), 1);
    assert!(dispatcher.using::<Vessel>(namespace).list().await.expect("kiwi Vessels").items.is_empty());
}

#[tokio::test]
async fn convoy_start_routes_admission_to_placement_over_peer_session() {
    assert_convoy_start_routes_through_peer_session(None).await;
}

#[tokio::test]
async fn governor_crew_caller_routes_admission_to_placement_over_peer_session() {
    let caller = CommandCaller {
        principal_ref: PrincipalRef { namespace: "flotilla".to_string(), name: "governor".to_string() },
        process: None,
        crew: Some(
            CallerCrew::builder()
                .namespace("flotilla".to_string())
                .convoy("standing-governor".to_string())
                .vessel("governor-work".to_string())
                .role("governor".to_string())
                .crew_id("crew-governor".to_string())
                .build(),
        ),
    };
    assert_convoy_start_routes_through_peer_session(Some(caller)).await;
}

#[tokio::test]
async fn convoy_start_routes_to_placement_when_presentation_membership_is_stale() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    seed_host_capacity(&follower, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let remote_host_id = topology.follower.local_host_id().expect("follower host identity").to_string();
    let placement_policy = format!("host-direct-{remote_host_id}");

    seed_target_placement_policy(&topology, namespace, &placement_policy).await;
    await_host_capacity(&topology.leader, &remote_host_id).await;

    seed_trusted_remote_convoy_project(&topology.leader, namespace).await;
    await_placement_workflow(&topology, "remote-workflow", None).await;

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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("remote-work".to_string())
                            .branch("fix/remote-work".to_string())
                            .workflow_ref("remote-workflow".to_string())
                            .placement_policy(placement_policy)
                            .escalation_reason("remote host requested".to_string())
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("placement host should admit despite stale presentation membership");

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
    assert!(origin_convoys.items.is_empty(), "the dispatcher must not author the convoy");
    assert_eq!(
        topology.follower.resource_backend().using::<Convoy>(namespace).list().await.expect("placement host Convoys").items.len(),
        1
    );
}

// Behaviour (#2340): explicit and automatic starts route using placement identity,
// even when dispatcher observations lag. Destination credentials remain authoritative.
async fn lagging_start_placement_scenario(explicit: bool, destination_holds_credential: bool, destination_has_capacity: bool) {
    let leader = empty_daemon_named("kiwi").await;
    let follower = empty_daemon_named_with_floor("feta", (!destination_has_capacity).then_some(1_000_000)).await;
    seed_host_capacity(&follower, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["claude-code".to_string()]), &["cleat".to_string()]).await;
    let remote_host_id = follower.local_host_id().expect("follower host identity").to_string();
    let follower_hosts = follower.resource_backend().using::<Host>("flotilla");
    let remote_host = follower_hosts.get(&remote_host_id).await.expect("feta self-report");
    let mut remote_status = remote_host.status.expect("feta status");
    remote_status.capabilities.extend([
        (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
        (
            HELD_CREDENTIALS_CAPABILITY.to_string(),
            serde_json::json!(if destination_holds_credential { vec!["claude-max"] } else { vec![] }),
        ),
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
    seed_target_placement_policy(&topology, namespace, &placement_policy).await;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "kiwi should read feta's single home-authored Host replica", || async {
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
        let fresh_replica = sources.iter().any(|source| {
            matches!(source.provenance, ResourceProvenance::Replica { .. })
                && source.object.status.as_ref().is_some_and(|status| {
                    status.daemon_generation.as_deref() == Some("feta-fresh-generation")
                        && status.held_credentials().expect("decode replica held credentials").contains("claude-max")
                            == destination_holds_credential
                })
        });
        sources.len() == 1 && fresh_replica
    })
    .await;

    seed_trusted_remote_convoy_project(&topology.leader, namespace).await;
    let workflows = topology.leader.resource_backend().using::<WorkflowTemplate>(namespace);
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
        .leader
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
        .leader
        .resource_backend()
        .definitions::<CredentialGrant>(namespace)
        .create(
            &InputMeta::builder().name("claude-max-trusted".to_string()).build(),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("grant Claude credential to trusted workflow");

    await_placement_workflow(&topology, "remote-workflow", Some("claude-code")).await;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "placement host should see credential declarations", || async {
        let backend = topology.follower.resource_backend();
        backend.definitions::<CredentialSpec>(namespace).get("claude-max").await.is_ok()
            && backend.definitions::<CredentialGrant>(namespace).get("claude-max-trusted").await.is_ok()
    })
    .await;

    // This is the replication boundary: replace the dispatcher's snapshot with
    // an older self-report, without changing the authoritative destination.
    let mut lagging = topology.follower.resource_backend().using::<Host>(namespace).list().await.expect("destination hosts");
    let host = lagging.items.iter_mut().find(|host| host.metadata.name == remote_host_id).expect("destination host");
    let status = host.status.as_mut().expect("host status");
    status.disk_free_bytes = Some(0);
    status.admission_free_space_floor_bytes = None;
    status.ready = false;
    status.heartbeat_at = Some(Utc::now() - chrono::Duration::hours(1));
    status.capabilities.insert(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([]));
    status.capabilities.insert(
        HELD_CREDENTIALS_CAPABILITY.to_string(),
        serde_json::json!(if destination_holds_credential { vec![] } else { vec!["claude-max"] }),
    );
    topology
        .leader
        .resource_backend()
        .replica_writer::<Host>(topology.follower.node_id().clone(), namespace)
        .replace(&lagging, Utc::now())
        .await
        .expect("lag dispatcher host replica");
    let action = CommandAction::ConvoyStart {
        intent: Box::new(
            ConvoyStartIntent::builder()
                .project_ref("flotilla".to_string())
                .name("kiwi-to-feta".to_string())
                .branch("fix/kiwi-to-feta".to_string())
                .workflow_ref("remote-workflow".to_string())
                .maybe_placement_policy(explicit.then_some(placement_policy))
                .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                .build(),
        ),
    };
    // The router resolves the destination without accepting the stale refusal.
    assert_eq!(
        topology.leader.resolve_command_target(&action, None).await.expect("route lagging start").host,
        TargetHost::Placement(flotilla_protocol::qualified_path::HostId::new(&remote_host_id))
    );
    let mut events = topology.leader.subscribe();
    let command_id = topology.client.execute(Command::builder().action(action).build()).await.expect("dispatch kiwi-to-feta convoy start");

    let result = await_command_result(&mut events, command_id).await;
    if destination_holds_credential && destination_has_capacity {
        assert_eq!(result, CommandValue::ConvoyStarted { name: "kiwi-to-feta@flotilla".to_string(), attach_plan: None, binding: None });
    } else {
        let CommandValue::Error { message } = result else {
            panic!("expected credential refusal: {result:?}");
        };
        if destination_has_capacity {
            assert!(message.contains("does not hold"), "{message}");
        } else {
            assert!(message.contains("below the 1000000.0 GiB floor"), "{message}");
        }
    }
    for (daemon, expected) in
        [(&topology.leader, 0), (&topology.follower, usize::from(destination_holds_credential && destination_has_capacity))]
    {
        assert_eq!(daemon.resource_backend().using::<Convoy>(namespace).list().await.expect("authored convoys").items.len(), expected);
    }
}

#[tokio::test]
async fn explicit_start_routes_with_lagging_dispatcher_observations() {
    lagging_start_placement_scenario(true, true, true).await;
}

#[tokio::test]
async fn automatic_start_routes_with_lagging_dispatcher_observations() {
    lagging_start_placement_scenario(false, true, true).await;
}

#[tokio::test]
async fn automatic_start_destination_refuses_optimistic_credential_replica() {
    lagging_start_placement_scenario(false, false, true).await;
}

#[tokio::test]
async fn automatic_start_destination_refuses_capacity_with_lagging_replica() {
    lagging_start_placement_scenario(false, true, false).await;
}

// Generator spans both routing choices and destination credential/capacity boundaries.
// Every scenario checks the router, request dispatch, result, and authored stores.
#[hegel::test]
fn generated_start_placement_with_lagging_replicas(tc: hegel::TestCase) {
    let explicit = tc.draw(gs::booleans());
    let destination_holds_credential = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    let destination_has_capacity = tc.draw(gs::booleans());
    runtime.block_on(lagging_start_placement_scenario(explicit, destination_holds_credential, destination_has_capacity));
}

#[tokio::test]
async fn placement_host_convoy_start_enforces_capacity_before_persistence() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named_with_floor("follower", Some(1_000_000)).await;
    seed_host_capacity(&follower, 0, 1_000_000 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let remote_host_id = topology.follower.local_host_id().expect("follower host identity").to_string();
    let placement_policy = format!("host-direct-{remote_host_id}");

    seed_target_placement_policy(&topology, namespace, &placement_policy).await;
    await_host_capacity(&topology.leader, &remote_host_id).await;
    seed_trusted_remote_convoy_project(&topology.leader, namespace).await;

    let mut events = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("remote-disk-hungry".to_string())
                            .branch("fix/remote-disk-hungry".to_string())
                            .placement_policy(placement_policy)
                            .escalation_reason("remote capacity check".to_string())
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("admitting store should evaluate the convoy");

    let result = await_command_result(&mut events, command_id).await;
    let CommandValue::Error { message } = result else {
        panic!("expected target free-space refusal, got {result:?}");
    };
    assert!(message.contains(&format!("host `{}`", topology.follower_host)), "{message}");
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
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("create remote Docker host");
    hosts
        .update_status("remote-docker-host", &host.metadata.resource_version, &HostStatus {
            capabilities: [
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"])),
                ("docker".to_string(), serde_json::json!(true)),
                ("os".to_string(), serde_json::json!("linux")),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("mark remote Docker host ready without capacity");
    let remote_policy = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: "remote-docker-host".to_string(),
            image: "crew:latest".to_string().into(),
            pull_policy: Default::default(),
            agent_adapters: BTreeSet::from(["codex".to_string()]),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
        })
        .build();
    daemon
        .resource_backend()
        .using::<PlacementPolicy>(namespace)
        .create(&InputMeta::builder().name("remote-docker".to_string()).build(), &remote_policy)
        .await
        .expect("create remote Docker placement");
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(namespace)
        .create(
            &InputMeta::builder().name("remote-docker".to_string()).build(),
            &FulfilmentKindSpec::from_policy(&remote_policy, "linux").expect("kind"),
        )
        .await
        .expect("remote Docker kind");

    let mut events = daemon.subscribe();
    let command_id = daemon
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".to_string())
                            .name("remote-docker-work".to_string())
                            .branch("fix/remote-docker-work".to_string())
                            .placement_policy("remote-docker".to_string())
                            .escalation_reason("remote capacity check".to_string())
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
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
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "legacy-remote-docker-work".to_string(),
                    workflow_ref: "remote-workflow".to_string(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: Some("remote-docker".to_string()),
                    adopted_checkout: None,
                })
                .build(),
        )
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

/// A Repository-addressed remote issue query returns results without a tracked root.
#[tokio::test]
async fn remote_issue_query_returns_results() {
    use flotilla_resources::{Repository, RepositorySpec};
    // Mock provider stands in for the external issue tracker API.
    let mock_service = Arc::new(MockIssueProvider);

    let follower_tmp = tempfile::tempdir().expect("tempdir");
    let follower_config = test_config_store(follower_tmp.path().join("config"));
    let follower_discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_issue_tracker(Arc::clone(&mock_service) as Arc<dyn IssueProvider>),
    );
    let follower = InProcessDaemon::new(vec![], follower_config, follower_discovery, HostName::new("follower")).await;
    let repository = RepositorySpec::remote("https://github.com/owner/repo").expect("repository");
    let key = repository.key();
    follower
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
        .await
        .expect("declare repository");
    assert!(follower.tracked_repo_paths().await.is_empty());

    let leader = empty_daemon_named("leader").await;

    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let follower_node_id = topology.follower.node_id().clone();

    let result = topology
        .client
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryIssues {
                    repo: RepoSelector::Repository(key),
                    params: IssueQuery::default(),
                    page: 1,
                    count: 10,
                })
                .node_id(follower_node_id)
                .build(),
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

// A stalled crew on A must deliver to a governor homed on B within one pass,
// then accept that governor's command at A using replicated session identity.
#[derive(Clone, Copy, bon::Builder)]
struct SupervisionScenario {
    #[builder(default)]
    legacy_cursor: bool,
    #[builder(default)]
    unavailable: bool,
    #[builder(default = 1)]
    passes: usize,
    #[builder(default)]
    source_remote: bool,
    #[builder(default)]
    stale_home: bool,
}

async fn cross_host_supervision_scenario(scenario: SupervisionScenario) {
    let SupervisionScenario { legacy_cursor, unavailable, passes, source_remote, stale_home } = scenario;
    let topology = spawn_in_memory_request_topology_stateful(empty_daemon_named("host-a").await, empty_daemon_named("host-b").await)
        .await
        .expect("router topology");
    let (a, b) = if source_remote { (&topology.follower, &topology.leader) } else { (&topology.leader, &topology.follower) };
    for (daemon, name, vessel, role, crew_id) in
        [(a, "stalled-work", "work", "coder", "coder-id"), (b, "governor", "govern", "governor", "governor-id")]
    {
        let backend = daemon.resource_backend();
        let convoys = backend.using::<Convoy>("flotilla");
        let spec = ConvoySpec::builder().workflow_ref("scratch".into()).project_ref("project".into()).role(role.into()).build();
        let convoy = convoys.create(&convoy_meta(name, role), &spec).await.expect("convoy");
        let snapshot = WorkflowSnapshot {
            vessels: vec![VesselRequirement::builder()
                .name(vessel.into())
                .crew(vec![CrewSpec::builder()
                    .role(role.into())
                    .source(CrewSource::Tool { command: "test".into() })
                    .completion_conditions(vec![CrewCompletionExpectation::artifact_exists(
                        role,
                        "decision-ledger",
                        ArtifactSubjectBinding::Convoy,
                    )])
                    .build()])
                .build()],
            stall_nudges: Default::default(),
            supervision: None,
            exit: None,
            turn_delivery: Default::default(),
        };
        convoys
            .update_status(name, &convoy.metadata.resource_version, &ConvoyStatus {
                phase: ResourceConvoyPhase::Active,
                workflow_snapshot: Some(snapshot),
                work: BTreeMap::from([(vessel.into(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
                crew_work: BTreeMap::from([(
                    vessel.into(),
                    BTreeMap::from([(role.into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            })
            .await
            .expect("active crew");
        let sessions = backend.using::<TerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name(format!("{name}-session"))
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), name.into()),
                        (VESSEL_LABEL.into(), vessel.into()),
                        (ROLE_LABEL.into(), role.into()),
                    ]))
                    .build(),
                &TerminalSessionSpec {
                    env_ref: "test-env".into(),
                    role: role.into(),
                    cwd: "/workspace".into(),
                    pool: "cleat".into(),
                    source: TerminalSessionSource::Agent {
                        selector: Selector::for_capability("coding"),
                        brief: TerminalBrief {
                            path: "brief.md".into(),
                            content: "original".into(),
                            artifact_digest: None,
                            copies: Vec::new(),
                        },
                        context: Box::new(TerminalCrewContext {
                            namespace: "flotilla".into(),
                            convoy: name.into(),
                            vessel_ref: vessel.into(),
                        }),
                        message: None,
                    },
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
                phase: flotilla_resources::TerminalSessionPhase::Running,
                crew: Some(CrewSessionStatus { id: crew_id.into(), adapter: "codex".into(), model: None, stance: role.into() }),
                attention: Some(flotilla_resources::TerminalAttention {
                    state: flotilla_resources::TerminalAttentionState::Working,
                    as_of: Utc::now(),
                    source: flotilla_resources::TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            })
            .await
            .expect("running crew session");
    }
    let a_backend = a.resource_backend();
    let b_backend = b.resource_backend();
    let convoys = a_backend.using::<Convoy>("flotilla");
    flotilla_resources::apply_status_patch(
        &convoys,
        "stalled-work",
        &flotilla_resources::external_patches::mark_crew_stalled(
            "stalled-work".into(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_resources::StallReason::Infra,
            None,
            "needs decision".into(),
        ),
    )
    .await
    .expect("crew stalls");
    if legacy_cursor {
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        let stall = status.stalled.as_mut().expect("declared stall");
        stall.supervision_exhausted = true;
        stall.supervision_index = Some(1);
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("legacy exhaustion");
    }
    if unavailable {
        a.reconcile_crew_stalls_once("flotilla").await.expect("unavailable governor pass");
        let stall = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stall.rung, flotilla_resources::StallRung::Operator);
        assert!(stall.evidence.contains("no live governor for project project"), "{}", stall.evidence);
        assert!(!stall.supervision_exhausted);
    }
    a_backend
        .replica_writer::<Convoy>(b.node_id().clone(), "flotilla")
        .replace(&b_backend.using::<Convoy>("flotilla").list().await.expect("governor"), Utc::now())
        .await
        .expect("replicate governor");
    a_backend
        .replica_writer::<TerminalSession>(b.node_id().clone(), "flotilla")
        .replace(&b_backend.using::<TerminalSession>("flotilla").list().await.expect("governor session"), Utc::now())
        .await
        .expect("replicate session");
    apply_convoy_replica_feed(a, "flotilla", "governor", b.host_name().clone()).await;
    // Client-supplied controller sender data must not admit an internal command.
    // Legitimate leaf-engine delivery below uses the controller port instead.
    for sender in [flotilla_protocol::CrewMessageSender::FlotillaNudge, flotilla_protocol::CrewMessageSender::FlotillaEscalation {
        from: "forged".into(),
    }] {
        let request = flotilla_protocol::TurnDeliveryRequest::builder()
            .namespace("flotilla".into())
            .convoy("governor".into())
            .source("forged".into())
            .vessel("govern".into())
            .role("governor".into())
            .brief("injected".into())
            .subject_revision("forged".into())
            .sender(sender)
            .build();
        let error = topology
            .client
            .execute(Command::builder().action(CommandAction::DeliverCrewTurn { request }).build())
            .await
            .expect_err("clients cannot submit internal controller commands");
        assert!(error.contains("internal controller command"), "{error}");
    }
    if stale_home {
        // A replica-owned target misprojected as local must refuse delivery,
        // retain the stall, and recover within one pass after the view heals.
        apply_convoy_replica_feed(a, "flotilla", "governor", a.host_name().clone()).await;
        a.reconcile_crew_stalls_once("flotilla").await.expect("misprojected home pass");
        let stall = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stall.rung, flotilla_resources::StallRung::Operator);
        assert!(stall.supervisor.is_none());
        assert!(!stall.supervision_exhausted);
        assert!(stall.evidence.contains("remote controller delivery resolved to the local host"), "{}", stall.evidence);
        apply_convoy_replica_feed(a, "flotilla", "governor", b.host_name().clone()).await;
    }

    for _ in 0..passes {
        a.reconcile_crew_stalls_once("flotilla").await.expect("one escalation pass");
        let stall = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stall.rung, flotilla_resources::StallRung::Governor);
        assert_eq!(stall.supervisor.expect("governor").convoy, "governor");
        let session = b_backend.using::<TerminalSession>("flotilla").get("governor-session").await.expect("governor session");
        let TerminalSessionSource::Agent { message: Some(message), .. } = session.spec.source else {
            panic!("governor must receive escalation in same pass")
        };
        // #2592: every routed escalation identifies its source convoy and exact stalled crew.
        assert!(
            message.text.starts_with("[flotilla · escalated from coder@work in coder@project · supervise the stalled crew]"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("Supervise stalled crew coder@work in convoy coder@project (resource ref: stalled-work)"),
            "{}",
            message.text
        );
        assert!(message.text.contains("--convoy 'stalled-work' --vessel 'work' --role 'coder' resume"), "{}", message.text);
        assert!(message.following.is_empty(), "repeat passes must not redeliver");
    }
    // The governor may issue supervision from B: route back to A and
    // validate B's replicated identity at the stalled convoy's home.
    apply_convoy_replica_feed(b, "flotilla", "stalled-work", a.host_name().clone()).await;
    b_backend
        .replica_writer::<Convoy>(a.node_id().clone(), "flotilla")
        .replace(&convoys.list().await.expect("stalled convoy"), Utc::now())
        .await
        .expect("replicate stall to governor");
    let mut events = topology.leader.subscribe();
    for crew_id in ["unrelated-crew", "governor-id"] {
        let id = topology
            .client
            .execute(
                Command::builder()
                    .action(CommandAction::CrewSupervise {
                        namespace: Some("flotilla".into()),
                        convoy: "stalled-work".into(),
                        vessel: "work".into(),
                        role: "coder".into(),
                        operation: flotilla_protocol::CrewSupervisionAction::Resume,
                        message: "continue with guidance".into(),
                        actor_crew_id: Some(crew_id.into()),
                    })
                    .build(),
            )
            .await
            .expect("supervision command");
        let (node_id, result) = await_command_finished(&mut events, id).await;
        assert_eq!(node_id, *a.node_id());
        if crew_id == "governor-id" {
            assert_eq!(result, CommandValue::Ok);
        } else {
            assert!(matches!(result, CommandValue::Error { message } if message.contains("does not own this supervision rung")));
            assert!(convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.is_some());
        }
    }
    let status = convoys.get("stalled-work").await.expect("source").status.expect("status");
    assert!(status.stalled.is_none());
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
}

#[tokio::test]
async fn cross_host_supervision_pinned_scenario_rows() {
    for scenario in [
        SupervisionScenario::builder().build(),
        SupervisionScenario::builder().legacy_cursor(true).source_remote(true).build(),
        SupervisionScenario::builder().legacy_cursor(true).unavailable(true).passes(2).source_remote(true).build(),
        SupervisionScenario::builder().stale_home(true).passes(2).build(),
    ] {
        cross_host_supervision_scenario(scenario).await;
    }
}

// Generate legacy/current stalls, visibility lag, and repeated reconcile steps.
// Each step checks the persisted rung and the governor's actual message queue.
#[hegel::test]
fn generated_cross_host_supervision(tc: hegel::TestCase) {
    let legacy = tc.draw(gs::booleans());
    let unavailable = tc.draw(gs::booleans());
    let passes = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(cross_host_supervision_scenario(
        SupervisionScenario::builder()
            .legacy_cursor(legacy)
            .unavailable(unavailable)
            .passes(passes)
            .source_remote(tc.draw(gs::booleans()))
            .stale_home(tc.draw(gs::booleans()))
            .build(),
    ));
}

// The trusted controller receiver still rejects non-controller sender variants,
// independently of client admission and before touching any target resources.
#[tokio::test]
async fn internal_turn_delivery_rejects_non_controller_sender() {
    let daemon = empty_daemon_named("receiver").await;
    for sender in [
        flotilla_protocol::CrewMessageSender::Unknown,
        flotilla_protocol::CrewMessageSender::Governor { name: "governor".into() },
        flotilla_protocol::CrewMessageSender::FlotillaTurn { source: "exit".into() },
    ] {
        let request = flotilla_protocol::TurnDeliveryRequest::builder()
            .namespace("flotilla".into())
            .convoy("governor".into())
            .source("test".into())
            .vessel("govern".into())
            .role("governor".into())
            .brief("test".into())
            .subject_revision("test".into())
            .sender(sender)
            .build();
        let mut events = daemon.subscribe();
        let id = daemon
            .execute(Command::builder().action(CommandAction::DeliverCrewTurn { request }).build())
            .await
            .expect("receiver accepts envelope");
        let result = await_command_result(&mut events, id).await;
        assert!(matches!(result, CommandValue::Error { message } if message == "remote turn delivery requires a controller sender"));
    }
}

#[tokio::test]
async fn internal_turn_delivery_rejects_forwarded_client_caller() {
    let daemon = empty_daemon_named("receiver").await;
    let request = flotilla_protocol::TurnDeliveryRequest::builder()
        .namespace("flotilla".into())
        .convoy("governor".into())
        .source("test".into())
        .vessel("govern".into())
        .role("governor".into())
        .brief("test".into())
        .subject_revision("test".into())
        .sender(flotilla_protocol::CrewMessageSender::FlotillaNudge)
        .build();
    let caller =
        CommandCaller { principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "client".into() }, process: None, crew: None };
    let mut events = daemon.subscribe();
    let id = daemon
        .execute_for_caller(Command::builder().action(CommandAction::DeliverCrewTurn { request }).build(), Some(caller))
        .await
        .expect("receiver accepts envelope");
    let result = await_command_result(&mut events, id).await;
    assert!(matches!(result, CommandValue::Error { message } if message == "DeliverCrewTurn is an internal controller command"));
}

// Resume admission accepts exited unfinished crew at the convoy authority,
// through the same router whether the caller is local or across a peer session.
async fn exited_crew_resume_scenario(remote_home: bool, interrupted: bool) {
    let topology = spawn_in_memory_request_topology_stateful(empty_daemon_named("desk").await, empty_daemon_named("placement").await)
        .await
        .expect("router topology");
    let home = if remote_home { &topology.follower } else { &topology.leader };
    let backend = home.resource_backend();
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let convoy = convoys.create(&convoy_meta("exited-work", "exited-work"), &convoy_spec("scratch", "exited-work")).await.expect("convoy");
    convoys
        .update_status("exited-work", &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            work: BTreeMap::from([("work".into(), flotilla_resources::WorkState::builder().phase(ResourceWorkPhase::Interrupted).build())]),
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([(
                    "coder".into(),
                    flotilla_resources::CrewWorkState::builder()
                        .phase(if interrupted {
                            flotilla_resources::CrewWorkPhase::Interrupted
                        } else {
                            flotilla_resources::CrewWorkPhase::Working
                        })
                        .build(),
                )]),
            )]),
            ..Default::default()
        })
        .await
        .expect("unfinished exited work");
    let sessions = backend.using::<TerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("exited-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "exited-work".into()),
                    (flotilla_resources::VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "warm-env".into(),
                role: "coder".into(),
                cwd: "/warm/checkout".into(),
                pool: "cleat".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: TerminalBrief {
                        path: "brief.md".into(),
                        content: "original brief".into(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "exited-work".into(),
                        vessel_ref: "exited-work-work".into(),
                    }),
                    message: None,
                },
            },
        )
        .await
        .expect("warm terminal");
    sessions
        .update_status("exited-session", &session.metadata.resource_version, &TerminalSessionStatus {
            phase: flotilla_resources::TerminalSessionPhase::Stopped,
            ..Default::default()
        })
        .await
        .expect("agent exited");
    if remote_home {
        apply_convoy_replica_feed(&topology.leader, "flotilla", "exited-work", topology.follower_host.clone()).await;
    }
    let mut events = topology.leader.subscribe();
    let id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyResume {
                    namespace: Some("flotilla".into()),
                    name: "exited-work".into(),
                    prompt: "finish the review".into(),
                    vessel: Some("work".into()),
                    role: Some("coder".into()),
                })
                .build(),
        )
        .await
        .expect("routed resume");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *home.node_id());
    assert!(matches!(result, CommandValue::ConvoyBriefQueued { .. }), "resume rejected: {result:?}");
    let session = sessions.get("exited-session").await.expect("same terminal");
    assert_eq!(session.status.expect("status").phase, flotilla_resources::TerminalSessionPhase::Starting);
    assert_eq!(session.spec.cwd, "/warm/checkout");
    let TerminalSessionSource::Agent { message: Some(message), .. } = session.spec.source else { panic!("operator follow-up missing") };
    assert!(message.text.contains("finish the review"));
}

#[tokio::test]
async fn router_exited_crew_resume_pinned_rows() {
    for remote_home in [false, true] {
        for interrupted in [false, true] {
            exited_crew_resume_scenario(remote_home, interrupted).await;
        }
    }
}

#[hegel::test]
fn generated_router_exited_crew_resume(tc: hegel::TestCase) {
    use tokio::runtime::Builder;
    // Both authority locations and both sides of the exit-observation race.
    let remote_home = tc.draw(hegel::generators::booleans());
    let interrupted = tc.draw(hegel::generators::booleans());
    Builder::new_current_thread().enable_all().build().expect("runtime").block_on(exited_crew_resume_scenario(remote_home, interrupted));
}

// #1769: identity resolution is a local read through the request router and works
// for a declared Repository with no checkout or observation-root membership.
async fn repository_identity_operations_scenario(alias: bool, observed: bool) {
    use flotilla_resources::{Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, Repository, RepositorySpec};

    let path = Path::new("/checkouts/router-main");
    let leader = if observed {
        use flotilla_core::providers::discovery::test_support::{FakePresentationManagerFactory, FakeVcsFactory, FakeVcsState};
        let tmp = tempfile::tempdir().expect("config");
        let mut discovery = fake_discovery(false);
        discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(
            FakeVcsState::builder(path).branch("main", true).checkout("main").is_main(true).path(path).build().build(),
        ))];
        discovery.factories.presentation_managers = vec![Box::new(FakePresentationManagerFactory(Arc::new(
            flotilla_core::providers::discovery::test_support::FakePresentationManager::new(),
        )))];
        InProcessDaemon::new(Vec::new(), test_config_store_with_floor(tmp.keep(), None), discovery, HostName::new("leader")).await
    } else {
        empty_daemon_named("leader").await
    };
    let topology = spawn_in_memory_request_topology_stateful(leader, empty_daemon_named("follower").await).await.expect("in-memory router");
    let backend = topology.leader.resource_backend();
    let spec = RepositorySpec::remote("https://github.com/acme/widgets").expect("repository");
    let key = spec.key();
    backend
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("declare Repository");
    let project = ProjectSpec::builder()
        .display_name("Widgets".into())
        .default_workflow_ref("single-agent".into())
        .repositories(vec![ProjectRepositorySpec::builder()
            .repo(key.clone())
            .alias("primary".into())
            .roles([ProjectRepositoryRole::Code].into())
            .build()])
        .build();
    backend
        .using::<Project>("flotilla")
        .create(&InputMeta::builder().name("widgets".into()).build(), &project)
        .await
        .expect("declare Project");
    let selector = RepoSelector::Query(if alias { "primary" } else { "acme/widgets" }.into());
    let result = topology
        .client
        .execute_query(
            Command::builder().action(CommandAction::QueryResolveRepository { repo: selector.clone() }).build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("resolve through router");
    assert_eq!(result, CommandValue::RepositoryResolved { key: Some(key.clone()) });
    let providers = topology
        .client
        .execute_query(
            Command::builder().action(CommandAction::QueryRepoProviders { repo: selector.clone() }).build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("providers through router");
    assert!(matches!(providers, CommandValue::RepoProviders(response) if response.repository == key && response.path.is_none()));
    for (action, refresh) in
        [(CommandAction::Refresh { repo: Some(selector.clone()) }, true), (CommandAction::CloseChangeRequest { id: "55".into() }, false)]
    {
        let mut events = topology.client.subscribe();
        let command_id = topology
            .client
            .execute(Command::builder().action(action).context_repo(selector.clone()).build())
            .await
            .expect("admit Repository command");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id: id, result, .. } = events.recv().await.expect("command event") {
                    if id == command_id {
                        return result;
                    }
                }
            }
        })
        .await
        .expect("Repository command finishes");
        if refresh {
            assert!(matches!(result, CommandValue::Refreshed { repository_count: 1, repos, .. } if repos.is_empty()));
        } else {
            // Provider refusal remains a capability error, independent of roots.
            assert!(matches!(result, CommandValue::Error { message } if message.contains("no change request provider")));
        }
    }
    if observed {
        // #1770: generic checkout admission/execution uses observed resources
        // and discovered VCS capabilities, without a presentation root.
        topology
            .leader
            .observed_resource_backend()
            .using::<Checkout>("flotilla")
            .create(
                &InputMeta::builder().name("router-main".into()).build(),
                &CheckoutSpec::Observed(
                    flotilla_resources::ObservedCheckoutSpec::builder()
                        .r#ref("main".into())
                        .path(path.to_string_lossy().into_owned())
                        .repo_ref(key.clone())
                        .host_ref(topology.leader.local_host_id().expect("host").to_string())
                        .is_main(true)
                        .build(),
                ),
            )
            .await
            .expect("observe checkout");
        let mut events = topology.client.subscribe();
        let id = topology
            .client
            .execute(
                Command::builder()
                    .action(CommandAction::Checkout {
                        repo: selector,
                        target: flotilla_protocol::CheckoutTarget::FreshBranch("router-branch".into()),
                        issue_ids: vec![],
                    })
                    .build(),
            )
            .await
            .expect("admit checkout command");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("event") {
                    if command_id == id {
                        break result;
                    }
                }
            }
        })
        .await
        .expect("checkout command finishes");
        assert!(matches!(result, CommandValue::CheckoutCreated { .. }), "{result:?}");
    }
    assert!(topology.leader.tracked_repo_paths().await.is_empty());
    assert!(topology.follower.tracked_repo_paths().await.is_empty());
}

// #1769/#2500: named Repository operations traverse the real request router
// without observation-root membership; aliases and slugs have identical behavior.
#[tokio::test]
async fn repository_identity_resolution_is_a_local_resource_read() {
    for (alias, observed) in [(false, false), (true, false), (false, true), (true, true)] {
        repository_identity_operations_scenario(alias, observed).await;
    }
}

#[hegel::test]
fn generated_router_repository_identity_operations(tc: hegel::TestCase) {
    // Both public selector forms, with provider queries and refresh/admission.
    let alias = tc.draw(gs::booleans());
    let observed = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(repository_identity_operations_scenario(alias, observed));
}

// #2498: the operator's request queries replicated stalls without a crew or repo target.
#[tokio::test]
async fn operator_crew_stalls_query_reads_remote_obligations() {
    let leader = empty_daemon_named("stall-reader").await;
    let follower = empty_daemon_named("stall-home").await;
    let remote = follower.resource_backend().using::<Convoy>("other-project-namespace");
    let convoy = remote
        .create(&InputMeta::builder().name("remote-stall".into()).build(), &convoy_spec("scratch", "implement"))
        .await
        .expect("remote convoy");
    remote
        .update_status("remote-stall", &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
        .await
        .expect("working crew");
    flotilla_resources::apply_status_patch(
        &remote,
        "remote-stall",
        &flotilla_resources::external_patches::mark_crew_stalled(
            "remote-stall".into(),
            "work".into(),
            "coder".into(),
            Utc::now(),
            flotilla_protocol::StallReason::Infra,
            Some(flotilla_protocol::StallProposedDisposition::Resume),
            "GitHub rate limit".into(),
        ),
    )
    .await
    .expect("declare stall");
    leader
        .resource_backend()
        .replica_writer::<Convoy>(follower.node_id().clone(), "other-project-namespace")
        .replace(&remote.list().await.expect("remote list"), Utc::now())
        .await
        .expect("replicate stall");
    let topology =
        spawn_in_memory_request_topology_stateful_with_surface(leader, follower, SurfaceDeclaration::focal_for_namespace("flotilla"))
            .await
            .expect("operator connection");
    let result = topology
        .client
        .execute_query(Command::builder().action(CommandAction::QueryCrewStalls { full: true }).build(), uuid::Uuid::nil())
        .await
        .expect("operator stalls");
    let CommandValue::CrewStalls(response) = result else { panic!("expected stalls: {result:?}") };
    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].namespace, "other-project-namespace");
    assert_eq!(response.rows[0].convoy, "remote-stall");
    assert_eq!(response.rows[0].role, "coder");
    assert_eq!(response.rows[0].rung, Some(flotilla_protocol::StallRung::Operator));
    assert_eq!(response.rows[0].evidence, "GitHub rate limit");
}
