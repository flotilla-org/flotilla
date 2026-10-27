//! The definition kinds' reads and writes through the store's definition paths (#3824 layer
//! 2b-ii): one contract, run against the old store and against the engine's records installed
//! behind `[primitives]`, so the readers and writers that go through those paths keep their
//! behaviour on either.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_provisioning::engine::definitions::EngineDefinitions;
use flotilla_resources::{
    definition_provenance::{MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION},
    definitions::{origin, stamp, DefinitionOrigin},
    Clock, Forge, ForgeKind, ForgeSpec, InputMeta, SystemClock, WorkflowTemplate, MANAGED_BY_LABEL,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn forge(id: &str) -> ForgeSpec {
    ForgeSpec {
        forge_id: id.into(),
        kind: ForgeKind::Forgejo,
        hosts: [format!("{id}.example")].into(),
        https_url: format!("https://{id}.example"),
        git_ssh_host: format!("{id}.example"),
    }
}

fn manifest(revision: &str) -> DefinitionOrigin {
    DefinitionOrigin::Manifest {
        root: "manifest-0011223344556677".into(),
        source: "project-map".into(),
        path: "forges/lab.yaml".into(),
        revision: revision.into(),
        charter: None,
        materialized_project: None,
    }
}

fn meta(name: &str, origin: &DefinitionOrigin) -> InputMeta {
    let (labels, annotations) = stamp(origin);
    InputMeta::builder().name(name.to_owned()).labels(labels).annotations(annotations).build()
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend) {
    let forges = backend.definitions::<Forge>(NS);

    // A watcher subscribed before a write sees it.
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "Forge").await.expect("watch forges");

    // A definition written with an origin reads back with its spec, and its origin from its stamps.
    forges.apply(&meta("lab", &manifest("r1")), &forge("lab")).await.expect("apply");
    let read = forges.get("lab").await.expect("get");
    assert_eq!(read.spec, forge("lab"));
    assert_eq!(origin(&read.metadata.labels, &read.metadata.annotations), Ok(manifest("r1")));
    assert_eq!(read.metadata.annotations[MANIFEST_RECONCILER_ROOT_ANNOTATION], "manifest-0011223344556677");
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");

    // An operator's definition sits beside it; a list holds both, and a selection by the
    // manifest's stamp only the manifest's.
    forges.apply(&meta("ops", &DefinitionOrigin::Operator), &forge("ops")).await.expect("apply operator's");
    let mut names: Vec<_> = forges.list().await.expect("list").into_iter().map(|object| object.metadata.name).collect();
    names.sort();
    assert_eq!(names, ["lab", "ops"]);
    let selected = forges
        .list_selected(StoreQuery::labels(&BTreeMap::from([(MANAGED_BY_LABEL.to_owned(), "manifest".to_owned())]), true, false))
        .await
        .expect("select");
    assert_eq!(selected.iter().map(|object| object.metadata.name.as_str()).collect::<Vec<_>>(), ["lab"]);
    assert_eq!(forges.list_names(["ops".to_owned()]).await.expect("by name").len(), 1);

    // This home's own definitions are its local rows, and the replica reads see the same.
    assert_eq!(backend.using::<Forge>(NS).get("lab").await.expect("local").spec, forge("lab"));
    assert_eq!(backend.including_replicas::<Forge>(NS).list().await.expect("replicas").items.len(), 2);

    // A new revision of the same document moves its origin; the spec stays.
    let mut revised = meta("lab", &manifest("r2"));
    revised.annotations.insert(MANIFEST_REVISION_ANNOTATION.into(), "r2".into());
    forges.update_metadata(&revised).await.expect("revise");
    let read = forges.get("lab").await.expect("get revised");
    assert_eq!(origin(&read.metadata.labels, &read.metadata.annotations), Ok(manifest("r2")));

    // A deleted definition is gone from every read.
    forges.delete("ops").await.expect("delete");
    assert!(forges.get("ops").await.is_err());
    assert_eq!(forges.list().await.expect("list").len(), 1);

    // The built-in templates are served from the code, whichever store holds the others.
    let templates = backend.definitions::<WorkflowTemplate>(NS).list().await.expect("templates");
    assert!(templates.iter().any(|template| flotilla_resources::builtins::is_builtin("WorkflowTemplate", &template.metadata.name)));
}

#[tokio::test]
async fn the_old_store_keeps_the_definition_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

fn engine_backend(directory: &tempfile::TempDir) -> ResourceBackend {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = EngineDefinitions::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_definition_records(Arc::new(records));
    backend
}

// Behaviour (ADR 0053, ADR 0047): a previous generation's retired image layer still applies on the
// engine, so its manifest is not refused during the roll, and it is inert: nothing reads it, and a
// current layer of that name it replaces is removed, as the import drops retired layers.
#[tokio::test]
async fn on_the_engine_a_retired_layer_applies_and_holds_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let backend = engine_backend(&directory);
    let layers = backend.definitions::<flotilla_resources::ImageLayer>(NS);
    let builtin = flotilla_resources::builtins::builtin_definitions("ImageLayer").expect("built-ins")[0].spec.clone();
    let current: flotilla_resources::ImageLayerResourceSpec = serde_json::from_value(builtin).expect("layer");
    let name = "retiring-layer";
    layers.apply(&meta(name, &DefinitionOrigin::Operator), &current).await.expect("current layer");
    let retired: flotilla_resources::ImageLayerResourceSpec =
        serde_json::from_value(serde_json::json!({"retired": true})).expect("retired");
    layers.apply(&meta(name, &DefinitionOrigin::Operator), &retired).await.expect("a retired layer applies");
    assert!(layers.get(name).await.is_err(), "a retired layer is inert");
}

#[tokio::test]
async fn the_engine_keeps_the_definition_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let backend = engine_backend(&directory);
    contract(&backend).await;
    // Nothing reached the old store.
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Forge>(), NS).await.expect("old rows").items.is_empty());
}
