//! Standing-convoy behavior scenarios driven by an injected controller.
use async_trait::async_trait;
use chrono::Utc;
use flotilla_admission::convoy_ensure::{ConvoyEnsureAdmission, ConvoyEnsureReconciler, StandingConvoyBackingInspector};
use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_config::ConfigStore;
use flotilla_protocol::{CommandAction, HostName, NodeId};
use flotilla_resources::BRIEF_ARTIFACTS_ANNOTATION;
use flotilla_resources::{
    CheckoutSpec as ResourceCheckoutSpec, Clock, Convoy as ResourceConvoy, ConvoyEnsure, Host as ResourceHost, InputMeta, Project,
    ProjectSpec, Repository, RepositoryKey, RepositorySpec, ResourceObject, WorkflowTemplate,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use std::{sync::Arc, time::Duration};

use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone;
use flotilla_charter::ops_entry::PRESENTS_AS_ANNOTATION;
use flotilla_charter::ops_entry::SOURCE_ENTRY_PATH_ANNOTATION;
use flotilla_charter::ops_entry::SOURCE_REPOSITORY_ANNOTATION;
use flotilla_core::in_process::*;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_resources::attentions::{OpenAttention, Sender};
use flotilla_resources::controller_patches;
use flotilla_resources::ConvoyEnsureHoldReason;
use flotilla_resources::ConvoyEnsureSpec;
use flotilla_resources::ConvoyEnsureStatus;
use flotilla_resources::ConvoyPhase;
use flotilla_resources::ConvoyStatus;
use flotilla_resources::CrewSource;
use flotilla_resources::CrewSpec;
use flotilla_resources::EnvironmentSpec as ResourceEnvironmentSpec;
use flotilla_resources::FulfilmentKindSpec;
use flotilla_resources::FulfilmentRealisation;
use flotilla_resources::HarnessFacts;
use flotilla_resources::HostSpec;
use flotilla_resources::HostStatus;
use flotilla_resources::ProjectRepositoryRole;
use flotilla_resources::ProjectRepositorySpec;
use flotilla_resources::RepositoryStatus;
use flotilla_resources::Selector;
use flotilla_resources::VesselRequirement;
use flotilla_resources::WorkflowTemplateSpec;
use flotilla_resources::DRIVER_ADMISSION_CONDITION_TYPE;
use flotilla_resources::GENERATION_LABEL;
use flotilla_resources::PROJECT_LABEL;
use flotilla_resources::ROLE_LABEL;
use flotilla_store_testkit::VirtualClock;

#[path = "ensure_scenarios/admission.rs"]
mod admission;
pub use admission::*;

mod in_process;
pub use in_process::InProcessDiscoveryExt;
mod vessels;
pub use vessels::{run_vessels, RunningVessels};
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
    fn create(
        &self,
        teardown: Arc<dyn flotilla_admission::ports::ConvoyTeardown>,
        backend: ResourceBackend,
        clock: Arc<dyn Clock>,
    ) -> Arc<dyn ConvoyEnsureReconciler>;
    async fn dependency_hash(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        teardown: Arc<dyn flotilla_admission::ports::ConvoyTeardown>,
        backend: ResourceBackend,
        clock: Arc<dyn Clock>,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String>;
    async fn start(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        teardown: Arc<dyn flotilla_admission::ports::ConvoyTeardown>,
        backend: ResourceBackend,
        clock: Arc<dyn Clock>,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String>;
}

async fn assert_independent_placement_snapshots(factory: &dyn EnsureScenarioController, replica_before_admission: bool) {
    let (first, _first_backend, _first_clock, _first_temp) = standing_ensure_fixture_for(factory, "feta", true).await;
    let (second, _second_backend, _second_clock, _second_temp) = standing_ensure_fixture_for(factory, "udder", true).await;
    for daemon in [&first, &second] {
        configure_standing_ensure_agent(daemon, Vec::new()).await;
    }

    // Exercise both simultaneous first admissions and an already-visible replica.
    first.reconcile_convoy_ensures_once("flotilla").await.expect("first admission");
    if replica_before_admission {
        let mut snapshots = first.resource_backend().using::<FulfilmentKind>("flotilla").list().await.expect("first placements");
        snapshots.items.retain(|kind| kind.metadata.name.starts_with("placement-snapshot-"));
        second
            .resource_backend()
            .replica_writer::<FulfilmentKind>(first.node_id().clone(), "flotilla")
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
        let mut snapshots = source.using::<FulfilmentKind>("flotilla").list().await.expect("placement log");
        snapshots.items.retain(|kind| kind.metadata.name.starts_with("placement-snapshot-"));
        destination
            .replica_writer::<FulfilmentKind>(root, "flotilla")
            .replace(&snapshots, Utc::now())
            .await
            .expect("exchange placement snapshots");
    }
    for store in [&first_store, &second_store] {
        assert!(flotilla_store::home_bound_authorship_collisions(store, "flotilla").await.expect("diagnostics").is_empty());
    }
    // A remote reference may still be in flight when the first origin releases
    // its convoy. The second admission must retain its own frozen placement.
    let first_convoy = first_store.using::<ResourceConvoy>("flotilla").list().await.expect("first convoys").items.remove(0);
    flotilla_store::PreparedSnapshotGarbageCollector::new(first_store.clone(), "flotilla")
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
    second_store.using::<FulfilmentKind>("flotilla").get(snapshot).await.expect("second origin retains its placement");
}

/// Admits convoy `record` at `daemon`'s home with `role`'s identity in `project`; returns its launch.
async fn create_identity_convoy(daemon: &InProcessDaemon, record: &str, role: &str, project: Option<&str>) -> String {
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), project.unwrap_or_default().to_string()),
        (ROLE_LABEL.to_string(), role.to_string()),
        (GENERATION_LABEL.to_string(), "1".to_string()),
    ]);
    let mut spec = ConvoySpec::builder().workflow_ref("review".to_string()).role(role.to_string()).generation(1).build();
    spec.project_ref = project.map(str::to_string);
    let mut convoy = flotilla_engine_convoys_testkit::convoy("flotilla", record, spec, flotilla_engine_convoys_testkit::single_agent(role));
    convoy.metadata.labels = labels;
    flotilla_engine_convoys_testkit::launch(&daemon.convoy_engine_path(), convoy).await
}

/// Lands convoy `name` at `daemon`'s home: an operator keeps its work, and it is delivered.
async fn land_convoy(daemon: &InProcessDaemon, name: &str) {
    let convoy = daemon.resource_backend().using::<ResourceConvoy>("flotilla").get(name).await.expect("convoy");
    let engine = convoy_engine(daemon);
    let scope = flotilla_resources::convoy::engine::VerdictScope { vessel: "work".to_string(), role: None };
    engine.keep_by_verdict(&convoy, &scope, "operator", "landed").await.expect("keep the work");
    engine.deliver(&convoy, "claim").await.expect("deliver");
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

/// Installs the engine `daemon` reads its environments, clones and checkouts from, on its decided
/// store, as the runtime's composition does; a scenario daemon is not composed by the runtime.
fn install_engine(daemon: &InProcessDaemon) {
    // Attentions are Messages on the engine, and convoys its launches, as the daemon composes them.
    daemon.open_message_primitives().expect("the engine Message and Convoy paths");
    daemon.install_lifecycle_reads(engine(daemon).reads()).expect("install the engine's readings");
}

/// The standing role's holds open in `flotilla`.
async fn open_holds(backend: &ResourceBackend) -> Vec<OpenAttention> {
    let open = backend.attentions().open("flotilla", Some(Sender::Standing), None).await.expect("attentions");
    open.into_iter().filter(OpenAttention::is_hold).collect()
}

/// Every attention open in `flotilla`.
async fn open_attentions(backend: &ResourceBackend) -> Vec<OpenAttention> {
    backend.attentions().open("flotilla", None, None).await.expect("attentions")
}

/// A person acknowledges every message waiting for them at `daemon`'s home, and says how many.
async fn acknowledge_waiting(daemon: &InProcessDaemon) -> usize {
    let messages = daemon.message_primitives().expect("the engine Message path");
    let waiting = messages.person_messages("flotilla").await.expect("person messages");
    for (_, message) in &waiting {
        let record = flotilla_engine_messages::view::record_name(message.name()).expect("a message record");
        messages.acknowledge("flotilla", &record).await.expect("acknowledge");
    }
    waiting.len()
}

/// The engine over `daemon`'s decided store: one per store, so a scenario's writes go through the
/// same lifecycles its readings serve, and a daemon restarted over the store reads the same one.
fn engine(daemon: &InProcessDaemon) -> Arc<flotilla_provisioning_testkit::EngineFixture> {
    type Engines =
        std::sync::Mutex<Vec<(std::sync::Weak<dyn std::any::Any + Send + Sync>, Arc<flotilla_provisioning_testkit::EngineFixture>)>>;
    static ENGINES: std::sync::OnceLock<Engines> = std::sync::OnceLock::new();
    let service = daemon.decided_store().expect("decided store");
    let held: Arc<dyn std::any::Any + Send + Sync> = service.clone();
    let mut engines = ENGINES.get_or_init(Engines::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    engines.retain(|(store, _)| store.strong_count() > 0);
    if let Some((_, engine)) = engines.iter().find(|(store, _)| std::ptr::addr_eq(store.as_ptr(), Arc::as_ptr(&held))) {
        return Arc::clone(engine);
    }
    let engine = Arc::new(flotilla_provisioning_testkit::EngineFixture::over(service.home(), service.store()));
    engines.push((Arc::downgrade(&held), Arc::clone(&engine)));
    engine
}

/// The stamps project operations put on a definition they materialise for `project` from
/// `repository`: the project, and the repository and commit read.
fn project_ops_stamps(project: &str, repository: &RepositoryKey) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project.to_string()),
        (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository.to_string()),
        (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
    ])
}

/// The other home `home` writing for `daemon`, opened on first use. On the engine a record is
/// written only at its home; one written there reaches `daemon` as that home's log would, once
/// [`replicate_other_home`] sends it.
fn other_home(daemon: &InProcessDaemon, home: &str) -> Arc<flotilla_provisioning_testkit::OtherHome> {
    flotilla_provisioning_testkit::other_home_for(&daemon.decided_store().expect("decided store"), home)
}

/// Sends what the other home `home` holds to `daemon`, heard now.
fn replicate_other_home(daemon: &InProcessDaemon, home: &str) {
    other_home(daemon, home).replicate_to(&daemon.decided_store().expect("decided store"));
}

/// Writes `project` at the other home `home` with its stamps, and sends that home to `daemon`.
async fn project_at_other_home(daemon: &InProcessDaemon, home: &str, project: &ResourceObject<Project>) {
    other_home(daemon, home).project(&project.metadata.name, &project.metadata.labels, &project.metadata.annotations, &project.spec).await;
    replicate_other_home(daemon, home);
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
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("timestamp");
    let clock = Arc::new(VirtualClock::new(now));
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new(host),
        backend.clone(),
        clock.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    flotilla_engine_messages_testkit::designate_fleet(&backend, "flotilla").await;
    install_engine(&daemon);
    daemon
        .install_convoy_ensure_reconciler(factory.create(daemon.convoy_teardown(), daemon.resource_backend(), daemon.clock_for_scenarios()))
        .await;
    let repository_spec = RepositorySpec::remote("https://github.com/acme/standing").expect("repository spec");
    let repository_key = repository_spec.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository_spec).await.expect("repository");
    create_host_direct_placement((&*daemon).into(), &format!("host-direct-{host}-vessels"), &format!("{host}-vessels")).await;
    create_standing_agent_placement(&daemon).await;
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
                .name(flotilla_charter::ops_entry::materialized_workflow_name("standing-project", "quartermaster"))
                .annotations(project_ops_stamps("standing-project", &repository_key))
                .build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key.clone()])
                    .crew(vec![governor()])
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
                    placement_policy: Some("standing-agent".to_string()),
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

/// A standing convoy's obligation is its agent seats' lifetime duties, so its governor is an
/// agent, placed where a harness is (#3108: the engine refuses a convoy that owes nothing). This
/// is the `standing-agent` kind such an ensure places onto, at `daemon`.
async fn create_standing_agent_placement(daemon: &InProcessDaemon) {
    let facts = BTreeMap::from([(
        "standing-agent".into(),
        flotilla_resources::FulfilmentFacts {
            harnesses: BTreeMap::from([("codex".into(), HarnessFacts { version: "0.160.0".into(), models: BTreeMap::new() })]),
            ..Default::default()
        },
    )]);
    create_docker_placement(daemon.into(), "standing-agent", "standing-agent-host", BTreeSet::new(), facts).await;
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

/// The engine's Convoy path at `daemon`'s home.
fn convoy_engine(daemon: &InProcessDaemon) -> Arc<dyn flotilla_resources::convoy::engine::EngineConvoys> {
    Arc::clone(daemon.convoy_engine_path().get().expect("the engine Convoy path"))
}

/// Admits convoy `name` at `daemon`'s home as admission would, its one agent seat owing the
/// work of vessel `work`, and ends it as `phase` says when that is Failed or Abandoned. Returns
/// its launch's name: on the engine an operator's convoy is a launch (#3108).
async fn launch_convoy(daemon: &InProcessDaemon, name: &str, spec: ConvoySpec, phase: ConvoyPhase) -> String {
    let status = ConvoyStatus { phase, ..flotilla_engine_convoys_testkit::single_agent("governor") };
    flotilla_engine_convoys_testkit::launch(
        &daemon.convoy_engine_path(),
        flotilla_engine_convoys_testkit::convoy("flotilla", name, spec, status),
    )
    .await
}

/// Ends generation `name` Failed at `daemon`'s home now, as its work failing would. Status is
/// derived on the engine (#3108), so the failure is the engine's edge, not a status write.
async fn fail_generation(daemon: &InProcessDaemon, name: &str, reason: &str) {
    let convoy = daemon.resource_backend().using::<ResourceConvoy>("flotilla").get(name).await.expect("generation");
    convoy_engine(daemon).fail(&convoy, "operator", reason).await.expect("fail generation");
}

/// An operator abandons generation `name` at `daemon`'s home: on the engine a convoy ends and is
/// never deleted (#3108), so this is what reaping a live generation is.
async fn abandon_generation(daemon: &InProcessDaemon, name: &str) {
    let convoy = daemon.resource_backend().using::<ResourceConvoy>("flotilla").get(name).await.expect("generation");
    convoy_engine(daemon).abandon(&convoy, "operator", "operator reap").await.expect("abandon generation");
}

async fn fail_ensured_generation(daemon: &InProcessDaemon) -> String {
    let convoy_ref = daemon
        .resource_backend()
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("live generation");
    fail_generation(daemon, &convoy_ref, "placement failed").await;
    convoy_ref
}

async fn fail_latest_ensured_generation(daemon: &InProcessDaemon, clock: &VirtualClock) -> String {
    let convoy = daemon
        .resource_backend()
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("generations")
        .items
        .into_iter()
        .max_by_key(|convoy| convoy.spec.generation)
        .expect("latest generation");
    // It fails as soon as it was admitted, which the engine's view gives as its start.
    clock.set(convoy.status.as_ref().and_then(|status| status.started_at).expect("an admitted generation's start"));
    fail_generation(daemon, &convoy.metadata.name, "placement failed").await;
    convoy.metadata.name
}

/// Sets the standing ensure's agent overrides; the fixture already placed its governor agent.
async fn configure_standing_ensure_agent(daemon: &InProcessDaemon, overrides: Vec<flotilla_protocol::AgentOverride>) {
    let ensures = daemon.resource_backend().using::<ConvoyEnsure>("flotilla");
    let mut ensure = ensures.get("quartermaster").await.expect("standing ensure");
    ensure.spec.agent_overrides = overrides;
    ensures
        .update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &ensure.spec)
        .await
        .expect("update standing ensure");
}

/// The standing workflow's governor: an agent seat on a codex harness.
fn governor() -> CrewSpec {
    CrewSpec::builder()
        .role("governor".to_string())
        .source(CrewSource::Agent {
            selector: Selector { capability: "governor".to_string(), adapter: Some("codex".to_string()), model: None },
            prompt: None,
            brief_template: None,
        })
        .build()
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

async fn placement_kind(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<FulfilmentKind> {
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta(name),
            &FulfilmentKindSpec::builder()
                .host_ref(host_ref.to_string())
                .pool("passthrough".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("fulfilment kind")
}

/// Where a scenario's placement Hosts are written. A plain store holds whatever it is given; a
/// daemon's engine takes another host's Host only from that host's home (ADR 0064), so it is
/// written there and replicated in, as that host's daemon would publish it.
#[derive(Clone, Copy)]
enum HostsAt<'a> {
    Store(&'a ResourceBackend),
    Daemon(&'a InProcessDaemon),
}

impl<'a> From<&'a ResourceBackend> for HostsAt<'a> {
    fn from(backend: &'a ResourceBackend) -> Self {
        Self::Store(backend)
    }
}

impl<'a> From<&'a InProcessDaemon> for HostsAt<'a> {
    fn from(daemon: &'a InProcessDaemon) -> Self {
        Self::Daemon(daemon)
    }
}

impl HostsAt<'_> {
    fn backend(self) -> ResourceBackend {
        match self {
            Self::Store(backend) => backend.clone(),
            Self::Daemon(daemon) => daemon.resource_backend(),
        }
    }
}

/// A ready host with room for vessels, advertising the fixture providers and `facts`. Its
/// heartbeat, and when its home was last heard, stay fresh however far a scenario advances its
/// virtual clock.
async fn create_placement_host(
    at: HostsAt<'_>,
    host_ref: &str,
    capabilities: BTreeMap<String, serde_json::Value>,
    facts: BTreeMap<String, flotilla_resources::FulfilmentFacts>,
) {
    let fresh_until = Utc::now() + ChronoDuration::days(365);
    let spec = HostSpec { display_name: host_ref.to_string(), connection: Default::default(), ..HostSpec::default() };
    let mut capabilities = capabilities;
    capabilities.insert(
        flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
        serde_json::json!(flotilla_discovery_testkit::environment::ready_placement_advertisements()),
    );
    let status = HostStatus {
        capabilities,
        fulfilment_facts: facts,
        heartbeat_at: Some(fresh_until),
        ready: true,
        disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
        admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
        ..HostStatus::default()
    };
    match at {
        HostsAt::Daemon(daemon) => {
            let target = daemon.decided_store().expect("decided store");
            flotilla_provisioning_testkit::put_host_at_its_home_heard(&target, host_ref, &spec, Some(&status), fresh_until).await;
        }
        HostsAt::Store(backend) => {
            let hosts = backend.using::<ResourceHost>("flotilla");
            let host = hosts.create(&test_meta(host_ref), &spec).await.expect("host create");
            hosts.update_status(&host.metadata.name, &host.metadata.resource_version, &status).await.expect("host status update");
        }
    }
}

/// Registers the host-direct kind standing convoys place onto.
async fn create_host_direct_placement(at: HostsAt<'_>, kind_name: &str, host_ref: &str) {
    create_placement_host(at, host_ref, BTreeMap::new(), BTreeMap::new()).await;
    placement_kind(&at.backend(), kind_name, host_ref).await;
}

async fn create_docker_placement(
    at: HostsAt<'_>,
    kind_name: &str,
    host_ref: &str,
    held_credentials: BTreeSet<String>,
    facts: BTreeMap<String, flotilla_resources::FulfilmentFacts>,
) {
    create_placement_host(
        at,
        host_ref,
        BTreeMap::from([
            (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials)),
            ("docker".to_string(), serde_json::json!(true)),
            ("os".to_string(), serde_json::json!("linux")),
        ]),
        facts,
    )
    .await;
    at.backend()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta(kind_name),
            &FulfilmentKindSpec::builder()
                .host_ref(host_ref.to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([
                    flotilla_resources::FulfilmentGrant::platform("linux".to_string()),
                    flotilla_resources::FulfilmentGrant::network("scoped".to_string()),
                ]))
                .realisation(FulfilmentRealisation::DockerPerVessel {
                    image: Some(flotilla_resources::PlacementImageSource::Literal("crew:latest".into())),
                })
                .build(),
        )
        .await
        .expect("kind create");
}

use chrono::Duration as ChronoDuration;
use flotilla_charter::ops_entry::{MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION};
use flotilla_protocol::PrincipalRef;
use flotilla_resources::{ConvoySpec, FulfilmentKind};
use flotilla_store::apply_status_patch as apply_resource_status_patch;
use sha2::{Digest, Sha256};

use flotilla_admission::convoy_admission::refuse_live_convoy_in;
