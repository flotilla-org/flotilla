//! Project's reads and writes through the store's typed paths (#3824 layer 3b): one contract, run
//! against the old store and against the engine's records installed behind `[primitives]`, so
//! Project's writers (declarations, project operations' conditions, the dispatch reconciler's
//! queue) and readers keep their behaviour on either.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use flotilla_provisioning::engine::projects::EngineProjects;
use flotilla_resources::{
    definition_provenance::{BOOTSTRAP_COMMIT_ANNOTATION, BOOTSTRAP_REPOSITORY_ANNOTATION, DECLARATION_FILE_ANNOTATION},
    Clock, DeclarationRefusedCondition, DispatchQueueEntry, InputMeta, Project, ProjectRepositorySpec, ProjectSpec, ProjectStatusPatch,
    RepositoryKey, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{apply_status_patch, watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";
const REPOSITORY: &str = "github.com/acme/widgets";

fn spec(name: &str, parent: Option<&str>) -> ProjectSpec {
    ProjectSpec::builder()
        .display_name(name.into())
        .maybe_parent(parent.map(Into::into))
        .repositories(vec![ProjectRepositorySpec {
            charter_store: None,
            repo: RepositoryKey(REPOSITORY.into()),
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }])
        .build()
}

/// A time the engine's microseconds hold exactly.
fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("time")
}

fn entry(issue: &str) -> DispatchQueueEntry {
    serde_json::from_value(serde_json::json!({
        "issue": {"source": {"service": "github", "scope": "acme/widgets"}, "id": issue},
        "title": "ready", "issue_as_of": at(1), "ready_observed_at": at(2), "observed_at": at(3), "provenance": "test",
    }))
    .expect("entry")
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend) {
    let definitions = backend.definitions::<Project>(NS);
    let projects = backend.using::<Project>(NS);
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "Project").await.expect("watch");

    // A declared Project keeps the provenance of its declaration, and its parent.
    definitions.apply(&InputMeta::builder().name("acme".into()).build(), &spec("acme", None)).await.expect("parent");
    let stamps = BTreeMap::from([
        (BOOTSTRAP_REPOSITORY_ANNOTATION.to_owned(), REPOSITORY.to_owned()),
        (BOOTSTRAP_COMMIT_ANNOTATION.to_owned(), "abc".to_owned()),
        (DECLARATION_FILE_ANNOTATION.to_owned(), ".flotilla/project.yaml".to_owned()),
    ]);
    let meta = InputMeta::builder().name("widgets".into()).annotations(stamps.clone()).build();
    definitions.apply(&meta, &spec("widgets", Some("acme"))).await.expect("declare");
    let read = definitions.get("widgets").await.expect("read");
    assert_eq!(read.spec.parent.as_deref(), Some("acme"));
    assert_eq!(read.metadata.annotations, stamps);
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");

    // Attach finds a Project by its repository.
    let query = StoreQuery { fields: BTreeMap::from([("spec.repositories[].repo".into(), REPOSITORY.into())]), ..Default::default() };
    let mut found: Vec<String> =
        definitions.query_declared(&query).await.expect("query").into_iter().map(|project| project.metadata.name).collect();
    found.sort();
    assert_eq!(found, ["acme", "widgets"]);

    // Project operations' conditions and the dispatch reconciler's queue are its status, and a
    // watcher sees a change of either alone.
    let refused =
        DeclarationRefusedCondition { entry_path: "ops.yaml".into(), message: "duplicate".into(), since: at(4), observed_at: at(5) };
    let operations = [
        ProjectStatusPatch::ReplaceOperationalEntries { ready: false, message: "duplicate".into() },
        ProjectStatusPatch::DeclarationRefused { condition: Some(refused.clone()) },
    ];
    let queue = [
        ProjectStatusPatch::ReplaceDispatchQueue { queue: vec![entry("7")], attention: None },
        ProjectStatusPatch::DispatchQueueError { message: Some("forge unavailable".into()) },
    ];
    for patches in [&operations[..], &queue[..]] {
        while tokio::time::timeout(Duration::from_millis(1200), watch.stream.next()).await.is_ok() {}
        for patch in patches {
            apply_status_patch(&projects, "widgets", patch).await.expect("status");
        }
        tokio::time::timeout(Duration::from_secs(10), watch.stream.next())
            .await
            .expect("a status change is seen")
            .expect("open")
            .expect("event");
    }
    let status = projects.get("widgets").await.expect("read").status.expect("status");
    assert_eq!(status.declaration_refused, Some(refused));
    assert_eq!(status.operational_entries.map(|entries| (entries.ready, entries.message)), Some((false, "duplicate".into())));
    assert_eq!(status.dispatch_queue, [entry("7")]);
    assert_eq!(status.dispatch_queue_error.as_deref(), Some("forge unavailable"));

    // A spec write keeps the status.
    let current = definitions.get("widgets").await.expect("read");
    let mut edited = current.spec.clone();
    edited.display_name = "Widgets".into();
    definitions.apply(&meta, &edited).await.expect("edit");
    let read = projects.get("widgets").await.expect("read");
    assert_eq!(read.spec.display_name, "Widgets");
    assert_eq!(read.status.expect("status").dispatch_queue, [entry("7")], "a spec write keeps the status");

    // A status write at a version that moved on is refused.
    let stale = current.metadata.resource_version;
    assert!(projects.update_status("widgets", &stale, &Default::default()).await.is_err(), "stale version");

    // The parent of another Project cannot be deleted; a leaf can, and is gone from every read.
    assert!(definitions.delete("acme").await.is_err(), "a parent stays");
    definitions.delete("widgets").await.expect("delete");
    assert!(definitions.get("widgets").await.is_err());
    assert!(backend.including_replicas::<Project>(NS).get("widgets").await.is_err());
}

#[tokio::test]
async fn the_old_store_keeps_the_project_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

fn engine(directory: &std::path::Path) -> (Arc<Service>, ResourceBackend) {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineProjects::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    (service, backend)
}

#[tokio::test]
async fn the_engine_keeps_the_project_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_, backend) = engine(directory.path());
    contract(&backend).await;
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Project>(), NS).await.expect("old rows").items.is_empty());
}

// Behaviour (#3824 layer 3b, the 2026-10-26 ruling): the dispatch queue is a judgement, not stored.
// Reopened over the same store, a Project keeps project operations' record and recomputes the queue.
#[tokio::test]
async fn the_engine_stores_project_operations_but_not_the_queue() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (service, backend) = engine(directory.path());
    let projects = backend.using::<Project>(NS);
    backend
        .definitions::<Project>(NS)
        .apply(&InputMeta::builder().name("widgets".into()).build(), &spec("widgets", None))
        .await
        .expect("declare");
    for patch in [
        ProjectStatusPatch::ReplaceOperationalEntries { ready: true, message: "ready".into() },
        ProjectStatusPatch::ReplaceDispatchQueue { queue: vec![entry("7")], attention: None },
    ] {
        apply_status_patch(&projects, "widgets", &patch).await.expect("status");
    }
    let reopened = ResourceBackend::new(InMemoryBackend::default());
    reopened.install_engine_records(Arc::new(EngineProjects::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("reopen")));
    let status = reopened.using::<Project>(NS).get("widgets").await.expect("read").status.expect("status");
    assert_eq!(status.operational_entries.map(|entries| entries.ready), Some(true));
    assert!(status.dispatch_queue.is_empty());
}
