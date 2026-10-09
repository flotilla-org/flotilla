//! Standing-convoy behavior scenarios driven by an injected controller.
use async_trait::async_trait;
use chrono::Utc;
use flotilla_core::config::ConfigStore;
use flotilla_protocol::{CommandAction, HostName, NodeId};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, Clock, Convoy as ResourceConvoy, ConvoyEnsure,
    Host as ResourceHost, InMemoryBackend, InputMeta, Project, ProjectSpec, Repository, RepositoryKey, RepositorySpec, ResourceBackend,
    ResourceError, ResourceObject, WorkflowTemplate,
};
use std::{sync::Arc, time::Duration};

use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone;
use flotilla_core::in_process::*;
use flotilla_core::ops_entry::PRESENTS_AS_ANNOTATION;
use flotilla_core::ops_entry::SOURCE_ENTRY_PATH_ANNOTATION;
use flotilla_core::ops_entry::SOURCE_REPOSITORY_ANNOTATION;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_resources::controller_patches;
use flotilla_resources::ConvoyEnsureHoldReason;
use flotilla_resources::ConvoyEnsureSpec;
use flotilla_resources::ConvoyEnsureStatus;
use flotilla_resources::ConvoyPhase;
use flotilla_resources::ConvoyProvisioningState;
use flotilla_resources::ConvoyStatus;
use flotilla_resources::CrewSource;
use flotilla_resources::CrewSpec;
use flotilla_resources::CrewWorkPhase;
use flotilla_resources::CrewWorkState;
use flotilla_resources::Demand as ResourceDemand;
use flotilla_resources::DemandKind;
use flotilla_resources::DemandSpec;
use flotilla_resources::DemandStatusPatch;
use flotilla_resources::Environment as ResourceEnvironment;
use flotilla_resources::EnvironmentPhase;
use flotilla_resources::EnvironmentSpec as ResourceEnvironmentSpec;
use flotilla_resources::EnvironmentStatus as ResourceEnvironmentStatus;
use flotilla_resources::Event;
use flotilla_resources::FulfilmentKindSpec;
use flotilla_resources::HarnessFacts;
use flotilla_resources::HostDirectPlacementPolicyCheckout;
use flotilla_resources::HostDirectPlacementPolicySpec;
use flotilla_resources::HostSpec;
use flotilla_resources::HostStatus;
use flotilla_resources::PlacementPolicy;
use flotilla_resources::PlacementPolicySpec;
use flotilla_resources::ProjectRepositoryRole;
use flotilla_resources::ProjectRepositorySpec;
use flotilla_resources::RepositoryStatus;
use flotilla_resources::Selector;
use flotilla_resources::TerminalSession as ResourceTerminalSession;
use flotilla_resources::TerminalSessionSource;
use flotilla_resources::TerminalSessionSpec as ResourceTerminalSessionSpec;
use flotilla_resources::Vessel;
use flotilla_resources::VesselRequirement;
use flotilla_resources::VesselSpec;
use flotilla_resources::WorkflowTemplateSpec;
use flotilla_resources::AUTHORITY_LABEL;
use flotilla_resources::CONVOY_LABEL;
use flotilla_resources::DRIVER_ADMISSION_CONDITION_TYPE;
use flotilla_resources::GENERATION_LABEL;
use flotilla_resources::PROJECT_LABEL;
use flotilla_resources::ROLE_LABEL;
use flotilla_store_testkit::VirtualClock;

#[path = "ensure_scenarios/admission.rs"]
mod admission;
pub use admission::*;
#[path = "ensure_scenarios/retry.rs"]
mod retry;
pub use retry::*;
#[path = "ensure_scenarios/teardown.rs"]
mod teardown;
pub use teardown::*;
#[path = "ensure_scenarios/addressing.rs"]
mod addressing;
pub use addressing::*;

/// Injects the controller crate at the composition boundary without a production cycle.
#[async_trait]
pub trait EnsureScenarioController: Send + Sync {
    fn create(&self, backend: ResourceBackend, clock: Arc<dyn Clock>) -> Arc<dyn ConvoyEnsureReconciler>;
    async fn dependency_hash(
        &self,
        daemon: &InProcessDaemon,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String>;
    async fn start(&self, daemon: &InProcessDaemon, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String>;
}

async fn assert_independent_placement_snapshots(factory: &dyn EnsureScenarioController, replica_before_admission: bool) {
    let (first, first_backend, _first_clock, _first_temp) = standing_ensure_fixture_for(factory, "feta", true).await;
    let (second, second_backend, _second_clock, _second_temp) = standing_ensure_fixture_for(factory, "udder", true).await;
    for backend in [&first_backend, &second_backend] {
        configure_standing_ensure_agent(backend, Vec::new()).await;
    }

    // Exercise both simultaneous first admissions and an already-visible replica.
    first.reconcile_convoy_ensures_once("flotilla").await.expect("first admission");
    if replica_before_admission {
        let mut snapshots = first.resource_backend().using::<PlacementPolicy>("flotilla").list().await.expect("first placements");
        snapshots.items.retain(|policy| policy.metadata.name.starts_with("placement-snapshot-"));
        second
            .resource_backend()
            .replica_writer::<PlacementPolicy>(first.node_id().clone(), "flotilla")
            .replace(&snapshots, Utc::now())
            .await
            .expect("replicate before second admission");
    }
    second.reconcile_convoy_ensures_once("flotilla").await.expect("second admission");
    let first_store = first.resource_backend();
    let second_store = second.resource_backend();
    for (source, destination, root) in
        [(&first_store, &second_store, first.node_id().clone()), (&second_store, &first_store, second.node_id().clone())]
    {
        let mut snapshots = source.using::<PlacementPolicy>("flotilla").list().await.expect("placement log");
        snapshots.items.retain(|policy| policy.metadata.name.starts_with("placement-snapshot-"));
        destination
            .replica_writer::<PlacementPolicy>(root, "flotilla")
            .replace(&snapshots, Utc::now())
            .await
            .expect("exchange placement snapshots");
    }
    for store in [&first_store, &second_store] {
        assert!(flotilla_resources::home_bound_authorship_collisions(store, "flotilla").await.expect("diagnostics").is_empty());
    }
    // A remote reference may still be in flight when the first origin releases
    // its convoy. The second admission must retain its own frozen placement.
    let first_convoy = first_store.using::<ResourceConvoy>("flotilla").list().await.expect("first convoys").items.remove(0);
    flotilla_resources::PreparedSnapshotGarbageCollector::new(first_store.clone(), "flotilla")
        .collect(Some(&first_convoy.metadata.name))
        .await
        .expect("collect first origin snapshots");
    let second_convoy = second_store.using::<ResourceConvoy>("flotilla").list().await.expect("second convoys").items.remove(0);
    assert_ne!(
        first_convoy.metadata.annotations[flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION],
        second_convoy.metadata.annotations[flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION],
        "origins must own distinct snapshot identities even when replicas are visible"
    );
    let snapshot = &second_convoy.metadata.annotations[flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION];
    second_store.using::<PlacementPolicy>("flotilla").get(snapshot).await.expect("second origin retains its placement");
}

async fn create_identity_convoy(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), project.unwrap_or_default().to_string()),
        (ROLE_LABEL.to_string(), role.to_string()),
        (GENERATION_LABEL.to_string(), "1".to_string()),
    ]);
    let mut spec = ConvoySpec::builder().workflow_ref("review".to_string()).role(role.to_string()).generation(1).build();
    spec.project_ref = project.map(str::to_string);
    backend
        .clone()
        .using::<ResourceConvoy>("flotilla")
        .create(&InputMeta::builder().name(record.to_string()).labels(labels).build(), &spec)
        .await
        .expect("convoy");
}

async fn seed_convoy_routing_row(
    daemon: &InProcessDaemon,
    record: &str,
    role: Option<&str>,
    project: Option<&str>,
    phase: flotilla_protocol::ConvoyPhase,
) {
    let resource =
        flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", record).on_host(daemon.host_name().clone());
    let mut row = flotilla_protocol::ConvoyRow::builder()
        .resource(resource.clone())
        .maybe_address_role(role.map(str::to_string))
        .name(role.unwrap_or(record).to_string())
        .workflow_ref("review".to_string())
        .phase(phase)
        .build();
    row.project_ref = project.map(str::to_string);
    daemon.aggregator_projection_state().await.write().await.local_rows.insert(resource, row);
}

fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

async fn standing_ensure_fixture(
    factory: &dyn EnsureScenarioController,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<VirtualClock>, tempfile::TempDir) {
    standing_ensure_fixture_for(factory, "local", true).await
}

async fn standing_ensure_fixture_for(
    factory: &dyn EnsureScenarioController,
    host: &str,
    materialize_ensure: bool,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<VirtualClock>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"standing-{host}\"\n")).expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("timestamp");
    let clock = Arc::new(VirtualClock::new(now));
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new(host),
        backend.clone(),
        clock.clone(),
    )
    .await;
    daemon.install_convoy_ensure_reconciler(factory.create(daemon.resource_backend(), daemon.clock_for_scenarios())).await;
    let repository_spec = RepositorySpec::remote("https://github.com/acme/standing").expect("repository spec");
    let repository_key = repository_spec.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository_spec).await.expect("repository");
    if materialize_ensure {
        backend
            .definitions::<Project>("flotilla")
            .create(
                &test_meta("standing-project"),
                &ProjectSpec::builder()
                    .display_name("Standing Project".to_string())
                    .default_workflow_ref("quartermaster".to_string())
                    .repositories(vec![ProjectRepositorySpec {
                        charter_store: None,
                        repo: repository_key.clone(),
                        alias: Some("app".to_string()),
                        roles: BTreeSet::from([ProjectRepositoryRole::Code]),
                        subpath: None,
                        default_branch: Some("main".to_string()),
                    }])
                    .build(),
            )
            .await
            .expect("project");
    }
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder()
                .name(flotilla_core::ops_entry::materialized_workflow_name("standing-project", "quartermaster"))
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string())]))
                .build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key.clone()])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("standing workflow");
    if materialize_ensure {
        backend
            .definitions::<ConvoyEnsure>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("quartermaster".to_string())
                    .annotations(BTreeMap::from([
                        (MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string()),
                        (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository_key.to_string()),
                        (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
                        (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), "ops/quartermaster.md".to_string()),
                    ]))
                    .build(),
                &ConvoyEnsureSpec {
                    project_ref: "standing-project".to_string(),
                    role: "quartermaster".to_string(),
                    driver_ref: None,
                    workflow_ref: "quartermaster".to_string(),
                    placement_policy: None,
                    escalation_reason: None,
                    repositories: vec![repository_key],
                    presents_as: Some("fleet".to_string()),
                    agent_overrides: Vec::new(),
                },
            )
            .await
            .expect("ensure declaration");
    }
    (daemon, backend, clock, temp)
}

struct VerifiedDeadBacking;

#[async_trait]
impl StandingConvoyBackingInspector for VerifiedDeadBacking {
    async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        Ok(())
    }
}

struct RecordlessBacking;

#[async_trait]
impl StandingConvoyBackingInspector for RecordlessBacking {
    async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        Err("no backing environment evidence is available".to_string())
    }
}

async fn fail_ensured_generation(backend: &ResourceBackend, clock: &VirtualClock) -> String {
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("live generation");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("generation");
    convoys
        .update_status(
            &convoy_ref,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                message: Some("placement failed".to_string()),
                started_at: Some(clock.now()),
                finished_at: Some(clock.now()),
                ..Default::default()
            },
        )
        .await
        .expect("fail generation");
    convoy_ref
}

async fn fail_latest_ensured_generation(backend: &ResourceBackend, clock: &VirtualClock) -> String {
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .list()
        .await
        .expect("generations")
        .items
        .into_iter()
        .max_by_key(|convoy| convoy.spec.generation)
        .expect("latest generation");
    clock.set(convoy.metadata.creation_timestamp);
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                message: Some("placement failed".to_string()),
                started_at: Some(clock.now()),
                finished_at: Some(clock.now()),
                ..Default::default()
            },
        )
        .await
        .expect("fail generation");
    convoy.metadata.name
}

async fn configure_standing_ensure_agent(backend: &ResourceBackend, overrides: Vec<flotilla_protocol::AgentOverride>) {
    create_docker_placement(backend, "standing-agent", "standing-agent-host", BTreeSet::new()).await;
    let policy = backend.using::<PlacementPolicy>("flotilla").get("standing-agent").await.expect("policy");
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(&test_meta("standing-agent"), &FulfilmentKindSpec::from_policy(&policy.spec, "linux").expect("kind"))
        .await
        .expect("kind");
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("standing-agent-host").await.expect("standing agent host");
    let mut status = host.status.expect("standing agent host status");
    status.fulfilment_facts.insert(
        "standing-agent".into(),
        flotilla_resources::FulfilmentFacts {
            harnesses: BTreeMap::from([("codex".into(), HarnessFacts { version: "0.160.0".into(), models: BTreeMap::new() })]),
            ..Default::default()
        },
    );
    status.disk_free_bytes = Some(100 * 1024 * 1024 * 1024);
    status.admission_free_space_floor_bytes = Some(20 * 1024 * 1024 * 1024);
    hosts.update_status(&host.metadata.name, &host.metadata.resource_version, &status).await.expect("standing agent host capacity");
    let workflows = backend.using::<WorkflowTemplate>("flotilla");
    let mut workflow = workflows.get("standing-project--quartermaster").await.expect("standing workflow");
    workflow.spec.vessels[0].crew = vec![CrewSpec::builder()
        .role("governor".to_string())
        .source(CrewSource::Agent {
            selector: Selector { capability: "governor".to_string(), adapter: Some("codex".to_string()), model: None },
            prompt: None,
            brief_template: None,
        })
        .build()];
    workflows
        .update(&InputMeta::from(&workflow.metadata), &workflow.metadata.resource_version, &workflow.spec)
        .await
        .expect("update standing workflow");

    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let mut ensure = ensures.get("quartermaster").await.expect("standing ensure");
    ensure.spec.placement_policy = Some("standing-agent".to_string());
    ensure.spec.agent_overrides = overrides;
    ensures
        .update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &ensure.spec)
        .await
        .expect("update standing ensure");
}

async fn admitted_standing_workflow(daemon: &InProcessDaemon, backend: &ResourceBackend) -> WorkflowTemplateSpec {
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("admit standing convoy");
    let convoy = backend
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("list admitted convoys")
        .items
        .into_iter()
        .next()
        .expect("admitted convoy");
    let snapshot = convoy.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("workflow snapshot annotation");
    backend.using::<WorkflowTemplate>("flotilla").get(snapshot).await.expect("workflow snapshot").spec
}

#[derive(Default)]
struct RecordingBriefArtifacts {
    writes: tokio::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
}

#[async_trait]
impl BriefArtifactWriter for RecordingBriefArtifacts {
    async fn put_brief(
        &self,
        _namespace: &str,
        convoy: &str,
        role: &str,
        subject: &str,
        content: &[u8],
        _charter_commit: Option<&str>,
    ) -> Result<String, String> {
        assert_eq!(subject, convoy);
        self.writes.lock().await.push((convoy.to_string(), role.to_string(), content.to_vec()));
        Ok(format!("{:x}", Sha256::digest(content)))
    }
}

async fn set_ensure_driver(backend: &ResourceBackend, driver_ref: &str) {
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let mut spec = ensure.spec;
    spec.driver_ref = Some(driver_ref.to_string());
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &spec).await.expect("set ensure driver");
}

async fn placement_policy(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<PlacementPolicy> {
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_ref.to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("placement policy")
}

async fn create_docker_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, held_credentials: BTreeSet<String>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(host_ref),
            &HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("host create");
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &HostStatus {
                capabilities: [
                    (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials)),
                    ("docker".to_string(), serde_json::json!(true)),
                    ("os".to_string(), serde_json::json!("linux")),
                ]
                .into_iter()
                .collect(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status update");
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(policy_name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: host_ref.to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("placement create");
}

use chrono::Duration as ChronoDuration;
use flotilla_core::ops_entry::{MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION};
use flotilla_protocol::{PrincipalRef, ResourceRef};
use flotilla_resources::{
    apply_status_patch as apply_resource_status_patch, CheckoutStatus as ResourceCheckoutStatus, ConvoySpec, FulfilmentKind,
    LifecycleAuthority,
};
use sha2::{Digest, Sha256};

use flotilla_core::in_process::convoy_admission::allocate_convoy_generation;
