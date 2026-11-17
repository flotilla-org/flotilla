//! #2523: the real PM connector consumes the daemon's resource-watch commands
//! and Aggregator queries. No watch-accepting daemon mock is involved.
#![cfg(unix)]

use flotilla_store_memory::InMemoryBackend;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_cli::pm_connect::Connector;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_manifest::{
    entity,
    recipe::FlotillaRecipes,
    sink::{PatchSink, WheelhouseHttpSink},
    wire::{MetadataPatch, MetadataTarget, MetadataValue},
};
use flotilla_protocol::{HostName, IssueSource, Relationship, Subject, SubjectKind};
use flotilla_provisioning::engine::observations::EngineObservations;
use flotilla_provisioning_testkit::other_home::OtherHome;
use flotilla_resources::{
    engine_records::KindRecords, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, Convoy,
    ConvoyEnsure, ConvoyEnsureSpec, ConvoySpec, DeclaredSubject, HostSpec, HostStatus, InputMeta, IssueSourceBindingSpec, Observation,
    ObservedChangeRequestState, ProjectSpec, SystemClock, ENSURED_FROM_ANNOTATION,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;

#[derive(Default)]
struct Sink {
    patches: Mutex<Vec<MetadataPatch>>,
    forward: Option<WheelhouseHttpSink>,
}
#[async_trait]
impl PatchSink for Sink {
    async fn send(&self, patch: &MetadataPatch) -> Result<(), String> {
        if let Some(forward) = &self.forward {
            forward.send(patch).await?;
        }
        self.patches.lock().expect("patches").push(patch.clone());
        Ok(())
    }
}
impl Sink {
    fn value(&self, target: &MetadataTarget, key: &str) -> Option<MetadataValue> {
        self.patches
            .lock()
            .expect("patches")
            .iter()
            .rev()
            .filter(|patch| &patch.target == target)
            .find_map(|patch| {
                if let Some(value) = patch.set.get(key) {
                    Some(Some(value.value.clone()))
                } else if patch.unset.iter().any(|unset| unset == key) {
                    Some(None)
                } else {
                    None
                }
            })
            .flatten()
    }
}
async fn wait(label: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("catalog did not converge: {label}"));
}
fn cr_name() -> String {
    flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address("github.com", "org/platform", 42, &[]).expect("change request address"),
    )
    .expect("record name")
}
fn meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}
fn status(title: &str, time: i64) -> ChangeRequestStatus {
    let at = chrono::DateTime::from_timestamp(time, 0).expect("timestamp");
    ChangeRequestStatus {
        title: Observation::known(title.to_string(), at),
        author: Observation::default(),
        state: Observation::known(ObservedChangeRequestState::Open, at),
        head_sha: Observation::default(),
        checks: Observation::default(),
        review: ChangeRequestReviewObservation { actionable_at_head: Observation::default() },
        mergeable: Observation::default(),
        review_decision: Observation::default(),
        review_requested_from_owner: Observation::default(),
    }
}
/// Admits attempt `name` of the governor's standing role with `spec` at `daemon`'s engine, returning its launch.
async fn admit_attempt(daemon: &InProcessDaemon, name: &str, spec: ConvoySpec) -> String {
    let mut convoy =
        flotilla_engine_convoys_testkit::convoy("flotilla", name, spec, flotilla_engine_convoys_testkit::single_agent("governor"));
    convoy.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "governor".into());
    flotilla_daemon_testkit::engine::admit_convoy(daemon, convoy).await
}
async fn seed(daemon: &InProcessDaemon, backend: &ResourceBackend, namespace: &str) -> String {
    let launch = admit_attempt(
        daemon,
        "attempt-1",
        ConvoySpec::builder()
            .workflow_ref("dev".into())
            .role("governor".into())
            .generation(1)
            .project_ref("platform".into())
            .subjects(vec![DeclaredSubject {
                subject: Subject {
                    kind: SubjectKind::ChangeRequest,
                    source: IssueSource { service: "github.com".into(), scope: "org/platform".into() },
                    id: "42".into(),
                },
                relationship: Relationship::Produces,
                issue: None,
                change_request: None,
            }])
            .build(),
    )
    .await;
    // The launch is the role's current generation: its newest live convoy ensured from it.
    backend
        .using::<ConvoyEnsure>(namespace)
        .create(
            &meta("governor"),
            &ConvoyEnsureSpec::builder()
                .project_ref("platform".into())
                .role("governor".into())
                .workflow_ref("dev".into())
                .repositories(vec![])
                .build(),
        )
        .await
        .expect("standing role");
    launch
}

/// The forge observer of github.com org/platform: another daemon, whose log reaches the daemon
/// under test. It declared Project platform with that source, so it observes change request 42.
struct RemoteObserver {
    home: OtherHome,
    records: EngineObservations,
    daemon: Arc<Service>,
}
impl RemoteObserver {
    async fn open(daemon: Arc<Service>) -> Self {
        let home = OtherHome::open("remote");
        let heartbeat = HostStatus { heartbeat_at: Some(chrono::Utc::now()), ..Default::default() };
        home.host("remote", &HostSpec { display_name: "remote".into(), ..Default::default() }, Some(&heartbeat)).await;
        let source = IssueSource { service: "github.com".into(), scope: "org/platform".into() };
        let project = ProjectSpec::builder()
            .display_name("Platform".into())
            .default_workflow_ref("dev".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source).alias("platform".into()).build()])
            .build();
        home.project("platform", &Default::default(), &Default::default(), &project).await;
        home.replicate_to(&daemon);
        let records = EngineObservations::open(Arc::clone(home.service()), Arc::new(SystemClock)).expect("remote observations");
        Self { home, records, daemon }
    }
    async fn observe(&self, title: &str, time: i64) {
        let spec = ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/platform".into())
            .number(42)
            .observing_authority("remote".into())
            .build();
        let records: &dyn KindRecords<ChangeRequest> = &self.records;
        let written = records.apply(&cr_name(), None, &Default::default(), &Default::default(), &spec).await.expect("observed");
        records.set_status(&cr_name(), written.version, &status(title, time)).await.expect("observed status");
        self.home.replicate_to(&self.daemon);
    }
    async fn forget(&self) {
        KindRecords::<ChangeRequest>::delete(&self.records, &cr_name()).await.expect("forgotten");
        self.home.replicate_to(&self.daemon);
    }
}

// #2523 acceptance: an observed subject keeps its project/role edges and opening
// recipes across bootstrap, updates, deletions and reconnect. The observation is
// another daemon's, replicated in; the runtime graph is local.
async fn lifecycle(forward: Option<WheelhouseHttpSink>) {
    let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
    let tmp = tempfile::tempdir().expect("config");
    std::fs::write(tmp.path().join("daemon.toml"), "machine_id = \"pm-e2e-2523\"\n").expect("config");
    let config = Arc::new(ConfigStore::with_base(tmp.path()));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        config.clone(),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let runtime = DaemonRuntime::start_with_options(
        daemon.clone(),
        config,
        None,
        RuntimeOptions {
            start_controllers: false,
            heartbeat_interval: Duration::from_secs(300),
            controller_resync_interval: Duration::from_secs(300),
            ..Default::default()
        },
    )
    .await
    .expect("runtime");
    // The engine-served kinds are written once the runtime has installed the engine.
    let remote = RemoteObserver::open(daemon.decided_store().expect("decided store")).await;
    let first = seed(&daemon, &backend, "flotilla").await;
    remote.observe("initial", 1).await;
    let query_state = daemon.aggregator_projection_state().await;
    let baseline_queries = query_state.subscribed_queries();
    let sink = Arc::new(Sink { forward, ..Default::default() });
    let publication = Arc::new(Connector::default());
    let start = || {
        let publication = publication.clone();
        let daemon = daemon.clone();
        let sink = sink.clone();
        tokio::spawn(
            async move { publication.run(daemon, sink, Arc::new(FlotillaRecipes::new("flotilla")), Duration::from_secs(60)).await },
        )
    };
    let mut connector = start();
    let target = MetadataTarget::Entity(entity::change_request("github.com", "org/platform", "42"));
    wait("bootstrap observation", || sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("initial"))).await;
    wait("subject project edge", || sink.value(&target, "flotilla.project").is_some()).await;
    let project = MetadataTarget::Entity(entity::project("flotilla", "platform", "fleet"));
    wait("project opening recipe", || sink.value(&project, "action.primary.recipe").is_some()).await;
    assert_eq!(sink.value(&project, "action.primary.recipe"), Some(MetadataValue::text("'flotilla' view 'project/flotilla/platform'")));
    let role = MetadataTarget::Entity(entity::role("flotilla", "platform", "governor", "fleet"));
    wait("standing-role current attempt", || sink.value(&role, "flotilla.role.current_attempt").is_some()).await;
    assert!(sink.value(&role, "flotilla.role.attempts").is_some(), "attempt history published");
    // Queued writes happen without waiting for the consumer between them.
    for time in 2..=5 {
        remote.observe(&format!("replica-{time}"), time).await;
    }
    // A second attempt updates the stable role without rebuilding the connection.
    let previous = backend.using::<Convoy>("flotilla").get(&first).await.expect("previous attempt");
    let mut second_spec = previous.spec;
    second_spec.generation = 2;
    let second = admit_attempt(&daemon, "attempt-2", second_spec).await;
    wait("new current attempt", || {
        sink.value(&role, "flotilla.role.current_attempt")
            == Some(MetadataValue::EntityRefs(vec![entity::convoy("flotilla", &second, "local")]))
    })
    .await;
    wait(
        "both attempt edges",
        || matches!(sink.value(&role, "flotilla.role.attempts"), Some(MetadataValue::EntityRefs(attempts)) if attempts.len() == 2),
    )
    .await;
    wait("queued replica updates", || sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("replica-5"))).await;
    remote.forget().await;
    wait("source removal", || sink.value(&target, "flotilla.change_request.title").is_none()).await;
    // Re-add the source, then delete it while disconnected below.
    remote.observe("reconnect-source", 6).await;
    wait("source restored", || sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("reconnect-source"))).await;
    // Stay alive beyond several of the reported ~2-second churn periods.
    tokio::select! { result = &mut connector => panic!("connector exited: {result:?}"), _ = tokio::time::sleep(Duration::from_secs(7)) => {} }
    connector.abort();
    let _ = connector.await;
    wait("query cleanup on cancellation", || query_state.subscribed_queries() == baseline_queries).await;
    remote.forget().await;
    let restarted = start();
    wait("offline deletion retraction", || sink.value(&target, "flotilla.change_request.title").is_none()).await;
    // A fresh bootstrap must retract observations removed while disconnected.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!restarted.is_finished(), "reconnected watch remains healthy");
    restarted.abort();
    let _ = restarted.await;
    assert_eq!(query_state.subscribed_queries(), baseline_queries, "reconnect cleanup releases named query demand");
    runtime.shutdown();
}
#[tokio::test]
async fn connector_real_daemon_subject_lifecycle() {
    lifecycle(None).await;
}

// Run against a disposable current Wheelhouse ingress fixture, never production.
#[tokio::test]
#[ignore = "requires a disposable Wheelhouse ingress fixture"]
async fn connector_real_wheelhouse_ingress() {
    let socket = std::env::var("FLOTILLA_TEST_WHEELHOUSE_SOCKET").expect("isolated ingress socket");
    lifecycle(Some(WheelhouseHttpSink::new(socket))).await;
}
