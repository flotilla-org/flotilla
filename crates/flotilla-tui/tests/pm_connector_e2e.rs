//! #2523: the real PM connector consumes the daemon's resource-watch commands
//! and Aggregator queries. No watch-accepting daemon mock is involved.
#![cfg(unix)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_manifest::{
    entity,
    recipe::FlotillaRecipes,
    sink::{PatchSink, WheelhouseHttpSink},
    wire::{MetadataPatch, MetadataTarget, MetadataValue},
};
use flotilla_protocol::{HostName, IssueSource, NodeId, Relationship, Subject, SubjectKind};
use flotilla_resources::{
    change_request_record_name, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus,
    ChangeRequestSubjectHistory, Convoy, ConvoyEnsure, ConvoyEnsureSpec, ConvoyEnsureStatus, ConvoyPhase, ConvoySpec, ConvoyStatus,
    DeclaredSubject, InMemoryBackend, InputMeta, Observation, ObservedChangeRequestState, Project, ProjectSpec, ResourceBackend,
    ResourceObject, WatchEvent, ENSURED_FROM_ANNOTATION,
};
use flotilla_tui::pm_connect::Connector;

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
    change_request_record_name("github.com", "org/platform", 42)
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
async fn seed(backend: &ResourceBackend, namespace: &str) -> ResourceObject<ChangeRequest> {
    backend
        .using::<Project>(namespace)
        .create(&meta("platform"), &ProjectSpec::builder().display_name("Platform".into()).default_workflow_ref("dev".into()).build())
        .await
        .expect("project");
    let mut convoy_meta = meta("attempt-1");
    convoy_meta.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "governor".into());
    let convoy = backend
        .using::<Convoy>(namespace)
        .create(
            &convoy_meta,
            &ConvoySpec::builder()
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
        .await
        .expect("convoy");
    backend
        .using::<Convoy>(namespace)
        .update_status("attempt-1", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .expect("active convoy");
    let ensure = backend
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
    let ensure = backend.using::<ConvoyEnsure>(namespace).get(&ensure.metadata.name).await.expect("local ensure version");
    backend
        .using::<ConvoyEnsure>(namespace)
        .update_status(
            "governor",
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus { convoy_ref: Some("attempt-1".into()), ..Default::default() },
        )
        .await
        .expect("current attempt");
    let cr = backend
        .using::<ChangeRequest>(namespace)
        .create(
            &meta(&cr_name()),
            &ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("org/platform".into())
                .number(42)
                .observing_authority("remote".into())
                .subject_of(vec![ChangeRequestSubjectHistory {
                    namespace: namespace.into(),
                    convoy: "attempt-1".into(),
                    origin: "local".into(),
                    relationship: Relationship::Produces,
                    role: "governor".into(),
                    project: Some("platform".into()),
                    last_seen: chrono::Utc::now(),
                }])
                .build(),
        )
        .await
        .expect("subject");
    backend
        .using::<ChangeRequest>(namespace)
        .update_status(&cr_name(), &cr.metadata.resource_version, &status("initial", 1))
        .await
        .expect("subject status")
}

// #2523 acceptance: local and replica subjects retain project/role edges and
// opening recipes across bootstrap, updates, deletions and reconnect.
async fn lifecycle(forward: Option<WheelhouseHttpSink>) {
    let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
    let tmp = tempfile::tempdir().expect("config");
    std::fs::write(tmp.path().join("daemon.toml"), "machine_id = \"pm-e2e-2523\"\n").expect("config");
    let config = Arc::new(ConfigStore::with_base(tmp.path()));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let remote = seed(&source, "flotilla").await;
    // Only the observation is replicated; the runtime graph is local.
    let local = seed(&backend, "flotilla").await;
    backend
        .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
        .apply(WatchEvent::Added(remote.clone()), chrono::Utc::now())
        .await
        .expect("replica");
    let daemon =
        InProcessDaemon::new_with_resource_backend(vec![], config.clone(), fake_discovery(false), HostName::new("local"), backend.clone())
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
    let mut remote = remote;
    for time in 2..=5 {
        remote.status = Some(status(&format!("replica-{time}"), time));
        backend
            .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
            .apply(WatchEvent::Modified(remote.clone()), chrono::Utc::now())
            .await
            .expect("replica update");
    }
    // A second attempt updates the stable role without rebuilding the connection.
    let previous = backend.using::<Convoy>("flotilla").get("attempt-1").await.expect("previous attempt");
    let mut second_meta = meta("attempt-2");
    second_meta.annotations.insert(ENSURED_FROM_ANNOTATION.into(), "governor".into());
    let mut second_spec = previous.spec;
    second_spec.generation = 2;
    let second = backend.using::<Convoy>("flotilla").create(&second_meta, &second_spec).await.expect("second attempt");
    backend
        .using::<Convoy>("flotilla")
        .update_status("attempt-2", &second.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .expect("second active attempt");
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("governor").await.expect("standing role version");
    backend
        .using::<ConvoyEnsure>("flotilla")
        .update_status(
            "governor",
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus { convoy_ref: Some("attempt-2".into()), ..Default::default() },
        )
        .await
        .expect("new current attempt");
    wait("new current attempt", || {
        sink.value(&role, "flotilla.role.current_attempt")
            == Some(MetadataValue::EntityRefs(vec![entity::convoy("flotilla", "attempt-2", "local")]))
    })
    .await;
    wait(
        "both attempt edges",
        || matches!(sink.value(&role, "flotilla.role.attempts"), Some(MetadataValue::EntityRefs(attempts)) if attempts.len() == 2),
    )
    .await;
    wait("queued replica updates", || sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("replica-5"))).await;
    backend
        .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
        .apply(WatchEvent::Deleted(remote.clone()), chrono::Utc::now())
        .await
        .expect("replica deletion");
    wait("local source survives replica removal", || {
        sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("initial"))
    })
    .await;
    backend.using::<ChangeRequest>("flotilla").delete(&local.metadata.name).await.expect("local deletion");
    wait("last source removal", || sink.value(&target, "flotilla.change_request.title").is_none()).await;
    // Re-add a source, then delete it while disconnected below.
    remote.status = Some(status("reconnect-source", 6));
    backend
        .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
        .apply(WatchEvent::Added(remote.clone()), chrono::Utc::now())
        .await
        .expect("reconnect source");
    wait("source restored", || sink.value(&target, "flotilla.change_request.title") == Some(MetadataValue::text("reconnect-source"))).await;
    // Stay alive beyond several of the reported ~2-second churn periods.
    tokio::select! { result = &mut connector => panic!("connector exited: {result:?}"), _ = tokio::time::sleep(Duration::from_secs(7)) => {} }
    connector.abort();
    let _ = connector.await;
    wait("query cleanup on cancellation", || query_state.subscribed_queries() == baseline_queries).await;
    backend
        .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
        .apply(WatchEvent::Deleted(remote), chrono::Utc::now())
        .await
        .expect("offline deletion");
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
