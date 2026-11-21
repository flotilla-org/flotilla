use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_admission::reconcilers::convoy_ensure::EnsureReconciler;
use flotilla_config::ConfigStore;
use flotilla_core::command_target::TargetHost;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_crew::ports::WorkCredentialReconciler;
use flotilla_daemon::blob_store::TieredBlobStore;
use flotilla_daemon::runtime::DaemonRuntime;
use flotilla_daemon::runtime::RuntimeOptions;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_daemon_testkit::engine::{admit_convoy, current_launch, engine, run_session, runtime_terminals, session_request};
use flotilla_daemon_testkit::server::apply_convoy_replica_feed;
use flotilla_daemon_testkit::server::seed_trusted_remote_convoy_project;
use flotilla_daemon_testkit::server::spawn_in_memory_request_mesh;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful_with_caller;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful_with_caller_and_blob_store;
use flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful_with_surface;
use flotilla_daemon_testkit::server::HomedDaemon;
use flotilla_daemon_testkit::server::InMemoryRequestMesh;
use flotilla_daemon_testkit::server::InMemoryRequestTopology;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_discovery_testkit::fake_discovery_with_provider_set;
use flotilla_discovery_testkit::FakeDiscoveryProviders;
use flotilla_environment::environment::ProvisionedEnvironment;
use flotilla_environment::environment::ProvisionedMount;
use flotilla_issue_tracker::IssueProvider;
use flotilla_protocol::issue_query::IssueQuery;
use flotilla_protocol::issue_query::IssueResultPage;
use flotilla_protocol::qualified_path::HostId;
use flotilla_protocol::CallerCrew;
use flotilla_protocol::CallerProcess;
use flotilla_protocol::Command;
use flotilla_protocol::CommandAction;
use flotilla_protocol::CommandCaller;
use flotilla_protocol::CommandValue;
use flotilla_protocol::ConvoyStartIntent;
use flotilla_protocol::CrewCommandContext;
use flotilla_protocol::DaemonEvent;
use flotilla_protocol::EnvironmentId;
use flotilla_protocol::HostName;
use flotilla_protocol::ImageId;
use flotilla_protocol::Issue;
use flotilla_protocol::IssueChangeset;
use flotilla_protocol::IssueRef;
use flotilla_protocol::IssueSource;
use flotilla_protocol::NodeInfo;
use flotilla_protocol::PeerConnectionState;
use flotilla_protocol::PrincipalRef;
use flotilla_protocol::RepoSelector;
use flotilla_protocol::SurfaceCharacter;
use flotilla_protocol::SurfaceDeclaration;
use flotilla_protocol_testkit::TestIssue;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::terminal_records::{
    AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource, TerminalRecords,
};
use flotilla_resources::{
    Artifact, ArtifactSpec, ArtifactSubjectBinding, CheckoutSpec, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus,
    CredentialConsumer, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle,
    CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewCompletionExpectation, CrewSource, CrewSpec,
    CrewWorkPhase, CrewWorkState, FulfilmentKind, FulfilmentKindSpec, Host, HostSpec, HostStatus, InputMeta, Message, RepositoryKey,
    Resource, ResourceError, ResourceProvenance, Selector, SystemClock, TerminalBrief, TerminalCrewContext, TerminalSession,
    TerminalSessionSource, TerminalSessionSpec, Vessel, VesselRequirement, WorkPhase as ResourceWorkPhase, WorkState, WorkflowSnapshot,
    WorkflowTemplate, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, ROLE_LABEL,
};
use flotilla_store::off_frame;
use flotilla_store::{controller::ControllerLoop, ResourceBackend};
use hegel::generators as gs;

// A convoy's checkouts are the engine's, released at their home when the convoy ends (#3108):
// `flotilla-provisioning`'s `checkout_controller` tests cover that half, a holder at another home
// included. A forced delete ends the convoy at its own home: a convoy ends on the engine, never
// deleted, so it reads Abandoned there.
#[tokio::test]
async fn deleting_mis_homed_convoy_finalizes_it_at_its_home() {
    let topology =
        spawn_in_memory_request_topology_stateful(empty_daemon_named("convoy-home").await, empty_daemon_named("checkout-home").await)
            .await
            .expect("spawn two-home router");
    let namespace = "flotilla";
    let launch = admit(&topology.leader, "old-convoy", "old-convoy").await;
    eventually(Duration::from_secs(3), Duration::from_millis(10), "the convoy replicates to the other home", || async {
        topology.follower.resource_backend().including_replicas::<Convoy>(namespace).get(&launch).await.is_ok()
    })
    .await;
    let mut events = topology.leader.subscribe();
    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.into()), name: launch.clone(), force: true })
                .build(),
        )
        .await
        .expect("route parent delete");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::Ok);
    eventually(Duration::from_secs(3), Duration::from_millis(10), "the convoy ends at its home", || async {
        convoy_phase(&topology.leader, &launch).await == Some(ResourceConvoyPhase::Abandoned)
    })
    .await;
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

/// What a crew's terminal reports once its agent runs, as `adapter` with `stance`.
fn crew_facts(adapter: &str, stance: &str) -> TerminalLaunchFacts {
    TerminalLaunchFacts { agent: Some(AgentLaunch { adapter: adapter.into(), model: None, stance: stance.into() }), ..Default::default() }
}

async fn empty_daemon_named(host_name: &str) -> HomedDaemon {
    empty_daemon_named_with_floor(host_name, None).await
}

async fn empty_daemon_named_with_floor(host_name: &str, free_space_floor_gib: Option<u64>) -> HomedDaemon {
    let daemon = message_home_daemon_with_floor(host_name, free_space_floor_gib).await;
    // Its Message path, over the designated fleet's roles, as the runtime composes it.
    message_homes::seed_fleet(&daemon.resource_backend()).await;
    daemon
}

/// A daemon composed with its engine.
async fn message_home_daemon(host_name: &str) -> HomedDaemon {
    message_home_daemon_with_floor(host_name, None).await
}

async fn message_home_daemon_with_floor(host_name: &str, free_space_floor_gib: Option<u64>) -> HomedDaemon {
    let daemon = daemon_named_without_engine(host_name, free_space_floor_gib).await;
    // Composed with its engine, as the runtime composes a daemon.
    flotilla_daemon_testkit::engine::engine(&daemon);
    daemon
}

/// A daemon a [`DaemonRuntime`] will start, which composes its engine itself.
async fn runtime_daemon_named(host_name: &str) -> HomedDaemon {
    daemon_named_without_engine(host_name, None).await
}

/// A daemon in a temporary home, removed when the daemon's harness drops it.
async fn daemon_named_without_engine(host_name: &str, free_space_floor_gib: Option<u64>) -> HomedDaemon {
    let home = tempfile::tempdir().expect("tempdir");
    let config = test_config_store_with_floor(home.path().to_path_buf(), free_space_floor_gib);
    let daemon = InProcessDaemon::new(
        vec![],
        config,
        fake_discovery(false),
        HostName::new(host_name),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    daemon
        .install_convoy_ensure_reconciler(Arc::new(
            EnsureReconciler::builder()
                .teardown(daemon.convoy_teardown())
                .resource_backend(daemon.resource_backend())
                .clock(Arc::new(SystemClock))
                .manifest_roots(daemon.manifest_roots())
                .standing_roles(daemon.standing_roles())
                .build(),
        ))
        .await;
    HomedDaemon::new(daemon, home)
}

fn compatible_codex_facts() -> flotilla_resources::FulfilmentFacts {
    flotilla_resources::FulfilmentFacts {
        harnesses: BTreeMap::from([(
            "codex".into(),
            flotilla_resources::HarnessFacts { version: "0.160.0".into(), models: BTreeMap::new() },
        )]),
        ..Default::default()
    }
}

async fn seed_host_capacity(daemon: &Arc<InProcessDaemon>, free_bytes: u64, floor_bytes: u64) {
    let host_id = daemon.local_host_id().expect("host identity").to_string();
    let hosts = daemon.resource_backend().using::<Host>("flotilla");
    let host = hosts
        .create(&InputMeta::builder().name(host_id.clone()).build(), &HostSpec::default())
        .await
        .expect("create host capacity resource");
    hosts
        .update_status(
            &host_id,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [
                    (
                        flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                        serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
                    ),
                    (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"])),
                ]
                .into_iter()
                .collect(),
                fulfilment_facts: BTreeMap::from([(format!("host-direct-{host_id}"), compatible_codex_facts())]),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                daemon_generation: Some("test-generation".to_string()),
                daemon_started_at: Some(Utc::now()),
                disk_free_bytes: Some(free_bytes),
                admission_free_space_floor_bytes: Some(floor_bytes),
                ..HostStatus::default()
            },
        )
        .await
        .expect("publish host capacity");
}

async fn await_host_capacity(daemon: &Arc<InProcessDaemon>, host_id: &str) {
    eventually(Duration::from_secs(5), Duration::from_millis(10), "host capacity should replicate", || async {
        daemon.resource_backend().including_replicas::<Host>("flotilla").list().await.expect("list federated hosts").items.into_iter().any(
            |source| {
                source.object.metadata.name == host_id && source.object.status.is_some_and(|status| status.below_free_space_floor.is_some())
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

fn host_direct_kind(host_ref: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref(host_ref.to_string())
        .pool("cleat".to_string())
        .grants(BTreeSet::from([
            flotilla_resources::FulfilmentGrant::platform("linux".to_string()),
            flotilla_resources::FulfilmentGrant::host_account_reach(),
            flotilla_resources::FulfilmentGrant::network("host".to_string()),
        ]))
        .realisation(flotilla_resources::FulfilmentRealisation::HostDirect)
        .build()
}

/// Register the daemon's own `host-direct-<host>` kind, whose facts `seed_host_capacity` publishes.
async fn seed_local_host_direct_kind(daemon: &Arc<InProcessDaemon>) {
    let host_id = daemon.local_host_id().expect("host identity").to_string();
    daemon
        .resource_backend()
        .using::<FulfilmentKind>("flotilla")
        .create(&InputMeta::builder().name(format!("host-direct-{host_id}")).build(), &host_direct_kind(&host_id))
        .await
        .expect("local host-direct kind");
}

async fn seed_target_fulfilment_kind(topology: &InMemoryRequestTopology, namespace: &str, policy_name: &str) {
    let host_ref = topology.follower.local_host_id().expect("placement host identity").to_string();
    topology
        .follower
        .resource_backend()
        .using::<FulfilmentKind>(namespace)
        .create(&InputMeta::builder().name(policy_name.to_string()).build(), &host_direct_kind(&host_ref))
        .await
        .expect("placement host should register its fulfilment kind");
    let hosts = topology.follower.resource_backend().using::<Host>(namespace);
    let host = hosts.get(&host_ref).await.expect("placement host");
    let mut status = host.status.expect("host status");
    status.fulfilment_facts.insert(policy_name.to_string(), compatible_codex_facts());
    hosts.update_status(&host_ref, &host.metadata.resource_version, &status).await.expect("compatible harness facts");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "home-authored fulfilment kind should replicate to origin", || async {
        topology.leader.resource_backend().including_replicas::<FulfilmentKind>(namespace).get(policy_name).await.is_ok()
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

/// Convoy `record_name` as admission hands it to the engine: `spec`, and `status`, whose frozen
/// workflow is one work `work` crewed by an agent seat for `coder` where it names none.
fn engine_convoy(record_name: &str, spec: ConvoySpec, mut status: ConvoyStatus) -> flotilla_resources::ResourceObject<Convoy> {
    if status.workflow_snapshot.is_none() {
        status.workflow_snapshot = flotilla_engine_convoys_testkit::single_agent("coder").workflow_snapshot;
    }
    flotilla_engine_convoys_testkit::convoy("flotilla", record_name, spec, status)
}

/// Admits convoy `record_name` of `role` at `daemon`'s engine as [`engine_convoy`] describes it,
/// returning its launch.
async fn admit(daemon: &InProcessDaemon, record_name: &str, role: &str) -> String {
    admit_convoy(daemon, engine_convoy(record_name, convoy_spec("scratch", role), ConvoyStatus::default())).await
}

/// The phase `daemon` reads of convoy launch `name`.
async fn convoy_phase(daemon: &InProcessDaemon, name: &str) -> Option<ResourceConvoyPhase> {
    daemon.resource_backend().using::<Convoy>("flotilla").get(name).await.ok().and_then(|convoy| convoy.status).map(|status| status.phase)
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

/// The home-bound kind the replication scenarios author to follow one record, and its deletion,
/// through the router. Artifact is written at its home on the engine and reaches the other homes
/// by log following.
type Sample = Artifact;

fn sample_record(name: &str) -> (InputMeta, ArtifactSpec) {
    let at = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
    let spec = ArtifactSpec::builder()
        .convoy(name.to_string())
        .producer("sample".to_string())
        .kind("note".to_string())
        .subject("a record replication follows".to_string())
        .digest("0".repeat(64))
        .size(0)
        .media_type("text/plain".to_string())
        .expires_at(at + chrono::Duration::days(365 * 1000))
        .build();
    (InputMeta::builder().name(name.to_string()).build(), spec)
}

async fn create_sample(host: &InProcessDaemon, name: &str) -> Result<(), ResourceError> {
    let (meta, spec) = sample_record(name);
    host.resource_backend().using::<Sample>("flotilla").create(&meta, &spec).await.map(drop)
}

async fn wait_for_host_resource_visibility(hosts: &[Arc<InProcessDaemon>], name: &str, should_exist: bool) {
    let failure_message = format!("Artifact {name} did not become {} on every host", if should_exist { "visible" } else { "absent" });
    eventually(Duration::from_secs(5), Duration::from_millis(10), &failure_message, || async {
        for host in hosts {
            let read = host.resource_backend().including_replicas::<Sample>("flotilla").get(name).await;
            let matches_expected = if should_exist { read.is_ok() } else { matches!(read, Err(ResourceError::NotFound { .. })) };
            if !matches_expected {
                return false;
            }
        }
        true
    })
    .await;
}

async fn paired_world_trace(home_index: usize, issuer_index: usize) -> Vec<String> {
    let hosts = vec![empty_daemon_named("pair-a").await, empty_daemon_named("pair-b").await, empty_daemon_named("pair-c").await];
    let home = Arc::clone(&hosts[home_index]);
    seed_host_capacity(&home, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    home.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("paired world mesh");
    let host_id = home.local_host_id().expect("home id").to_string();
    let policy_name = format!("host-direct-{host_id}");
    home.resource_backend()
        .using::<FulfilmentKind>("flotilla")
        .create(&InputMeta::builder().name(policy_name.clone()).build(), &host_direct_kind(&host_id))
        .await
        .expect("pair kind");
    for host in &mesh.hosts {
        seed_trusted_remote_convoy_project(host, "flotilla").await;
    }
    eventually(Duration::from_secs(5), Duration::from_millis(10), "pair placement visible", || async {
        let issuer = mesh.hosts[issuer_index].resource_backend();
        issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
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
        CommandValue::ConvoyStarted { name, attach_plan, binding, .. } => {
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
                        reopen: false,
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
    // A convoy ends on the engine and is never deleted (#3108): the delete ends it at its home.
    assert_eq!(convoy_phase(&home, &name).await, Some(ResourceConvoyPhase::Abandoned));
    create_sample(&home, "pair-resource").await.expect("pair resource");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "pair resource visible", || async {
        mesh.hosts[issuer_index].resource_backend().including_replicas::<Sample>("flotilla").get("pair-resource").await.is_ok()
    })
    .await;
    let mut events = mesh.hosts[issuer_index].subscribe();
    let id = mesh.clients[issuer_index]
        .execute(
            Command::builder()
                .action(CommandAction::ResourceDelete {
                    namespace: "flotilla".into(),
                    kind: Sample::API_PATHS.plural.into(),
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

// The home x remote-issuer domain has six pairs, so enumerate all of them and
// trace each home's at-home baseline once.
#[test]
fn paired_world_host_independence_over_every_home_and_issuer() {
    for home in 0..3 {
        let at_home = paired_world_trace_in_own_runtime(home, home);
        for remote in (0..3).filter(|remote| *remote != home) {
            let remote_desk = paired_world_trace_in_own_runtime(home, remote);
            assert_eq!(remote_desk, at_home, "home={home}, remote={remote}");
        }
    }
}

/// One trace per paused runtime. A dropped mesh leaves its hosts' change-stream rounds to
/// their idle deadline, and a round's blocking producer stops paused time from advancing
/// to it; dropping the runtime closes those rounds before the next world starts.
fn paired_world_trace_in_own_runtime(home: usize, issuer: usize) -> Vec<String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
    runtime.block_on(paired_world_trace(home, issuer))
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
        // Held to the end: the hosts outlive each mesh made over them.
        let homed = [empty_daemon_named("home").await, empty_daemon_named("peer-b").await, empty_daemon_named("peer-c").await];
        let hosts: Vec<_> = homed.iter().map(|host| Arc::clone(host)).collect();
        let mesh = spawn_in_memory_request_mesh(hosts.clone()).await.expect("full mesh");
        let name = "deleted-during-partition";
        create_sample(&hosts[0], name).await.expect("create home resource");
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
                        kind: Sample::API_PATHS.plural.to_string(),
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
                host.resource_backend().including_replicas::<Sample>("flotilla").get(name).await,
                Err(ResourceError::NotFound { .. })
            ));
        }
    });
}

async fn spawn_tombstone_mesh(hosts: Vec<Arc<InProcessDaemon>>) -> Result<InMemoryRequestMesh, String> {
    spawn_in_memory_request_mesh(hosts).await
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
        // Held to the end: the hosts outlive each mesh made over them.
        let homed =
            [empty_daemon_named("transition-a").await, empty_daemon_named("transition-b").await, empty_daemon_named("transition-c").await];
        let hosts: Vec<_> = homed.iter().map(|host| Arc::clone(host)).collect();
        let mut mesh = spawn_tombstone_mesh(hosts.clone()).await.expect("initial mesh");
        let name = "transition-tombstone";
        create_sample(&hosts[home_index], name).await.expect("create at home");
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
                        kind: Sample::API_PATHS.plural.into(),
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
            create_sample(&hosts[home_index], &step_name).await.expect("create next resource at home");
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
                                kind: Sample::API_PATHS.plural.into(),
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
                                kind: Sample::API_PATHS.plural.into(),
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
                placement
                    .resource_backend()
                    .using::<FulfilmentKind>("flotilla")
                    .create(&InputMeta::builder().name(policy_name.clone()).build(), &host_direct_kind(&host_id))
                    .await
                    .expect("placement kind");
                eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees placement", || async {
                    let issuer = mesh.hosts[issuer_index].resource_backend();
                    issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
                        && issuer.including_replicas::<FulfilmentKind>("flotilla").get(&policy_name).await.is_ok()
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

// The checkout on another host is the engine's, released at its home when its convoy ends
// (#3108): `flotilla-provisioning`'s `checkout_controller` tests cover that half. A forced teardown
// dispatched from a third host ends the convoy at its home.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn forced_convoy_teardown_from_another_host_finalizes_at_its_home() {
    let hosts = vec![empty_daemon_named("home-a").await, empty_daemon_named("checkout-b").await, empty_daemon_named("desk-c").await];
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("request mesh");
    let namespace = "flotilla";
    let convoy_name = admit_convoy(
        &mesh.hosts[0],
        engine_convoy(
            "cross-home-convoy",
            ConvoySpec::builder()
                .workflow_ref("scratch".to_string())
                .project_ref("flotilla".to_string())
                .role("cross-home-convoy".to_string())
                .adopted_checkout_refs(BTreeMap::from([(RepositoryKey("test-repo".to_string()), "checkout-on-b".to_string())]))
                .build(),
            ConvoyStatus::default(),
        ),
    )
    .await;
    let convoy_name = convoy_name.as_str();
    eventually(Duration::from_secs(5), Duration::from_millis(10), "desk sees convoy", || async {
        mesh.hosts[2].resource_backend().including_replicas::<Convoy>(namespace).get(convoy_name).await.is_ok()
            && mesh.hosts[1].resource_backend().including_replicas::<Convoy>(namespace).get(convoy_name).await.is_ok()
    })
    .await;
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
    eventually(Duration::from_secs(5), Duration::from_millis(10), "#2343: the convoy on A must end", || async {
        convoy_phase(&mesh.hosts[0], convoy_name).await == Some(ResourceConvoyPhase::Abandoned)
    })
    .await;
}

async fn codex_version_admission_row(issuer_index: usize, minor: u32) {
    let hosts = vec![empty_daemon_named("codex-home").await, empty_daemon_named("codex-issuer").await];
    let home = Arc::clone(&hosts[0]);
    seed_host_capacity(&home, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    home.set_local_placement_capabilities(&BTreeSet::from(["codex".into()]), &["cleat".into()]).await;
    let host_id = home.local_host_id().expect("home").to_string();
    let backend = home.resource_backend();
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(&InputMeta::builder().name("codex-direct".into()).build(), &host_direct_kind(&host_id))
        .await
        .expect("kind");
    let host = backend.using::<Host>("flotilla").get(&host_id).await.expect("host");
    let mut status = host.status.expect("status");
    status.fulfilment_facts.insert(
        "codex-direct".into(),
        flotilla_resources::FulfilmentFacts {
            harnesses: BTreeMap::from([(
                "codex".into(),
                flotilla_resources::HarnessFacts { version: format!("0.{minor}.0"), models: BTreeMap::new() },
            )]),
            ..Default::default()
        },
    );
    backend.using::<Host>("flotilla").update_status(&host_id, &host.metadata.resource_version, &status).await.expect("probe facts");
    for host in &hosts {
        seed_trusted_remote_convoy_project(host, "flotilla").await;
        let workflows = host.resource_backend().using::<WorkflowTemplate>("flotilla");
        let mut workflow = workflows.get("remote-workflow").await.expect("workflow");
        workflow.spec.vessels[0].crew = vec![flotilla_resources::CrewSpec::builder()
            .role("coder".into())
            .source(flotilla_resources::CrewSource::Agent {
                selector: flotilla_resources::Selector::for_capability("code"),
                prompt: None,
                brief_template: None,
            })
            .build()];
        workflows
            .update(&InputMeta::from(&workflow.metadata), &workflow.metadata.resource_version, &workflow.spec)
            .await
            .expect("agent workflow");
    }
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("mesh");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "Codex placement replicated", || async {
        let issuer = mesh.hosts[issuer_index].resource_backend();
        issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
            && issuer.including_replicas::<FulfilmentKind>("flotilla").get("codex-direct").await.is_ok()
    })
    .await;
    let mut events = mesh.hosts[issuer_index].subscribe();
    let id = mesh.clients[issuer_index]
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyStart {
                    intent: Box::new(
                        ConvoyStartIntent::builder()
                            .project_ref("flotilla".into())
                            .name("codex-version".into())
                            .branch("test/codex-version".into())
                            .placement_policy("codex-direct".into())
                            .escalation_reason("test pinned host".into())
                            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                            .build(),
                    ),
                })
                .build(),
        )
        .await
        .expect("dispatch");
    let result = await_command_result(&mut events, id).await;
    if minor < 160 {
        assert!(
            matches!(&result, CommandValue::Error { message } if message.contains("harness:codex>=0.160.0")
            && message.contains(&format!("0.{minor}.0"))),
            "{result:?}"
        );
        assert!(backend.using::<Convoy>("flotilla").list().await.expect("convoys").items.is_empty());
    } else {
        assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
        assert_eq!(backend.using::<Convoy>("flotilla").list().await.expect("convoys").items.len(), 1);
    }
}

#[tokio::test]
async fn codex_version_admission_pinned_rows() {
    for issuer in [0, 1] {
        for minor in [154, 160] {
            codex_version_admission_row(issuer, minor).await;
        }
    }
}

#[hegel::test]
fn generated_codex_version_admission(tc: hegel::TestCase) {
    let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
    let minor = tc.draw(gs::integers::<u32>().min_value(150).max_value(170));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(codex_version_admission_row(issuer, minor));
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
        placement
            .resource_backend()
            .using::<FulfilmentKind>("flotilla")
            .create(&InputMeta::builder().name(policy_name.clone()).build(), &host_direct_kind(&host_id))
            .await
            .expect("fulfilment kind");
        for host in &mesh.hosts {
            seed_trusted_remote_convoy_project(host, "flotilla").await;
        }
        eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees placement", || async {
            let issuer = mesh.hosts[issuing_index].resource_backend();
            issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
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
                            reopen: false,
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
        // A convoy ends on the engine and is never deleted (#3108): the delete ends it at its home.
        assert_eq!(convoy_phase(&placement, &convoy_name).await, Some(ResourceConvoyPhase::Abandoned));
        let resource_name = "generated-resource";
        create_sample(&placement, resource_name).await.expect("create resource at home");
        eventually(Duration::from_secs(5), Duration::from_millis(10), "issuer sees home resource", || async {
            mesh.hosts[deleting_index].resource_backend().including_replicas::<Sample>("flotilla").get(resource_name).await.is_ok()
        })
        .await;
        let mut resource_events = mesh.hosts[deleting_index].subscribe();
        let resource_delete_id = mesh.clients[deleting_index]
            .execute(
                Command::builder()
                    .action(CommandAction::ResourceDelete {
                        namespace: "flotilla".to_string(),
                        kind: Sample::API_PATHS.plural.to_string(),
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
            placement.resource_backend().using::<Sample>("flotilla").get(resource_name).await,
            Err(ResourceError::NotFound { .. })
        ));
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
        // A convoy owes its agent seats' work, so one with none is refused on the engine (#3108).
        home.resource_backend()
            .using::<WorkflowTemplate>("flotilla")
            .create(&InputMeta::builder().name("empty".to_string()).build(), &flotilla_resources::single_agent_workflow_spec())
            .await
            .expect("workflow at create host");
        flotilla_store_testkit::fixtures::project(&home.resource_backend(), "flotilla", "flotilla").await;
        if !remote {
            seed_local_host_direct_kind(home).await;
        }
        let placement_policy = if remote {
            let host_id = home.local_host_id().expect("placement host identity").to_string();
            await_host_capacity(&topology.leader, &host_id).await;
            seed_target_fulfilment_kind(&topology, "flotilla", "duplicate-placement").await;
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
                r#ref: None,
                project_ref: "flotilla".to_string(),
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
        let launch = admit(home, name, name).await;
        if remote_home {
            apply_convoy_replica_feed(&topology.leader, namespace, &launch, topology.follower_host.clone()).await;
        }

        let action = match mutation {
            Mutation::Abandon => {
                CommandAction::ConvoyAbandon { namespace: Some(namespace.into()), name: launch.clone(), reason: "accepted loss".into() }
            }
            Mutation::Delete => CommandAction::ConvoyDelete { namespace: Some(namespace.into()), name: launch.clone(), force: true },
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
        } else {
            assert_eq!(result, CommandValue::Ok, "{name}");
        }
        // A convoy ends on the engine and is never deleted (#3108): a delete ends it, as an abandon does.
        assert_eq!(convoy_phase(home, &launch).await, Some(ResourceConvoyPhase::Abandoned), "{name}");
    }
}

/// A routed command, built from the launch it targets.
type LaunchAction = fn(&str) -> CommandAction;

#[tokio::test]
async fn router_delivery_scenario_table_reaches_remote_convoy_authority() {
    let scenarios: [(&str, LaunchAction); 4] = [
        ("ensure-roll", |_| CommandAction::ConvoyEnsureRoll { namespace: "flotilla".into(), name: "remote-ensure".into() }),
        ("resume", |launch| CommandAction::ConvoyResume {
            reopen: false,
            namespace: Some("flotilla".into()),
            name: launch.into(),
            prompt: "continue".into(),
            vessel: None,
            role: None,
        }),
        ("nudge", |launch| CommandAction::ConvoyResume {
            reopen: false,
            namespace: Some("flotilla".into()),
            name: launch.into(),
            prompt: "check in".into(),
            vessel: Some("work".into()),
            role: Some("coder".into()),
        }),
        ("supervise", |launch| CommandAction::CrewSupervise {
            namespace: Some("flotilla".into()),
            convoy: launch.into(),
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
        // The standing role's launch at its home, ensured from `remote-ensure`; the desk reads it
        // from its replica of that home's engine.
        let mut convoy = engine_convoy("remote-work", convoy_spec("scratch", "remote-work"), ConvoyStatus::default());
        convoy.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "remote-ensure".into());
        let launch = admit_convoy(&topology.follower, convoy).await;
        eventually(Duration::from_secs(5), Duration::from_millis(10), "the desk reads the remote launch", || async {
            topology.leader.resource_backend().including_replicas::<Convoy>("flotilla").get(&launch).await.is_ok()
        })
        .await;
        apply_convoy_replica_feed(&topology.leader, "flotilla", &launch, topology.follower_host.clone()).await;

        let mut events = topology.leader.subscribe();
        let command_id = topology.client.execute(Command::builder().action(action(&launch)).build()).await.expect("dispatch scenario");
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

// A convoy ends on the engine and is never deleted (#3108): a routed delete ends it at its home,
// and its end reaches the other home and stays after the homes reconnect.
#[tokio::test]
async fn convoy_delete_routes_to_the_home_and_its_tombstone_does_not_resurrect() {
    let kiwi = empty_daemon_named("kiwi").await;
    let feta = empty_daemon_named("feta").await;
    let namespace = "flotilla";
    let name = admit(&feta, "feta-homed", "feta-homed").await;

    let topology = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("connect kiwi and feta");
    let action = CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: name.clone(), force: true };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "feta-homed convoy should replicate to kiwi", || async {
        kiwi.resource_backend().including_replicas::<Convoy>(namespace).get(&name).await.is_ok()
    })
    .await;
    apply_convoy_replica_feed(&kiwi, namespace, &name, HostName::new("feta")).await;
    assert_eq!(
        kiwi.resolve_existing_convoy_target(&action).await.expect("resolve replica home").expect("remote target").home,
        HostName::new("feta")
    );

    let mut events = topology.leader.subscribe();
    let command_id = topology.client.execute(Command::builder().action(action).build()).await.expect("dispatch convoy delete from kiwi");
    assert_eq!(await_command_result(&mut events, command_id).await, CommandValue::Ok);

    let ended_at = |daemon: Arc<InProcessDaemon>, name: String| async move {
        daemon
            .resource_backend()
            .including_replicas::<Convoy>(namespace)
            .get(&name)
            .await
            .is_ok_and(|convoy| convoy.object.status.is_some_and(|status| status.phase == ResourceConvoyPhase::Abandoned))
    };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the convoy's end should reach kiwi", || async {
        ended_at(Arc::clone(&feta), name.clone()).await && ended_at(Arc::clone(&kiwi), name.clone()).await
    })
    .await;

    drop(topology);
    let reconnected = spawn_in_memory_request_topology(Arc::clone(&kiwi), Arc::clone(&feta)).await.expect("reconnect kiwi and feta");
    let marker = admit(&feta, "after-reconnect", "after-reconnect").await;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "reconnected replication should converge", || async {
        kiwi.resource_backend().including_replicas::<Convoy>(namespace).get(&marker).await.is_ok()
    })
    .await;
    assert!(ended_at(Arc::clone(&kiwi), name.clone()).await, "the ended convoy must not reopen after federation reconnects");
    drop(reconnected);
}

#[test]
fn convoy_creation_attributes_provenance_and_regard_to_the_surface_principal() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("request runtime");
    runtime.block_on(convoy_creation_attribution_scenario());
}

async fn convoy_creation_attribution_scenario() {
    let leader = empty_daemon_named("leader").await;
    seed_host_capacity(&leader, 2 * 1024 * 1024 * 1024, 1024 * 1024 * 1024).await;
    seed_local_host_direct_kind(&leader).await;
    // A convoy owes its agent seats' work, so one with none is refused on the engine (#3108).
    leader
        .resource_backend()
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("empty".to_string()).build(), &flotilla_resources::single_agent_workflow_spec())
        .await
        .expect("create workflow");
    flotilla_store_testkit::fixtures::project(&leader.resource_backend(), "flotilla", "flotilla").await;
    let follower = empty_daemon_named("follower").await;
    seed_host_capacity(&follower, 2 * 1024 * 1024 * 1024, 1024 * 1024 * 1024).await;
    let follower_host_id = follower.local_host_id().expect("follower host identity").to_string();
    let principal_ref = PrincipalRef { namespace: "flotilla".to_string(), name: "alice".to_string() };
    let topology = spawn_in_memory_request_topology_stateful_with_surface(
        Arc::clone(&leader),
        follower,
        SurfaceDeclaration { principal_ref: principal_ref.clone(), character: SurfaceCharacter::Focal },
    )
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
                    r#ref: None,
                    project_ref: "flotilla".to_string(),
                    placement_policy: None,
                    adopted_checkout: None,
                })
                .build(),
        )
        .await
        .expect("dispatch convoy creation");
    let created = await_command_result(&mut events, command_id).await;

    let backend = leader.resource_backend();
    let launch = convoy_record_name(&backend, "alice-dispatch").await;
    // Creation reports the launch it admitted, by its address.
    let address = flotilla_resources::convoy::engine::launch_address(&"project:flotilla".parse().expect("project address"), &launch)
        .expect("launch address");
    assert_eq!(created, CommandValue::ConvoyCreated { name: address.to_string() });
    let convoy = backend.using::<Convoy>("flotilla").get(&launch).await.expect("created convoy");
    assert_eq!(convoy.spec.dispatching_principal_ref, principal_ref);
}

/// A status whose frozen workflow has one work, `implement`, crewed by an agent seat for `coder`,
/// running.
fn implementing() -> ConvoyStatus {
    let mut snapshot = flotilla_engine_convoys_testkit::single_agent("coder").workflow_snapshot.expect("a frozen workflow");
    snapshot.vessels[0].name = "implement".to_string();
    ConvoyStatus {
        phase: ResourceConvoyPhase::Active,
        workflow_snapshot: Some(snapshot),
        work: BTreeMap::from([("implement".to_string(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
        ..Default::default()
    }
}

// The acting principal is the engine's Abandon edge's `by` (#3108); the convoy's view shows the
// operator's reason, not who gave it, so the attribution is no longer read back here.
async fn assert_abandon_attribution(principal_ref: PrincipalRef) {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful_with_surface(
        Arc::clone(&leader),
        follower,
        SurfaceDeclaration { principal_ref, character: SurfaceCharacter::Focal },
    )
    .await
    .expect("spawn attributed client topology");
    let role = "attributed-abandon";
    let address = format!("{role}@flotilla");
    let launch = admit_convoy(&leader, engine_convoy("attributed-abandon-g1", convoy_spec("empty", role), implementing())).await;
    let mut events = leader.subscribe();

    let command_id = topology
        .client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyAbandon {
                namespace: Some("flotilla".to_string()),
                name: address.clone(),
                reason: "no longer needed".to_string(),
            },
        })
        .await
        .expect("dispatch abandon");
    assert_eq!(
        await_command_result(&mut events, command_id).await,
        CommandValue::ConvoyAbandoned { name: address.clone(), archives: Vec::new() }
    );

    let status =
        leader.resource_backend().using::<Convoy>("flotilla").get(&launch).await.expect("abandoned convoy").status.expect("status");
    assert_eq!(status.phase, ResourceConvoyPhase::Abandoned);
    assert_eq!(status.message.as_deref(), Some("no longer needed"));
}

#[tokio::test]
async fn agent_abandon_is_attributed_to_the_acting_principal() {
    assert_abandon_attribution(PrincipalRef { namespace: "flotilla".to_string(), name: "governor agent".to_string() }).await;
}

#[tokio::test]
async fn implicit_human_abandon_remains_a_human_override() {
    assert_abandon_attribution(PrincipalRef::implicit_for_namespace("flotilla")).await;
}

#[tokio::test]
async fn cross_namespace_implicit_human_abandon_remains_a_human_override() {
    assert_abandon_attribution(PrincipalRef::implicit_for_namespace("people")).await;
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
        caller,
    )
    .await
    .expect("spawn caller-attributed topology");
    let convoys = leader.resource_backend().using::<Convoy>("flotilla");
    let role = "attributed-status";
    let address = format!("{role}@flotilla");
    let launch = admit_convoy(
        &leader,
        engine_convoy(
            "attributed-status-g1",
            convoy_spec("empty", role),
            ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() },
        ),
    )
    .await;
    let mut events = leader.subscribe();
    let rejected_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyResume {
                    reopen: false,
                    namespace: Some("flotilla".into()),
                    name: address.clone(),
                    prompt: String::new(),
                    vessel: None,
                    role: None,
                })
                .build(),
        )
        .await
        .expect("dispatch rejected resume");
    assert!(matches!(await_command_result(&mut events, rejected_id).await, CommandValue::Error { .. }));

    let command_id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyAbandon { namespace: Some("flotilla".into()), name: address.clone(), reason: "done".into() })
                .build(),
        )
        .await
        .expect("dispatch abandon");
    assert!(matches!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyAbandoned { .. }));

    // A lifecycle mutation's caller is its trace (#3108, as ruled), not the convoy's status: the
    // mutation is made through the socket caller's session and the convoy is explained by its
    // record: ended, it may have retired, and a role address names only a live or unretired one.
    assert_eq!(convoys.get(&launch).await.expect("convoy").status.expect("status").phase, ResourceConvoyPhase::Abandoned);
    let value = topology
        .client
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".into()), name: launch.clone() })
                .build(),
            uuid::Uuid::nil(),
        )
        .await
        .expect("explain convoy");
    assert!(matches!(value, CommandValue::ConvoyExplanation(_)), "expected convoy explanation: {value:?}");
}

/// The launch through `vessels` at `crew_host` of `convoy`'s work, the convoy homed at `home`.
fn crew_vessel(
    vessels: &flotilla_provisioning_testkit::VesselsFixture,
    crew_host: &Arc<InProcessDaemon>,
    convoy: &str,
    home: &Arc<InProcessDaemon>,
) -> String {
    vessels.request_for(&flotilla_resources::vessel_records::VesselLaunchRequest {
        convoy: convoy.into(),
        convoy_home: home.node_id().as_str().into(),
        work: "work".into(),
        host: crew_host.node_id().as_str().into(),
    })
}

#[tokio::test]
async fn artifact_requests_store_body_locally_and_route_envelope_to_convoy_home() {
    let leader = empty_daemon_named("artifact-crew-host").await;
    let follower = empty_daemon_named("artifact-home").await;
    let namespace = "flotilla";
    let workspace = tempfile::tempdir().expect("crew workspace");
    let convoy_launch = admit(&follower, "artifact-demo", "artifact-demo").await;
    let convoy = convoy_launch.as_str();
    // The crew's vessel, holding its session, is a launch at the crew host of work in the home's convoy.
    let vessel = crew_vessel(&engine(&leader).vessels, &leader, convoy, &follower);
    let launch = run_session(
        &leader,
        "terminal-artifact-coder",
        &TerminalSessionSpec::builder()
            .env_ref(leader.local_environment_id().to_string())
            .role("coder".to_string())
            .source(TerminalSessionSource::Agent {
                selector: Selector::for_capability("coding"),
                brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: vec![] },
                context: Box::new(TerminalCrewContext { namespace: namespace.into(), convoy: convoy.into(), vessel_ref: vessel.clone() }),
            })
            .cwd(workspace.path().to_string_lossy().into_owned())
            .pool("cleat".to_string())
            .build(),
        &crew_facts("codex", "trusted"),
    );
    let caller = CommandCaller {
        principal_ref: PrincipalRef::implicit_for_namespace(namespace),
        process: None,
        crew: Some(
            CallerCrew::builder()
                .namespace(namespace.to_string())
                .convoy(convoy.to_string())
                .vessel(vessel.clone())
                .role("coder".to_string())
                .crew_id(launch.clone())
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
    // The envelope reaches the crew's host as the home's log does.
    flotilla_provisioning_testkit::replicate_home(&follower.decided_store(), follower.node_id().as_str(), &leader.decided_store());
    // The record the address names reaches the crew host by replication, which this read does
    // not otherwise wait for.
    eventually(Duration::from_secs(5), Duration::from_millis(10), "home artifact should replicate to the crew host", || async {
        !leader.resource_backend().including_replicas::<Artifact>(namespace).list().await.expect("crew host artifacts").items.is_empty()
    })
    .await;
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
        let leader = runtime_daemon_named("artifact-crew-host").await;
        // Install the real credential collaborator while keeping automatic
        // resource reconciliation gated; this contract drives requests itself.
        let (_startup_tx, startup_ready) = tokio::sync::watch::channel(false);
        let runtime = DaemonRuntime::start_with_options(
            leader.clone(),
            leader.config_store(),
            None,
            RuntimeOptions { startup_ready: Some(startup_ready), ..Default::default() },
        )
        .await
        .expect("runtime credential controller");
        let follower = empty_daemon_named("artifact-home").await;
        let namespace = "flotilla";
        let convoy = "artifact-demo";
        let workspace = tempfile::tempdir().expect("crew workspace");
        let mut convoy_spec = ConvoySpec::builder().workflow_ref("scratch".to_string()).project_ref("flotilla".to_string()).build();
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
        // The convoy is the engine's launch, admitted with its crew's decision-ledger promise.
        let convoys = leader.resource_backend().using::<Convoy>(namespace);
        let convoy_launch = admit_convoy(
            &leader,
            flotilla_engine_convoys_testkit::convoy(
                namespace,
                convoy,
                convoy_spec,
                ConvoyStatus {
                    phase: ResourceConvoyPhase::Active,
                    workflow_snapshot: Some(WorkflowSnapshot {
                        cascade: None,
                        exit: None,
                        turn_delivery: Default::default(),
                        stall_nudges: Default::default(),
                        supervision: None,
                        vessels: vec![VesselRequirement::builder()
                            .name("work".into())
                            .crew(vec![CrewSpec::builder()
                                .role("coder".into())
                                .promises(vec![flotilla_resources::TemplatePromise::decision_ledger("coder")])
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
                },
            ),
        )
        .await;
        let convoy = convoy_launch.as_str();
        // A runtime composed this daemon: the launch is written at its home on its decided store.
        let vessels = flotilla_provisioning_testkit::VesselsFixture::over_service(&leader.decided_store());
        let vessel = crew_vessel(&vessels, &leader, convoy, &follower);
        // A runtime composed this daemon: the launch is written at its home as its controller writes it.
        let terminals = runtime_terminals(&leader);
        let launch = terminals.request(&session_request(
            "terminal-artifact-coder",
            &TerminalSessionSpec::builder()
                .env_ref(env_ref)
                .role("coder".to_string())
                .source(TerminalSessionSource::Agent {
                    selector: Selector::for_capability("coding"),
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: vec![] },
                    context: Box::new(TerminalCrewContext {
                        namespace: namespace.into(),
                        convoy: convoy.into(),
                        vessel_ref: vessel.clone(),
                    }),
                })
                .cwd(if environment_kind == ArtifactEnvironment::LocalHostDirect {
                    workspace.path().to_string_lossy().into_owned()
                } else {
                    "/crew".into()
                })
                .pool("cleat".to_string())
                .build(),
        ));
        terminals.ready(&launch, "terminal-artifact-coder", &crew_facts("codex", "trusted"));
        let caller = CommandCaller {
            principal_ref: PrincipalRef::implicit_for_namespace(namespace),
            process: None,
            crew: Some(
                CallerCrew::builder()
                    .namespace(namespace.to_string())
                    .convoy(convoy.to_string())
                    .vessel(vessel.clone())
                    .role("coder".to_string())
                    .crew_id(launch.clone())
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
        // ADR 0061 routing row: promise mutations resolve the ambient crew and
        // reach its convoy home over the same local/remote environment matrix.
        for operation in [
            flotilla_protocol::commands::CrewPromiseOperation::Promise { id: "route-proof".into(), kind: "pr".into() },
            flotilla_protocol::commands::CrewPromiseOperation::Retract {
                id: "route-proof".into(),
                reason: "routing proof finished".into(),
            },
        ] {
            let mut events = leader.subscribe();
            let id = topology
                .client
                .execute(
                    Command::builder()
                        .action(CommandAction::CrewPromise {
                            context: CrewCommandContext { crew_id: Some(launch.clone()), ..Default::default() },
                            operation,
                        })
                        .build(),
                )
                .await
                .unwrap();
            assert_eq!(await_command_result(&mut events, id).await, CommandValue::Ok);
        }
        let status = leader.resource_backend().using::<Convoy>(namespace).get(convoy).await.unwrap().status.unwrap();
        let promise = status.promises["work"]["coder"].iter().find(|p| p.id == "route-proof").unwrap();
        assert_eq!(promise.state, flotilla_resources::promises::PromiseState::Retracted);
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
            // #2758: retries and multiple PR bindings never write forge comments.
            assert!(comments.lock().expect("comments").is_empty());
            topology
                .client
                .artifact_put("decision-ledger".into(), String::new(), BTreeMap::new(), "text/markdown".into(), "ledger.md".into())
                .await
                .expect("retry ledger put");
            assert!(comments.lock().expect("comments").is_empty());
            let artifacts = leader.resource_backend().using::<Artifact>(namespace).list().await.expect("artifacts");
            let artifact = artifacts.items.iter().find(|artifact| artifact.spec.kind == "decision-ledger").expect("stored ledger");
            assert_eq!(artifact.spec.subject, convoy);
            assert!(artifact.spec.summary.is_empty());
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
        topology
            .client
            .artifact_get(format!("artifact/{namespace}/{}", record.metadata.name), "result.bin".into())
            .await
            .expect("crew can qualify its own namespace");
        assert_eq!(tokio::fs::read(operator_destination).await.expect("operator file"), ledger);
        assert_eq!(tokio::fs::read(destination).await.expect("ledger destination"), ledger);
        // Automatic reconciliation is deliberately gated in this transport
        // contract. Drive the real convoy reconciler after transferring evidence.
        use flotilla_store::controller::Reconciler;
        let backend = leader.resource_backend();
        let reconciler = flotilla_crew::convoy::ConvoyReconciler::new(
            leader.lifecycle_reads(),
            leader.host_records(),
            leader.convoy_engine_path(),
            leader.crew_message_path(),
        )
        .with_artifacts(backend.including_replicas::<Artifact>(namespace));
        // The reconciler observes the ledger and keeps its promise on the engine, one edge a pass.
        for _ in 0..16 {
            let current = convoys.get(convoy).await.unwrap();
            if current.status.as_ref().is_some_and(|status| status.crew_work["work"]["coder"].phase == CrewWorkPhase::Done) {
                break;
            }
            reconciler.prepare(&current).await.unwrap();
        }
        let status = convoys.get(convoy).await.unwrap().status.unwrap();
        assert!(status.promises["work"]["coder"].iter().any(|p| p.kind == flotilla_resources::promises::PromiseKind::DecisionLedger
            && p.state == flotilla_resources::promises::PromiseState::Kept));
        // This transport fixture admits only a ledger promise; PR promise
        // gating is covered by the controller and dummy-fleet scenarios.
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        runtime.shutdown().await;
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
    flotilla_discovery_testkit::injected_environment_handle!();

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
    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Running)
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

// Post-install contains paths from the installing host. Pin router refusal of a
// remote target so delivery cannot accidentally drain or write on another host.
// Local completion and readable diagnostics must survive unrelated event traffic.
async fn fleet_post_install_target_scenario(remote: bool, background_events: usize) {
    const BACKGROUND_COMMAND_ID: u64 = u64::MAX;

    let topology = spawn_in_memory_request_topology_stateful(empty_daemon_named("leader").await, empty_daemon_named("follower").await)
        .await
        .expect("topology");
    let directory = tempfile::tempdir().expect("diagnostics directory");
    let command = Command::builder()
        .node_id(if remote { topology.follower.node_id().clone() } else { topology.leader.node_id().clone() })
        .action(CommandAction::FleetPostInstall {
            cleat_bin: directory.path().join("cleat"),
            generation: "new-gen".into(),
            diagnostics_dir: directory.path().join("diagnostics"),
        })
        .build();
    let mut observer = topology.client.subscribe();
    let dispatch = async {
        // Deliver unrelated traffic through the real session before dispatch
        // returns. The observer proves delivery without a sleep or clock budget.
        for _ in 0..background_events {
            topology.leader.send_event(DaemonEvent::CommandStarted {
                command_id: BACKGROUND_COMMAND_ID,
                node_id: topology.leader.node_id().clone(),
                repo_identity: flotilla_protocol::RepoIdentity { authority: "test".into(), path: "background".into() },
                repo: None,
                description: "unrelated command".into(),
            });
            loop {
                if matches!(
                    observer.recv().await.expect("background event"),
                    DaemonEvent::CommandStarted { command_id: BACKGROUND_COMMAND_ID, .. }
                ) {
                    break;
                }
            }
        }
        topology.client.execute(command).await
    };
    if remote {
        // Refusal arrives in the dispatch response; no completion subscription is needed.
        // The observer drains each injected event before the next is sent.
        assert!(matches!(dispatch.await, Err(message) if message.contains("must run on the installing host")));
        assert!(!directory.path().join("diagnostics").exists());
    } else {
        let mut events = topology.client.subscribe();
        // Execute returns only after diagnostics I/O and command completion.
        // Drain concurrently: waiting for its response first lets unrelated
        // peer events overwrite the bounded subscription under workspace load.
        let completion = async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, node_id, result, .. } = events.recv().await.expect("daemon event") {
                    // Match dispatch's error type so try_join can cancel this waiter on rejection.
                    return Ok::<_, String>((command_id, node_id, result));
                }
            }
        };
        // A rejected dispatch must fail immediately rather than leave completion waiting.
        let (dispatched, (finished_id, node_id, result)) = tokio::try_join!(dispatch, completion).expect("dispatch local post-install");
        assert_eq!(finished_id, dispatched);
        assert_eq!(node_id, *topology.leader.node_id());
        let CommandValue::FleetPostInstall { report, .. } = result else { panic!("expected post-install report: {result:?}") };
        assert_eq!(report["generation"], "new-gen");
        let saved = std::fs::read_to_string(report["diagnostics_path"].as_str().expect("diagnostics path")).expect("retained report");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&saved).expect("report JSON"), report);
    }
}

#[tokio::test]
async fn fleet_post_install_target_pinned_rows() {
    for remote in [false, true] {
        for background_events in [0, 256, 257, 512] {
            fleet_post_install_target_scenario(remote, background_events).await;
        }
    }
}

#[hegel::test]
fn generated_fleet_post_install_target(tc: hegel::TestCase) {
    // Both targets and traffic counts spanning the client buffer boundary.
    let remote = tc.draw(hegel::generators::booleans());
    let background_events = tc.draw(gs::integers::<usize>().min_value(0).max_value(512));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(fleet_post_install_target_scenario(remote, background_events));
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

/// A ResourceApply or ResourceDelete of `template`, addressed to no host.
fn template_mutation(namespace: &str, name: &str, delete: bool) -> Command {
    let action = if delete {
        CommandAction::ResourceDelete {
            namespace: namespace.to_string(),
            kind: "workflowtemplates".to_string(),
            name: name.to_string(),
            replica_origin: None,
        }
    } else {
        CommandAction::ResourceApply {
            namespace: namespace.to_string(),
            document: serde_json::json!({
                "kind": "WorkflowTemplate",
                "metadata": {"name": name},
                "spec": {"vessels": [], "repository_refs": ["edited"]},
            }),
        }
    };
    Command::builder().action(action).build()
}

// A definition is written at its home (#3108): an edit or delete issued at another host, addressed
// to no host, runs at the definition's home and writes nothing where it was issued; with its home
// unreachable, it is refused.
#[tokio::test]
async fn definition_mutations_from_another_host_run_at_its_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let (leader, follower) = (&topology.leader, &topology.follower);
    let decided = leader.decided_store();
    let spec: WorkflowTemplateSpec = serde_json::from_value(serde_json::json!({"vessels": []})).expect("template spec");
    let held_here = |name: &'static str| async move {
        !matches!(leader.resource_backend().using::<WorkflowTemplate>(namespace).get(name).await, Err(ResourceError::NotFound { .. }))
    };

    follower
        .resource_backend()
        .definitions::<WorkflowTemplate>(namespace)
        .apply(&InputMeta::builder().name("homed-template".to_string()).build(), &spec)
        .await
        .expect("declare the template at its home");
    flotilla_provisioning_testkit::replicate_home(&follower.decided_store(), follower.node_id().as_str(), &decided);
    let read = leader.resource_backend().including_replicas::<WorkflowTemplate>(namespace).get("homed-template").await.expect("replica");
    assert!(matches!(&read.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == follower.node_id()));

    let mut events = leader.subscribe();
    let id = topology.client.execute(template_mutation(namespace, "homed-template", false)).await.expect("dispatch the edit");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *follower.node_id());
    assert!(matches!(result, CommandValue::ResourceObject(_)), "{result:?}");
    let edited = follower.resource_backend().definitions::<WorkflowTemplate>(namespace).get("homed-template").await.expect("home's copy");
    assert_eq!(edited.spec.repository_refs, Some(vec![RepositoryKey("edited".to_string())]));
    assert!(!held_here("homed-template").await, "the edit wrote no copy where it was issued");

    let id = topology.client.execute(template_mutation(namespace, "homed-template", true)).await.expect("dispatch the delete");
    let (node, result) = await_command_finished(&mut events, id).await;
    assert_eq!(node, *follower.node_id());
    assert!(matches!(result, CommandValue::ResourceDeleted(_)), "{result:?}");
    assert!(matches!(
        follower.resource_backend().definitions::<WorkflowTemplate>(namespace).get("homed-template").await,
        Err(ResourceError::NotFound { .. })
    ));

    // Feta, which no peer of this mesh reaches, declared this one.
    let feta = flotilla_provisioning_testkit::other_home_for(&decided, "feta-root");
    feta.definition::<WorkflowTemplate>("stranded-template", &spec).await;
    feta.replicate_to(&decided);
    for delete in [false, true] {
        let error = topology
            .client
            .execute(template_mutation(namespace, "stranded-template", delete))
            .await
            .expect_err("an unreachable home refuses the mutation");
        assert!(error.starts_with("resource origin feta-root is unreachable"), "{error}");
    }
    assert!(!held_here("stranded-template").await, "the refused edit wrote no copy here");
    let read = leader.resource_backend().including_replicas::<WorkflowTemplate>(namespace).get("stranded-template").await.expect("kept");
    assert_eq!(read.object.spec, spec, "feta's declaration stands");
}

#[tokio::test]
async fn hostless_convoy_delete_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let launch = admit(&topology.follower, "remote-only", "remote-only").await;
    let convoy_name = launch.as_str();

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
    // A convoy ends on the engine and is never deleted (#3108): the delete ends it at its home.
    assert_eq!(convoy_phase(&topology.follower, convoy_name).await, Some(ResourceConvoyPhase::Abandoned));
    assert!(
        !topology
            .leader
            .resource_backend()
            .including_replicas::<Convoy>(namespace)
            .get_all(convoy_name)
            .await
            .expect("the dispatch host's reads")
            .items
            .iter()
            .any(|read| read.provenance == ResourceProvenance::Local),
        "dispatch host should not create or own the convoy"
    );
}

#[tokio::test]
async fn mistargeted_convoy_delete_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let launch = admit(&topology.follower, "mistargeted", "mistargeted").await;
    let convoy_name = launch.as_str();
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
    assert_eq!(
        convoy_phase(&topology.follower, convoy_name).await,
        Some(ResourceConvoyPhase::Abandoned),
        "convoy operation should be rerouted to the row's home even when the incoming command has a stale node target"
    );
}

/// Reconcile-now on a follower-homed convoy from the leader's client, targeted at the leader when
/// `mistargeted`: the follower's Convoy controller queues the launch, and the leader's does not.
async fn convoy_reconcile_now_reaches_the_home_controller(mistargeted: bool) {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let launch = admit(&topology.follower, "remote-wake", "remote-wake").await;
    apply_convoy_replica_feed(&topology.leader, namespace, &launch, topology.follower_host.clone()).await;
    let queue = |daemon: &Arc<InProcessDaemon>| {
        let (sender, queue) = flotilla_store::controller::WorkQueueSender::channel();
        tokio::spawn(daemon.convoy_wake_watch().spawn(daemon.resource_backend(), namespace.to_owned(), sender));
        queue
    };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the follower's convoy should replicate to the leader", || async {
        topology.leader.resource_backend().including_replicas::<Convoy>(namespace).get(&launch).await.is_ok()
    })
    .await;
    let mut at_follower = queue(&topology.follower);
    let mut at_leader = queue(&topology.leader);

    let mut rx = topology.leader.subscribe();
    let mut command = Command::builder()
        .action(CommandAction::ResourceReconcileNow { namespace: namespace.to_string(), kind: "Convoy".into(), name: launch.clone() })
        .build();
    if mistargeted {
        command.node_id = Some(topology.leader.node_id().clone());
    }
    let command_id = topology.client.execute(command).await.expect("dispatch convoy reconcile-now");

    let result = await_command_result(&mut rx, command_id).await;
    assert!(
        matches!(result, CommandValue::ResourceReconciled { ref name, .. } if *name == launch),
        "reconcile-now should succeed at the convoy's home: {result:?}"
    );
    let queued = tokio::time::timeout(Duration::from_secs(5), at_follower.recv()).await.expect("queued at the home").expect("queue open");
    assert_eq!(queued, launch);
    assert!(at_leader.try_recv().is_err(), "the dispatch host's controller must not be woken for a convoy it does not home");
}

#[tokio::test]
async fn hostless_convoy_reconcile_now_wakes_the_remote_home_controller() {
    convoy_reconcile_now_reaches_the_home_controller(false).await;
}

#[tokio::test]
async fn mistargeted_convoy_reconcile_now_wakes_the_remote_home_controller() {
    convoy_reconcile_now_reaches_the_home_controller(true).await;
}

#[tokio::test]
async fn active_convoy_delete_names_the_host_owning_an_unreachable_checkout_environment() {
    use flotilla_inbox::InboxBackend;
    use futures::StreamExt;

    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let convoy_name = "remote-checkout";
    let home = topology.follower.resource_backend();
    // The convoy's home asks for inspections on the Message path, as its runtime composes it.
    let (decided, primitives) = message_homes::engine(&topology.follower).await;

    let convoys = home.clone().using::<Convoy>(namespace);
    let launch = admit_convoy(
        &topology.follower,
        engine_convoy(
            convoy_name,
            convoy_spec("scratch", convoy_name),
            ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() },
        ),
    )
    .await;
    let convoy_name = launch.as_str();
    // Host feta is homed at its own daemon, which no peer of this mesh reaches. Its daemon holds
    // the environment and the convoy's checkout in it, and its log reached the convoy's home.
    let owner = "01HFETA";
    let owner_home = format!("node-of-{owner}");
    flotilla_provisioning_testkit::put_host_at_its_home(
        &decided,
        owner,
        &HostSpec { display_name: "feta".into(), ..HostSpec::default() },
        None,
    )
    .await;
    let owning_daemon = flotilla_provisioning_testkit::other_home_for(&decided, &owner_home);
    let checkouts = flotilla_provisioning_testkit::CheckoutFixture::over(owning_daemon.home(), owning_daemon.service().store());
    // The convoy's launch reaches the owning host as its home's log does, holding its checkout.
    flotilla_provisioning_testkit::replicate_home(&decided, decided.home(), owning_daemon.service());
    checkouts.put_ready(
        "checkout-remote",
        &CheckoutSpec::Worktree(
            flotilla_resources::CheckoutWorktreeSpec::builder()
                .repo_ref(RepositoryKey("repo".into()))
                .env_ref(flotilla_resources::host_direct_environment_name(owner))
                .r#ref("topic".into())
                .target_path("/srv/repos/topic".into())
                .clone_ref("clone".into())
                .build(),
        ),
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(launch.clone())),
    );
    owning_daemon.replicate_to(&decided);
    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;

    // The inspection is asked of the owning host's checkout inspector.
    let inspector = flotilla_engine_messages::roles::checkout_inspector(&owner_home).expect("inspector inbox");
    let mut asked = primitives.inbox().watch(&inspector, None).await.expect("watch the owning host's inspector");
    topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some(namespace.to_string()), name: convoy_name.to_string(), force: false })
                .build(),
        )
        .await
        .expect("dispatch convoy delete");

    let change = tokio::time::timeout(Duration::from_secs(10), asked.next())
        .await
        .expect("delete asks the owning host for a fresh inspection")
        .expect("inspector watch open")
        .expect("inspector change");
    let request = primitives.inbox().get_async(change.message.as_str()).await.expect("read request").expect("request held");
    assert_eq!(request.spec().inbox, inspector, "the request names the host holding the checkout's environment");
    let subject = request.spec().references.iter().find(|reference| reference.subject).expect("subject").address.to_string();
    assert_eq!(subject, format!("host:{owner_home}/checkout:checkout-remote"));
    // Unreachable, the host never answers: the delete waits on the request, it does not proceed.
    assert!(matches!(
        primitives.request_answer(change.message.as_str()).await.expect("answer"),
        flotilla_engine_messages::RequestAnswer::Waiting
    ));
    convoys.get(convoy_name).await.expect("an unanswered inspection keeps the convoy at its home");
}

#[tokio::test]
async fn hostless_convoy_abandon_routes_to_remote_home() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let launch = admit(&topology.follower, "remote-abandon", "remote-abandon").await;
    let convoy_name = launch.as_str();
    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
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

    assert_eq!(
        await_command_result(&mut rx, command_id).await,
        CommandValue::ConvoyAbandoned { name: convoy_name.to_string(), archives: Vec::new() }
    );
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
    let work_name = "implement";

    let follower_convoys = topology.follower.resource_backend().using::<Convoy>(namespace);
    let launch =
        admit_convoy(&topology.follower, engine_convoy("remote-work", convoy_spec("scratch", "remote-work"), implementing())).await;
    let convoy_name = launch.as_str();
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
    // The operator's message is the reason on the engine's verdict edge; the view derives no work
    // message (#3108).
    assert_eq!(work.phase, ResourceWorkPhase::Complete);
}

#[tokio::test]
async fn hostless_convoy_command_explains_missing_home_route() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    // Feta's launch, which no peer of this mesh reaches, last seen when it was admitted there.
    let decided = topology.leader.decided_store();
    let feta = flotilla_provisioning_testkit::other_home_for(&decided, "feta-root");
    let stranded = flotilla_engine_convoys_testkit::EngineConvoysHome::over(
        feta.service(),
        namespace,
        &flotilla_store_memory::engine_home_backend(),
        Arc::new(SystemClock),
        topology.leader.lifecycle_reads(),
    )
    .launch(engine_convoy(
        "stranded",
        ConvoySpec::builder().workflow_ref("scratch".to_string()).project_ref("flotilla".to_string()).role("stranded".to_string()).build(),
        ConvoyStatus::default(),
    ))
    .await;
    feta.replicate_to(&decided);
    let convoy_name = stranded.as_str();
    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, HostName::new("feta")).await;
    let replica = topology.leader.resource_backend().including_replicas::<Convoy>(namespace).get(convoy_name).await.expect("feta's launch");
    let ResourceProvenance::Replica { last_synced_at: last_seen_at, .. } = replica.provenance else {
        panic!("feta's launch is a replica here")
    };

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

    // Last seen is when feta's launch reached here: the replica's sync time, set by the store.
    let (seen, rest) = message
        .strip_prefix(&format!("convoy flotilla/{convoy_name} is homed at feta, last seen "))
        .and_then(|rest| rest.split_once("; "))
        .unwrap_or_else(|| panic!("{message}"));
    let seen = chrono::DateTime::parse_from_rfc3339(seen).unwrap_or_else(|error| panic!("{message}: {error}"));
    assert!((seen.with_timezone(&Utc) - last_seen_at).num_seconds().abs() < 5, "{message}");
    assert_eq!(
        rest,
        format!(
            "home is unreachable: no routed node address found for host. Break glass: flotilla resource delete convoys \
             {convoy_name} --namespace flotilla --host feta"
        )
    );
}

#[tokio::test]
async fn hostless_convoy_delete_uses_live_peer_route_when_connection_status_is_stale() {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named("follower").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let launch = admit(&topology.follower, "offline-home", "offline-home").await;
    let convoy_name = launch.as_str();

    apply_convoy_replica_feed(&topology.leader, namespace, convoy_name, topology.follower_host.clone()).await;
    topology
        .leader
        .publish_peer_connection_status(
            &NodeInfo::new(topology.follower.node_id().clone(), topology.follower_host.to_string()),
            PeerConnectionState::Disconnected,
        )
        .await;

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
    assert_eq!(
        convoy_phase(&topology.follower, convoy_name).await,
        Some(ResourceConvoyPhase::Abandoned),
        "remote-homed convoy should be ended through the live peer route"
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
    seed_target_fulfilment_kind(&topology, namespace, &placement_policy).await;
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

    let reads = engine(&topology.follower).reads();
    let controller = ControllerLoop {
        primary: placement.using::<Convoy>(namespace),
        secondaries: Vec::new(),
        reconciler: ConvoyReconciler::new(
            topology.follower.lifecycle_reads(),
            topology.follower.host_records(),
            topology.follower.convoy_engine_path(),
            topology.follower.crew_message_path(),
        )
        .with_vessels(placement.including_replicas::<Vessel>(namespace)),
        backend: placement.clone(),
    };
    let controller_task = tokio::spawn(controller.run());
    // Feta serves the placed host, so its vessel requests launch the work there.
    use flotilla_resources::vessel_records::VesselRecords;
    struct Kept;
    #[async_trait]
    impl flotilla_admission::reconcilers::vessel_requests::VesselReleaseGate for Kept {
        async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
            Err(format!("convoy {} is kept for the test", convoy))
        }
    }
    let requests = |backend: &ResourceBackend, reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>, host: &str| {
        flotilla_admission::reconcilers::vessel_requests::VesselRequests::new(
            std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(backend.clone(), namespace)),
            reads,
            Arc::new(Kept),
            flotilla_protocol::CanonicalHostId::resolved(host),
            [],
        )
    };
    let feta_requests = requests(&placement, Arc::clone(&reads), &remote_host_id);
    let kiwi_requests =
        requests(&dispatcher, engine(&topology.leader).reads(), &topology.leader.local_host_id().expect("kiwi host identity").to_string());
    let feta_vessels = &engine(&topology.follower).vessels;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "feta launches its vessel", || async {
        feta_requests.sync_once(Utc::now()).await.expect("feta vessel requests");
        let launches = VesselRecords::every(feta_vessels.vessels.as_ref()).expect("feta launches");
        if !launches.is_empty() {
            assert_eq!(launches.len(), 1, "one vessel is launched at its actuation host");
            assert_eq!(launches[0].convoy, convoy.metadata.name);
            return true;
        }
        false
    })
    .await;
    controller_task.abort();
    assert!(kiwi_requests.sync_once(Utc::now()).await.expect("kiwi vessel requests").requested.is_empty());
    // Kiwi follows feta's log, so feta's launch may already read here: only kiwi's own count.
    let kiwi_home = topology.leader.decided_store().home().to_owned();
    let kiwi_launches = VesselRecords::every(engine(&topology.leader).vessels.vessels.as_ref()).expect("kiwi launches");
    assert!(kiwi_launches.iter().all(|launch| launch.home != kiwi_home), "kiwi launches nothing: {kiwi_launches:?}");
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

    seed_target_fulfilment_kind(&topology, namespace, &placement_policy).await;
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

    assert_eq!(
        await_command_result(&mut events, command_id).await,
        CommandValue::ConvoyStarted {
            warnings: Vec::new(),
            naming: Some(flotilla_protocol::ConvoyNaming::Explicit),
            name: "remote-work@flotilla".to_string(),
            attach_plan: None,
            binding: None
        }
    );
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
    seed_target_fulfilment_kind(&topology, namespace, &placement_policy).await;
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
        .create(
            &InputMeta::builder().name("claude-max".to_string()).build(),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "crew@example.com".to_string() },
                source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
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

    // This is the replication boundary: the dispatcher holds an older self-report
    // than the destination, whose own record stays authoritative. Feta reports the
    // older one, kiwi installs it, and kiwi installs nothing feta reports after.
    let destination_hosts = topology.follower.resource_backend().using::<Host>(namespace);
    let current = destination_hosts.get(&remote_host_id).await.expect("destination host");
    let mut status = current.status.clone().expect("host status");
    status.disk_free_bytes = Some(0);
    status.admission_free_space_floor_bytes = None;
    status.ready = false;
    status.heartbeat_at = Some(Utc::now() - chrono::Duration::hours(1));
    status.capabilities.insert(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([]));
    status.capabilities.insert(
        HELD_CREDENTIALS_CAPABILITY.to_string(),
        serde_json::json!(if destination_holds_credential { vec![] } else { vec!["claude-max"] }),
    );
    let older =
        destination_hosts.update_status(&remote_host_id, &current.metadata.resource_version, &status).await.expect("older self-report");
    topology.hold_follower_stream_at_leader().await;
    destination_hosts
        .update_status(&remote_host_id, &older.metadata.resource_version, current.status.as_ref().expect("host status"))
        .await
        .expect("current self-report");
    let held = topology
        .leader
        .resource_backend()
        .including_replicas::<Host>(namespace)
        .get(&remote_host_id)
        .await
        .expect("dispatcher's replica")
        .object
        .status
        .expect("replica status")
        .held_credentials()
        .expect("decode replica held credentials");
    assert_eq!(held.contains("claude-max"), !destination_holds_credential, "the dispatcher must hold the older self-report");
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
        assert_eq!(
            result,
            CommandValue::ConvoyStarted {
                warnings: Vec::new(),
                naming: Some(flotilla_protocol::ConvoyNaming::Explicit),
                name: "kiwi-to-feta@flotilla".to_string(),
                attach_plan: None,
                binding: None
            }
        );
    } else {
        let CommandValue::Error { message } = result else {
            panic!("expected credential refusal: {result:?}");
        };
        // Preparation decides placement before the destination's own floor
        // can be judged, so a missing credential is reported first.
        if !destination_holds_credential {
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

// Enumerate both routing choices and destination credential/capacity boundaries:
// the eight rows are fewer than a generated budget would draw.
// Every scenario checks the router, request dispatch, result, and authored stores.
#[test]
fn start_placement_with_lagging_replicas_every_row() {
    for explicit in [false, true] {
        for destination_holds_credential in [false, true] {
            for destination_has_capacity in [false, true] {
                let runtime =
                    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused runtime");
                runtime.block_on(lagging_start_placement_scenario(explicit, destination_holds_credential, destination_has_capacity));
            }
        }
    }
}

#[tokio::test]
async fn placement_host_convoy_start_enforces_capacity_before_persistence() {
    placement_host_capacity_scenario(false).await;
}

#[tokio::test]
async fn placement_host_capacity_without_project_replication() {
    placement_host_capacity_scenario(true).await;
}

// Behaviour (#3599): the placement host admits the start and refuses its own
// insufficient capacity before persisting a Convoy, even with no definition replication.
async fn placement_host_capacity_scenario(hold_definitions: bool) {
    let leader = empty_daemon_named("leader").await;
    let follower = empty_daemon_named_with_floor("follower", Some(1_000_000)).await;
    seed_host_capacity(&follower, 0, 1_000_000 * 1024 * 1024 * 1024).await;
    follower.set_local_placement_capabilities(&BTreeSet::from(["codex".to_string()]), &["cleat".to_string()]).await;
    // Admission runs on the placement host. Holding definition replication
    // verifies that admission needs neither definition replicated to the origin:
    // the engine, which replicates a home's log whole, is held from before the
    // follower writes them.
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("spawn stateful topology");
    let namespace = "flotilla";
    let remote_host_id = topology.follower.local_host_id().expect("follower host identity").to_string();
    let placement_policy = format!("host-direct-{remote_host_id}");

    seed_target_fulfilment_kind(&topology, namespace, &placement_policy).await;
    await_host_capacity(&topology.leader, &remote_host_id).await;
    if hold_definitions {
        topology.hold_follower_stream_at_leader().await;
    }
    // The follower performs admission; seed its prerequisites synchronously.
    seed_trusted_remote_convoy_project(&topology.follower, namespace).await;

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
    // No absence check at the origin: Project and WorkflowTemplate are engine definitions now and
    // reach it with the follower's log, which the old-store kind filter cannot hold back.

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
    remote_docker_admission_refusal_scenario(None, false, false).await;
}

// Behaviour (#3108 owner ruling): a remote host whose free disk it reports below its admission
// floor is refused through both start and create, naming the floor and the remedies and no
// figures, before persisting a convoy.
#[tokio::test]
async fn remote_docker_admission_refuses_a_host_below_its_floor() {
    remote_docker_admission_refusal_scenario(None, false, true).await;
}

// Behaviour (#3578): an admitter below its own floor still judges work placed
// on a remote host by that host's evidence, through both start and create
// without an adopted checkout. Its own floor must not preempt the refusal.
#[tokio::test]
async fn full_admitter_judges_remote_placement_by_target_evidence() {
    remote_docker_admission_refusal_scenario(None, true, false).await;
}

// Admission refuses provider deficiencies at the target host through both
// public start/create routes, before persisting a convoy. Generate all readiness,
// missing-capability and ambiguity rows on the existing in-memory router harness.
#[hegel::test]
fn remote_provider_refusals_fail_closed(tc: hegel::TestCase) {
    let case = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(remote_docker_admission_refusal_scenario(Some(case), false, false));
}

/// `below_floor`: the remote host reports free disk below its floor; otherwise it reports no
/// free-space figures, so whether it has room is unknown.
async fn remote_docker_admission_refusal_scenario(provider_case: Option<usize>, admitter_full: bool, below_floor: bool) {
    let daemon = empty_daemon_named_with_floor("leader", Some(if admitter_full { 1_000_000 } else { 0 })).await;
    let namespace = "flotilla";
    seed_trusted_remote_convoy_project(&daemon, namespace).await;

    // The remote Docker host is homed at its own daemon; its record arrives here as its home's
    // log would. It is ready and heard, without known room.
    let host_spec = HostSpec { display_name: "remote-docker".to_string(), connection: Default::default(), ..HostSpec::default() };
    let mut host_status = HostStatus {
        capabilities: [
            (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"])),
            (
                flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
            ),
            ("docker".to_string(), serde_json::json!(true)),
            ("os".to_string(), serde_json::json!("linux")),
        ]
        .into_iter()
        .collect(),
        fulfilment_facts: BTreeMap::from([("remote-docker".into(), compatible_codex_facts())]),
        heartbeat_at: Some(Utc::now()),
        ready: true,
        // The figures the host reports, if any; the engine keeps only whether free disk is below
        // the floor, unknown without both.
        disk_free_bytes: below_floor.then_some(0),
        admission_free_space_floor_bytes: below_floor.then_some(1),
        ..HostStatus::default()
    };
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(namespace)
        .create(
            &InputMeta::builder().name("remote-docker".to_string()).build(),
            &FulfilmentKindSpec::builder()
                .host_ref("remote-docker-host".to_string())
                .pool("cleat".to_string())
                .grants(BTreeSet::from([
                    flotilla_resources::FulfilmentGrant::platform("linux".to_string()),
                    flotilla_resources::FulfilmentGrant::network("scoped".to_string()),
                ]))
                .realisation(flotilla_resources::FulfilmentRealisation::DockerPerVessel {
                    image: Some(flotilla_resources::PlacementImageSource::Literal("crew:latest".into())),
                })
                .build(),
        )
        .await
        .expect("remote Docker kind");

    let mut expected = if below_floor {
        "placement refused on host `remote-docker`: free space is below the admission floor; reap settled convoys, run scripts/prune-target.sh, or pick another host"
    } else {
        "placement refused on host `remote-docker`: free space is unavailable"
    };
    if let Some(case) = provider_case {
        use flotilla_protocol::provider::{MaterialMode, ProviderReadiness};
        let mut providers = flotilla_discovery_testkit::environment::ready_placement_advertisements();
        match case {
            0 => {
                providers[1].readiness = ProviderReadiness::MissingCredential { reason: "sandbox token absent".into() };
                expected = "missing credential: sandbox token absent";
            }
            1 => {
                providers[1].readiness = ProviderReadiness::default();
                expected = "readiness unknown";
            }
            2 => {
                // A Docker kind checks out a worktree on the host, which needs Shared material.
                providers[1].capabilities.material_modes.retain(|mode| *mode != MaterialMode::Shared);
                expected = "ready but no Shared material";
            }
            _ => {
                let mut other = providers[1].clone();
                other.instance = "other".into();
                providers.push(other);
                expected = "ambiguous provider instances";
            }
        }
        host_status.capabilities.insert(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers));
    }
    flotilla_provisioning_testkit::put_host_at_its_home(&daemon.decided_store(), "remote-docker-host", &host_spec, Some(&host_status))
        .await;
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
    assert!(message.contains(expected), "{message}");
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
                    r#ref: None,
                    project_ref: "flotilla".to_string(),
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
    assert!(message.contains(expected), "{message}");
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
    let follower = InProcessDaemon::new(
        vec![],
        follower_config,
        follower_discovery,
        HostName::new("follower"),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
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
    subscriptions: bool,
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
    let SupervisionScenario { subscriptions, unavailable, passes, source_remote, stale_home } = scenario;
    let topology = spawn_in_memory_request_topology_stateful(empty_daemon_named("host-a").await, empty_daemon_named("host-b").await)
        .await
        .expect("router topology");
    let (a, b) = if source_remote { (&topology.follower, &topology.leader) } else { (&topology.leader, &topology.follower) };
    let a_holds = if source_remote { &topology.follower_stream_holds } else { &topology.leader_stream_holds };
    let b_home = b.decided_store().home().to_owned();
    if unavailable {
        // The governor's launch travels B's log: until A installs it, A has no live governor.
        a_holds.hold(&b_home);
    }
    off_frame(|| seed_supervision_crew(a, "stalled-work", "work", "coder")).await;
    let governor = off_frame(|| seed_supervision_crew(b, "governor", "govern", "governor")).await;
    let b_backend = b.resource_backend();
    if subscriptions {
        off_frame(|| seed_governor_subscription(a, b)).await;
    }
    off_frame(|| stall_source_crew(a, unavailable)).await;
    a_holds.release(&b_home);
    off_frame(|| replicate_governor_to_source(a, b, stale_home)).await;
    off_frame(|| await_replicated_session(a, &governor)).await;

    for _ in 0..passes {
        off_frame(|| check_supervision_escalation_pass(a, b)).await;
    }
    let escalation_name =
        b_backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("governor inbox").items[0].metadata.name.clone();
    off_frame(|| replicate_stall_to_governor(a, b)).await;
    off_frame(|| supervise_stall_from_governor(&topology, a, &governor)).await;
    off_frame(|| check_supervised_resume(a, &escalation_name)).await;
}

/// The source crew at `a` declares a stall; an unavailable governor pass when the scenario asks.
async fn stall_source_crew(a: &InProcessDaemon, unavailable: bool) {
    let a_backend = a.resource_backend();
    let convoys = a_backend.using::<Convoy>("flotilla");
    let source = current_launch(a, "stalled-work");
    // A crew's stall is its home's note (#3108).
    flotilla_store::note_convoy(
        &a.convoy_engine_path(),
        &convoys,
        &source,
        &flotilla_resources::external_patches::mark_crew_stalled(
            source.clone(),
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
    if unavailable {
        a.reconcile_crew_stalls_once("flotilla").await.expect("unavailable governor pass");
        let stall = convoys.get(&source).await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stall.rung, flotilla_resources::StallRung::Operator);
        assert!(stall.evidence.contains("no live governor for project project"), "{}", stall.evidence);
        assert!(!stall.supervision_exhausted);
    }
}

/// The governor's convoy and session replicate to `a`, with its presentation feed.
async fn replicate_governor_to_source(a: &InProcessDaemon, b: &InProcessDaemon, stale_home: bool) {
    // The governor's convoy and session launches, and its holder vessel, travel B's engine log
    // over the topology's own change streams. A copy sent beside them races their rounds: a
    // round staged before a launch installs after it and resets the home without it.
    let governor = governor_launch(b);
    let convoys = a.resource_backend().including_replicas::<Convoy>("flotilla");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the governor's convoy should replicate to the source", || async {
        convoys.get(&governor).await.is_ok()
    })
    .await;
    apply_convoy_replica_feed(a, "flotilla", &governor, b.host_name().clone()).await;
    if stale_home {
        // Ordinary Message homing follows resource provenance even when the
        // older presentation feed incorrectly projects the convoy as local.
        apply_convoy_replica_feed(a, "flotilla", &governor, a.host_name().clone()).await;
    }
}

/// `a` reads the session of crew `crew_id`, held for its vessel: a's engine follows the log
/// replicated in, and its session listing catches up with it.
async fn await_replicated_session(a: &InProcessDaemon, crew_id: &str) {
    let sessions = a.resource_backend().including_replicas::<TerminalSession>("flotilla");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the source reads the replicated session", || async {
        sessions.list().await.expect("sessions").items.iter().map(|source| &source.object).any(|session| {
            session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id)
                && session.metadata.labels.contains_key(flotilla_resources::VESSEL_LABEL)
        })
    })
    .await;
}

/// The stalled convoy replicates to the governor's host `b`, with its presentation feed: the
/// governor may issue supervision from B, which routes back to A and validates B's replicated
/// identity at the stalled convoy's home.
async fn replicate_stall_to_governor(a: &InProcessDaemon, b: &InProcessDaemon) {
    let source = current_launch(a, "stalled-work");
    apply_convoy_replica_feed(b, "flotilla", &source, a.host_name().clone()).await;
    // The stalled convoy's launch travels A's engine log over the topology's own change streams.
    let convoys = b.resource_backend().including_replicas::<Convoy>("flotilla");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the stalled convoy should replicate to the governor", || async {
        convoys.get(&source).await.is_ok()
    })
    .await;
}

/// The supervised crew at `a` is working again, and the ruling replies to the escalation.
async fn check_supervised_resume(a: &InProcessDaemon, escalation_name: &str) {
    let a_backend = a.resource_backend();
    let convoys = a_backend.using::<Convoy>("flotilla");
    let status = convoys.get(&current_launch(a, "stalled-work")).await.expect("source").status.expect("status");
    assert!(status.stalled.is_none());
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    // Reply identity does not depend on the escalation replicating back from B.
    let replies = a_backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("source replies");
    assert!(replies.items.iter().any(|message| message.spec.in_reply_to.as_deref() == Some(escalation_name)));
}

/// The governor's launch at `daemon`: as the current generation of a standing role, the engine
/// names it by that role.
fn governor_launch(daemon: &InProcessDaemon) -> String {
    current_launch(daemon, &flotilla_resources::convoy::engine::standing_name("project", "governor").expect("standing name"))
}

/// One convoy with a working crew member and its running session, at `daemon`.
async fn seed_supervision_crew(daemon: &InProcessDaemon, name: &str, vessel: &str, role: &str) -> String {
    seed_supervision_crew_in(daemon, "flotilla", name, vessel, role).await
}

/// [`seed_supervision_crew`] in `namespace`.
async fn seed_supervision_crew_in(daemon: &InProcessDaemon, namespace: &str, name: &str, vessel: &str, role: &str) -> String {
    let spec = ConvoySpec::builder().workflow_ref("scratch".into()).project_ref("project".into()).role(role.into()).build();
    let snapshot = WorkflowSnapshot {
        cascade: None,
        vessels: vec![VesselRequirement::builder()
            .name(vessel.into())
            .crew(vec![CrewSpec::builder()
                .role(role.into())
                .promises(vec![flotilla_resources::TemplatePromise::decision_ledger(role)])
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
        exit: (name == "stalled-work").then(flotilla_resources::ExitDeclaration::standard_table),
        turn_delivery: Default::default(),
    };
    // The convoy is the daemon's engine launch, its crew owing a decision ledger.
    let mut convoy = flotilla_engine_convoys_testkit::convoy(
        namespace,
        name,
        spec,
        ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            workflow_snapshot: Some(snapshot),
            work: BTreeMap::from([(vessel.into(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
            crew_work: BTreeMap::from([(
                vessel.into(),
                BTreeMap::from([(role.into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        },
    );
    if role == "governor" {
        // The governor is the current generation of the standing role `seed_governor_subscription` declares.
        convoy.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "project-governor".into());
    }
    let convoy = admit_convoy(daemon, convoy).await;
    // The crew's session is the daemon's engine launch, held for its vessel's launch.
    let held = engine(daemon).vessels.request(&convoy, vessel, daemon.node_id().as_str());
    // The vessel is ready, so its work runs and the crew's seat is judged.
    engine(daemon).vessels.step(&held, flotilla_resources::vessel_records::VesselStep::Realised { environment: "test-env".into() });
    let terminals = &engine(daemon).terminals;
    let launch = terminals.request(&TerminalLaunchRequest {
        session: format!("{name}-session"),
        environment: "test-env".into(),
        vessel: Some(held),
        role: role.into(),
        pool: "cleat".into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "coding".into(),
            adapter: None,
            model: None,
            brief: TerminalBrief { path: "brief.md".into(), content: "original".into(), artifact_digest: None, copies: Vec::new() },
        },
    });
    terminals.ready(&launch, &format!("{name}-session"), &crew_facts("codex", role));
    let attention = flotilla_resources::TerminalAttention {
        state: flotilla_resources::TerminalAttentionState::Working,
        as_of: Utc::now(),
        source: flotilla_resources::TerminalAttentionSource::Hook,
    };
    TerminalRecords::observations(terminals.terminals.as_ref()).update(&launch, |observation| {
        observation.observe_attention(attention, Some("codex"));
    });
    launch
}

/// The project's governor role subscribes to supervision; its presence is held at `b` and
/// replicated to `a`.
async fn seed_governor_subscription(a: &InProcessDaemon, b: &InProcessDaemon) {
    let a_backend = a.resource_backend();
    let b_backend = b.resource_backend();
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, Project, ProjectSpec, RoleDefinition, RoleSubscription};
    a_backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &ProjectSpec::builder()
                .display_name("project".into())
                .role_definitions(BTreeMap::from([(
                    "governor".into(),
                    RoleDefinition {
                        subscriptions: Some(vec![RoleSubscription::builder().topic("supervision".into()).build()]),
                        ..Default::default()
                    },
                )]))
                .build(),
        )
        .await
        .expect("project subscriptions");
    // A's Project reaches B as A's log does.
    let a_store = a.decided_store();
    flotilla_provisioning_testkit::replicate_home(&a_store, a_store.home(), &b.decided_store());
    let ensures = b_backend.using::<ConvoyEnsure>("flotilla");
    ensures
        .create(
            &InputMeta::builder().name("project-governor".into()).build(),
            &ConvoyEnsureSpec::builder().project_ref("project".into()).role("governor".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("presence");
}

/// One escalation pass at `a`, checked at the governor's home `b`.
async fn check_supervision_escalation_pass(a: &InProcessDaemon, b: &InProcessDaemon) {
    let a_backend = a.resource_backend();
    let b_backend = b.resource_backend();
    let convoys = a_backend.using::<Convoy>("flotilla");
    let (source, governor) = (current_launch(a, "stalled-work"), governor_launch(b));
    a.reconcile_crew_stalls_once("flotilla").await.expect("one escalation pass");
    let stall = convoys.get(&source).await.expect("source").status.expect("status").stalled.expect("stall");
    assert_eq!(stall.rung, flotilla_resources::StallRung::Governor, "{}", stall.evidence);
    assert_eq!(stall.supervisor.expect("governor").convoy, governor);
    let messages = b_backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver inbox");
    assert_eq!(messages.items.len(), 1, "repeat passes must not publish duplicates");
    // Homed at the governor's home, and listed at A too once A follows B's log.
    let message = &messages.items[0];
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let listed = a_backend.including_replicas::<flotilla_resources::Message>("flotilla").list().await.expect("source replica");
            if listed.items.iter().any(|read| {
                read.object.metadata.name == message.metadata.name
                    && matches!(&read.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root == b.node_id())
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the escalation lists at A as B's");
    // Only the escalation's copy is at A: a person told of the stall before the governor was
    // live holds a System attention there, not a Supervisor message.
    let local = a_backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("source inbox").items;
    assert!(local.iter().all(|local| local.spec.relation != flotilla_resources::MessageRelation::Supervisor), "{local:?}");
    assert_eq!(message.spec.relation, flotilla_resources::MessageRelation::Supervisor);
    let sender = flotilla_resources::convoy::engine::launch_address(&"project:project".parse().expect("project address"), &source)
        .expect("launch address");
    assert_eq!(message.spec.sender, format!("{sender}/role:coder"));
    assert!(message.spec.body.starts_with("Escalated from coder@work in coder@project:"), "{}", message.spec.body);
    assert!(
        message.spec.body.contains(&format!("Supervise stalled crew coder@work in convoy coder@project (resource ref: {source})")),
        "{}",
        message.spec.body
    );
    assert!(message.spec.body.contains(&format!("--convoy '{source}' --vessel 'work' --role 'coder' resume")), "{}", message.spec.body);
}

/// The governor's supervision from its host resumes the stalled crew at `a`; another crew is
/// refused.
async fn supervise_stall_from_governor(topology: &InMemoryRequestTopology, a: &InProcessDaemon, governor: &str) {
    let convoys = a.resource_backend().using::<Convoy>("flotilla");
    let source = current_launch(a, "stalled-work");
    let mut events = topology.leader.subscribe();
    for crew_id in ["unrelated-crew", governor] {
        let id = topology
            .client
            .execute(
                Command::builder()
                    .action(CommandAction::CrewSupervise {
                        namespace: Some("flotilla".into()),
                        convoy: source.clone(),
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
        if crew_id == governor {
            assert_eq!(result, CommandValue::Ok);
        } else {
            assert!(matches!(result, CommandValue::Error { message } if message.contains("does not own this supervision rung")));
            assert!(convoys.get(&source).await.expect("source").status.expect("status").stalled.is_some());
        }
    }
}

#[tokio::test]
async fn cross_host_supervision_pinned_scenario_rows() {
    for scenario in [
        SupervisionScenario::builder().build(),
        SupervisionScenario::builder().source_remote(true).build(),
        SupervisionScenario::builder().unavailable(true).passes(2).source_remote(true).build(),
        SupervisionScenario::builder().stale_home(true).passes(2).build(),
        SupervisionScenario::builder().subscriptions(true).source_remote(true).passes(2).build(),
    ] {
        cross_host_supervision_scenario(scenario).await;
    }
}

// Generate stalls, visibility lag, and repeated reconcile steps.
// Each step checks the persisted rung and the governor's receiver-homed Message.
#[hegel::test]
fn generated_cross_host_supervision(tc: hegel::TestCase) {
    let subscriptions = tc.draw(gs::booleans());
    let unavailable = tc.draw(gs::booleans());
    let passes = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(cross_host_supervision_scenario(
        SupervisionScenario::builder()
            .subscriptions(subscriptions)
            .unavailable(unavailable)
            .passes(passes)
            .source_remote(tc.draw(gs::booleans()))
            .stale_home(tc.draw(gs::booleans()))
            .build(),
    ));
}

// Message admission refuses an intent whose addresses do not parse through ordinary
// resource mutation, before any transport work.
#[tokio::test]
async fn message_resource_apply_rejects_invalid_intent() {
    let daemon = empty_daemon_named("receiver").await;
    let convoy = admit_convoy(
        &daemon,
        engine_convoy(
            "convoy",
            ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("project".into()).build(),
            ConvoyStatus::default(),
        ),
    )
    .await;
    // The inbox names the convoy's launch.
    let project = flotilla_address::Address::project("project").expect("project");
    let inbox = flotilla_resources::convoy::engine::seat_address(&project, &convoy, "coder").expect("inbox");
    for (sender, relation, reason) in [("invalid/address/shape", "system", "is not `kind:value`")] {
        let mut events = daemon.subscribe();
        let admission = daemon.execute(Command::builder().action(CommandAction::ResourceApply {
            namespace: "flotilla".into(),
            document: serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"invalid","namespace":"flotilla"},
                "spec":{"sender":sender,"inbox":inbox,"relation":relation,"body":"test"}}),
        }).build()).await;
        let error = match admission {
            Err(error) => error,
            Ok(id) => match await_command_result(&mut events, id).await {
                CommandValue::Error { message } => message,
                result => panic!("invalid intent accepted: {result:?}"),
            },
        };
        assert!(error.contains(reason), "unexpected refusal: {error}");
        assert!(daemon.resource_backend().using::<Message>("flotilla").list().await.expect("receiver inbox").items.is_empty());
    }
}

// Resume admission accepts exited unfinished crew at the convoy authority,
// through the same router whether the caller is local or across a peer session.
async fn exited_crew_resume_scenario(remote_home: bool, interrupted: bool) {
    let topology = spawn_in_memory_request_topology_stateful(empty_daemon_named("desk").await, empty_daemon_named("placement").await)
        .await
        .expect("router topology");
    let home = if remote_home { &topology.follower } else { &topology.leader };
    let backend = home.resource_backend();
    let convoy = admit_convoy(
        home,
        engine_convoy(
            "exited-work",
            convoy_spec("scratch", "exited-work"),
            ConvoyStatus {
                phase: ResourceConvoyPhase::Active,
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(ResourceWorkPhase::Interrupted).build(),
                )]),
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
            },
        ),
    )
    .await;
    // The crew's session is the home's engine launch, held for its realised vessel's launch; its
    // agent exited, which interrupts the seat.
    let vessel = engine(home).vessels.request(&convoy, "work", home.node_id().as_str());
    engine(home).vessels.ready(&vessel, "warm-env");
    let terminals = &engine(home).terminals;
    let launch = terminals.request(&TerminalLaunchRequest {
        session: "exited-session".into(),
        environment: "warm-env".into(),
        vessel: Some(vessel),
        role: "coder".into(),
        pool: "cleat".into(),
        cwd: "/warm/checkout".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "coding".into(),
            adapter: None,
            model: None,
            brief: TerminalBrief { path: "brief.md".into(), content: "original brief".into(), artifact_digest: None, copies: Vec::new() },
        },
    });
    terminals.ready(&launch, "exited-session", &Default::default());
    terminals.exited(&launch, "agent exited");
    let sessions = backend.using::<TerminalSession>("flotilla");
    if remote_home {
        apply_convoy_replica_feed(&topology.leader, "flotilla", &convoy, topology.follower_host.clone()).await;
    }
    let mut events = topology.leader.subscribe();
    let id = topology
        .client
        .execute(
            Command::builder()
                .action(CommandAction::ConvoyResume {
                    reopen: false,
                    namespace: Some("flotilla".into()),
                    name: convoy.clone(),
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
    let inbox = backend.using::<flotilla_resources::Message>("flotilla").list().await.unwrap();
    assert_eq!(inbox.items.len(), 1);
    assert!(inbox.items[0].spec.body.contains("finish the review"));
    // A person sends as the role they hold (#3938): the record names the role, not the principal.
    assert_eq!(inbox.items[0].spec.sender, "project:flotilla/role:operator");
    assert!(inbox.items[0].status.as_ref().unwrap().resolved_receiver.is_none());
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
        use flotilla_discovery_testkit::{FakeVcsFactory, FakeVcsState};
        let tmp = tempfile::tempdir().expect("config");
        let mut discovery = fake_discovery(false);
        discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(
            FakeVcsState::builder(path).branch("main", true).checkout("main").is_main(true).path(path).build().build(),
        ))];
        let daemon = InProcessDaemon::new(
            Vec::new(),
            test_config_store_with_floor(tmp.path().to_path_buf(), None),
            discovery,
            HostName::new("leader"),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        flotilla_daemon_testkit::engine::engine(&daemon);
        HomedDaemon::new(daemon, tmp)
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
    assert!(
        matches!(&providers, CommandValue::RepoProviders(response) if response.repository == key && response.path.is_none()),
        "{providers:?}"
    );
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
        topology.leader.discovered_checkouts().replace(
            &key,
            vec![flotilla_resources::discovered_checkouts::DiscoveredCheckout {
                name: "router-main".into(),
                repo_slug: key.to_string(),
                spec: flotilla_resources::ObservedCheckoutSpec::builder()
                    .r#ref("main".into())
                    .path(path.to_string_lossy().into_owned())
                    .repo_ref(key.clone())
                    .host_ref(topology.leader.local_host_id().expect("host").to_string())
                    .is_main(true)
                    .build(),
            }],
        );
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
    let remote = follower.resource_backend().using::<Convoy>("flotilla");
    // The namespace's fleet is designated, so its stall judge can tell a person.
    flotilla_engine_messages_testkit::designate_fleet(&follower.resource_backend(), "flotilla").await;
    // The convoy's project, whose operator is told; it declares no governor.
    follower
        .resource_backend()
        .definitions::<flotilla_resources::Project>("flotilla")
        .apply(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder()
                .display_name("Project".into())
                .default_workflow_ref("scratch".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("project");
    // A working coder in a realised vessel, with its running session.
    off_frame(|| seed_supervision_crew_in(&follower, "flotilla", "stalled-work", "work", "coder")).await;
    let launch = current_launch(&follower, "stalled-work");
    // A crew's stall is its home's note (#3108).
    flotilla_store::note_convoy(
        &follower.convoy_engine_path(),
        &remote,
        &launch,
        &flotilla_resources::external_patches::mark_crew_stalled(
            launch.clone(),
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
    // With no governor the judge tells the operator: the attention is what reaches another host.
    follower.reconcile_crew_stalls_once("flotilla").await.expect("stall judge pass");
    let topology = spawn_in_memory_request_topology_stateful_with_surface(
        Arc::clone(&leader),
        follower,
        SurfaceDeclaration::focal_for_namespace("flotilla"),
    )
    .await
    .expect("operator connection");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the reader follows the stall's home", || async {
        leader.resource_backend().including_replicas::<Convoy>("flotilla").get(&launch).await.is_ok()
    })
    .await;
    let stalls = || async {
        let result = topology
            .client
            .execute_query(Command::builder().action(CommandAction::QueryCrewStalls { full: true }).build(), uuid::Uuid::nil())
            .await
            .expect("operator stalls");
        let CommandValue::CrewStalls(response) = result else { panic!("expected stalls: {result:?}") };
        response
    };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "the reader lists the remote stall", || async {
        !stalls().await.rows.is_empty()
    })
    .await;
    let response = stalls().await;
    // The row carries what the attention states: the convoy, the role and the operator rung. Its
    // vessel, evidence, reason and disposition are the home's note, so they are absent here.
    assert_eq!(response.rows.len(), 1, "{:?}", response.rows);
    let row = &response.rows[0];
    assert_eq!(row.namespace, "flotilla");
    assert_eq!(row.convoy, launch);
    assert_eq!(row.role, "coder");
    assert_eq!(row.rung, Some(flotilla_protocol::StallRung::Operator));
    assert_eq!((row.vessel.as_deref(), row.evidence.as_deref(), row.proposed_disposition), (None, None, None));
    // The attention states when it was admitted, not when the stall began.
    assert!(row.raised_at.is_some(), "{row:?}");
    assert_eq!((row.began_at, row.age_seconds), (None, None));
}

mod change_request_observation;
mod image_layers;
mod message_homes;

// Project roles retain a home before admission or launch. The ordinary router
// must create their intent there and admit a reply addressed to a system sender.
async fn run_message_role_waiting_home_case(remote: bool, admitted: bool) {
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, Message, MessagePhase};
    let topology =
        spawn_in_memory_request_topology_stateful(empty_daemon_named("message-caller").await, empty_daemon_named("role-home").await)
            .await
            .expect("role Message topology");
    let home = if remote { &topology.follower } else { &topology.leader };
    let home_backend = home.resource_backend();
    let declarations = home_backend.using::<ConvoyEnsure>("flotilla");
    declarations
        .create(
            &InputMeta::builder().name("waiting-governor".into()).build(),
            &ConvoyEnsureSpec::builder().project_ref("flotilla".into()).role("governor".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("declared standing role");
    if admitted {
        // Admitted on the engine, with no terminal, as the declared role's current generation.
        let mut standing = engine_convoy(
            "standing",
            ConvoySpec::builder().workflow_ref("standing".into()).project_ref("flotilla".into()).role("governor".into()).build(),
            ConvoyStatus::default(),
        );
        standing.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), "waiting-governor".into());
        admit_convoy(home, standing).await;
    }
    if remote {
        // The declaration reaches the caller through its home's log.
        let caller = topology.leader.resource_backend().definitions::<ConvoyEnsure>("flotilla");
        tokio::time::timeout(Duration::from_secs(5), async {
            while caller.get("waiting-governor").await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the caller follows the declaration from its home");
    }
    for (name, intent) in [
        (
            "request",
            serde_json::json!({"sender":"fleet:lab/role:turn-rules","inbox":"project:flotilla/role:governor","relation":"system","body":"settled checks","expectation":{"kind":"reply"}}),
        ),
        (
            "reply",
            serde_json::json!({"sender":"project:flotilla/role:governor","inbox":"fleet:lab/role:turn-rules","relation":"peer","body":"received","in_reply_to":"request"}),
        ),
    ] {
        if remote && name == "reply" {
            // The reply is routed by its original, which the caller reads once it follows the home's log.
            let caller = topology.leader.message_primitives();
            let inbox = caller.inbox_for("flotilla", "flotilla/governor").await.expect("the request's inbox");
            tokio::time::timeout(Duration::from_secs(5), async {
                while caller.locate_in(&inbox, "request").expect("locate").is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the caller follows the request from its home");
        }
        let mut events = topology.leader.subscribe();
        let id = topology.client.execute(Command::builder().action(CommandAction::ResourceApply { namespace: "flotilla".into(), document: serde_json::json!({"apiVersion":"flotilla.work/v1","kind":"Message","metadata":{"name":name},"spec":intent}) }).build()).await.expect("ordinary Message mutation");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, node_id, result, .. } = events.recv().await.expect("command result") {
                    if command_id == id {
                        assert_eq!(node_id, *home.node_id());
                        break result;
                    }
                }
            }
        })
        .await
        .expect("receiver-home admission completes");
        assert!(matches!(result, CommandValue::ResourceObject(_)), "{name}: {result:?}");
        let record = home_backend.using::<Message>("flotilla").get(name).await.expect("home-authored Message");
        // Admitted at its home on the engine: pending there, which reads as deliverable.
        assert_eq!(record.status.expect("admitted status").phase, MessagePhase::Deliverable);
        if remote {
            assert!(
                topology.leader.resource_backend().using::<Message>("flotilla").get(name).await.is_err(),
                "caller must not author receiver intent"
            );
        }
    }
}

// A producer in the daemon (a crew turn, a handoff) sends through the router: a receiver homed
// here is admitted by a direct call, with no command round on this daemon's event stream, and
// one homed elsewhere is admitted there, never here.
#[tokio::test]
async fn a_producers_send_is_admitted_at_its_home_without_a_local_command() {
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec, Message, MessagePhase};
    for remote in [false, true] {
        let topology =
            spawn_in_memory_request_topology_stateful(empty_daemon_named("message-caller").await, empty_daemon_named("role-home").await)
                .await
                .expect("role Message topology");
        let home = if remote { &topology.follower } else { &topology.leader };
        home.resource_backend()
            .using::<ConvoyEnsure>("flotilla")
            .create(
                &InputMeta::builder().name("waiting-governor".into()).build(),
                &ConvoyEnsureSpec::builder().project_ref("flotilla".into()).role("governor".into()).repositories(Vec::new()).build(),
            )
            .await
            .expect("declared standing role");
        if remote {
            let caller = topology.leader.resource_backend().definitions::<ConvoyEnsure>("flotilla");
            tokio::time::timeout(Duration::from_secs(5), async {
                while caller.get("waiting-governor").await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the caller follows the declaration from its home");
        }
        let intent: flotilla_messaging::Intent = serde_json::from_value(serde_json::json!({
            "sender": "fleet:lab/role:turn-rules", "inbox": "project:flotilla/role:governor", "relation": "system", "body": "settled checks"
        }))
        .expect("intent");
        let mut events = topology.leader.subscribe();
        let sent = topology.leader.publish_message("flotilla", "request", &intent).await.expect("producer send");
        assert_eq!(sent.name, "request");
        let record = home.resource_backend().using::<Message>("flotilla").get("request").await.expect("home-authored Message");
        assert_eq!(record.status.expect("admitted status").phase, MessagePhase::Deliverable);
        if remote {
            assert!(topology.leader.resource_backend().using::<Message>("flotilla").get("request").await.is_err(), "authored at its home");
        }
        // A repeat is the message already admitted.
        topology.leader.publish_message("flotilla", "request", &intent).await.expect("repeated send");
        loop {
            match events.try_recv() {
                // A remote home's command reports its progress here, under that home's node.
                Ok(DaemonEvent::CommandStarted { description, node_id, .. }) if node_id == *topology.leader.node_id() => {
                    panic!("the send ran a command here (remote: {remote}): {description}")
                }
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }
}

#[tokio::test]
async fn message_role_waiting_home_pinned_rows() {
    for remote in [false, true] {
        for admitted in [false, true] {
            run_message_role_waiting_home_case(remote, admitted).await;
        }
    }
}

#[hegel::test]
fn generated_message_role_waiting_home(tc: hegel::TestCase) {
    // Cover both declaration origins with and without an admitted convoy;
    // all four rows include the correlated system reply mutation.
    let remote = tc.draw(hegel::generators::booleans());
    let admitted = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("scenario runtime")
        .block_on(run_message_role_waiting_home_case(remote, admitted));
}

async fn capabilities_routing_scenario(remote: bool, workflows: bool) {
    use flotilla_capability_contract::CredentialCapability;
    use flotilla_capability_contract::SessionCapabilitySource;
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec};
    // Credential-delivery I/O is the boundary; routing and stores are real.
    struct Delivery(bool);
    #[async_trait]
    impl SessionCapabilitySource for Delivery {
        async fn credentials(&self, _: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
            assert_eq!(references, &BTreeSet::from(["github".into()]));
            let mut permissions = BTreeMap::from([("contents".into(), "write".into())]);
            if self.0 {
                permissions.insert("workflows".into(), "write".into());
            }
            Ok(vec![CredentialCapability::builder()
                .name("github".into())
                .repositories(vec!["flotilla-org/flotilla".into()])
                .permissions(permissions)
                .build()])
        }
    }
    let leader = empty_daemon_named("capabilities-desk").await;
    let follower = empty_daemon_named("capabilities-session").await;
    let topology = spawn_in_memory_request_topology_stateful(leader, follower).await.expect("connect capability scenario");
    let owner = if remote { &topology.follower } else { &topology.leader };
    // Each daemon reads the engine's environments, as its runtime installs them.
    let leader_environments = message_homes::engine_environments(&topology.leader);
    let follower_environments = message_homes::engine_environments(&topology.follower);
    let owner_environments = if remote { &follower_environments } else { &leader_environments };
    let role = if workflows { "governor" } else { "coder" };
    topology.leader.set_session_capability_source(Arc::new(Delivery(!workflows))).await;
    owner.set_session_capability_source(Arc::new(Delivery(workflows))).await;
    // The convoy is the leader's engine launch; its work's seat owes what an agent's does.
    let convoy = admit_convoy(
        &topology.leader,
        engine_convoy(
            "capabilities",
            convoy_spec("scratch", "capabilities"),
            ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    vessels: vec![VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![flotilla_engine_convoys_testkit::agent(role)])
                        .credential_refs(BTreeSet::from(["github".into()]))
                        .build()],
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                }),
                ..Default::default()
            },
        ),
    )
    .await;
    // The vessel is a launch at the owner of the convoy's work; the convoy is homed at the leader.
    let vessel = engine(owner).vessels.request_for(&flotilla_resources::vessel_records::VesselLaunchRequest {
        convoy: convoy.clone(),
        convoy_home: topology.leader.node_id().as_str().into(),
        work: "work".into(),
        host: "session-host".into(),
    });
    owner_environments.request(
        "capabilities-env",
        &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "session-host".into(), repo_default_dir: "/repo".into() }),
            ..Default::default()
        },
        None,
    );
    // The session is the owner's engine launch for the vessel; its credential grants are the
    // vessel requirement's, frozen in the convoy's workflow snapshot.
    let terminals = &engine(owner).terminals;
    let launch = terminals.request(&TerminalLaunchRequest {
        session: "capabilities-terminal".into(),
        environment: "capabilities-env".into(),
        vessel: Some(vessel.clone()),
        role: role.into(),
        pool: "cleat".into(),
        cwd: "/repo".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "code".into(),
            adapter: None,
            model: None,
            brief: TerminalBrief { path: "brief.md".into(), content: String::new(), artifact_digest: None, copies: vec![] },
        },
    });
    terminals.ready(&launch, "capabilities-terminal", &Default::default());
    if remote {
        // The leader routes by the owner's launches, which reach it with the owner's log over the
        // topology's own change streams. A copy sent beside them races their rounds: a round
        // staged before the launch installs after it and resets the home without it.
        let sessions = topology.leader.resource_backend().including_replicas::<TerminalSession>("flotilla");
        eventually(Duration::from_secs(5), Duration::from_millis(10), "the owner's session should replicate to the leader", || async {
            sessions.get("capabilities-terminal").await.is_ok_and(|read| matches!(read.provenance, ResourceProvenance::Replica { .. }))
        })
        .await;
        apply_convoy_replica_feed(&topology.follower, "flotilla", &convoy, topology.leader_host.clone()).await;
        // The convoy reaches the owner with the leader's log.
        let convoys = topology.follower.resource_backend().including_replicas::<Convoy>("flotilla");
        eventually(Duration::from_secs(5), Duration::from_millis(10), "the convoy should replicate to the owner", || async {
            convoys.get(&convoy).await.is_ok()
        })
        .await;
    }
    let context = CrewCommandContext::builder()
        .namespace("flotilla".into())
        .convoy(convoy.clone())
        .vessel_ref(vessel.clone())
        .role(role.into())
        .build();
    let target = topology
        .leader
        .resolve_command_target(&CommandAction::QueryCrewCapabilities { context: context.clone() }, None)
        .await
        .expect("target");
    assert_eq!(target.reason, flotilla_core::command_target::TargetReason::CrewSessionHome);
    assert_eq!(target.host, if remote { TargetHost::Node(owner.node_id().clone()) } else { TargetHost::Local });
    let response = topology
        .client
        .execute_query(Command::builder().action(CommandAction::QueryCrewCapabilities { context }).build(), uuid::Uuid::new_v4())
        .await
        .expect("query capabilities");
    let CommandValue::CrewCapabilities { card } = response else { panic!("capabilities response: {response:?}") };
    assert!(card.contains(&format!("You act as `{role}`")));
    assert_eq!(card.contains("You can push `.github/workflows`"), workflows);
}

// Pinned local/remote rows prove capabilities come from the session's actual
// delivery host, even when the convoy authority has a different grant.
#[tokio::test]
async fn router_capabilities_scenarios_use_session_home() {
    for remote in [false, true] {
        for workflows in [false, true] {
            capabilities_routing_scenario(remote, workflows).await;
        }
    }
}

#[hegel::test]
fn generated_capabilities_queries_use_session_home(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let remote = tc.draw(gs::booleans());
    let workflows = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(capabilities_routing_scenario(remote, workflows));
}

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

// Sender records end on the receiver's replicated drop, across duplicate delivery and restart.
#[hegel::test]
fn messages_are_homed_at_the_receiver_and_replies_route_by_subject(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Generate short cancellation sequences: supersede, withdraw, expire;
    // records may arrive after the drop, and the observer may restart between steps.
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    use message_homes::{DropAction, DropStep};
    // Pin drop-before-record with a cold observer in this same shared scenario.
    let mut operations = vec![DropStep { action: DropAction::Withdraw, late_record: true, restart: true }];
    operations.extend((0..steps).map(|_| DropStep {
        action: [DropAction::Supersede, DropAction::Withdraw, DropAction::Expire]
            [tc.draw(gs::integers::<usize>().min_value(0).max_value(2))],
        late_record: tc.draw(gs::booleans()),
        restart: tc.draw(gs::booleans()),
    }));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(message_homes::messages_are_homed_at_the_receiver_and_replies_route_by_subject(&operations));
}

// Pinned routing/admission rows: an interactive user or crew cannot acquire replication
// cursors from either its local daemon or a routed home, though every daemon holds a decided store.
#[tokio::test]
async fn router_change_stream_admission_scenario_rows() {
    for (name, remote, crew) in
        [("local-user", false, false), ("remote-user", true, false), ("local-crew", false, true), ("remote-crew", true, true)]
    {
        let leader = empty_daemon_named("desk").await;
        let follower = empty_daemon_named("home").await;
        let principal_ref = PrincipalRef::implicit_for_namespace("flotilla");
        let caller = CommandCaller {
            principal_ref: principal_ref.clone(),
            process: None,
            crew: crew.then(|| {
                CallerCrew::builder()
                    .namespace("flotilla".to_string())
                    .convoy("work".to_string())
                    .vessel("work".to_string())
                    .role("coder".to_string())
                    .crew_id("test-crew".to_string())
                    .build()
            }),
        };
        let topology = spawn_in_memory_request_topology_stateful_with_caller(
            leader,
            follower,
            SurfaceDeclaration { principal_ref, character: SurfaceCharacter::Focal },
            caller,
        )
        .await
        .unwrap();
        let target = if remote { topology.follower.node_id() } else { topology.leader.node_id() };
        let error = topology
            .client
            .execute_query(
                Command::builder()
                    .node_id(target.clone())
                    .action(CommandAction::QueryChangeStream { home: None, after: None, session: None })
                    .build(),
                uuid::Uuid::nil(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("peer replication transport"), "{name}: {error}");
    }
}
