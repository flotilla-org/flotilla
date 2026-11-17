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
        source: Some("project-map".into()),
        path: Some("forges/lab.yaml".into()),
        revision: revision.into(),
        charter: None,
        materialized_project: None,
    }
}

/// Whether `read` says it is the manifest root's at `revision`. The root and revision are what both
/// stores hold of the origin; on the engine the source and the document's path are read from the
/// ManifestRoot record. The engine runs hold that root, because the engine refuses a stamp naming
/// a root it does not hold; the old store holds none.
fn assert_manifest(read: &flotilla_resources::ResourceObject<Forge>, revision: &str) {
    let found = origin(&read.metadata.labels, &read.metadata.annotations);
    let found = found.as_ref().map(|origin| match origin {
        DefinitionOrigin::Manifest { root, revision, .. } => Some((root.as_str(), revision.as_str())),
        _ => None,
    });
    if let Ok(found) = found {
        assert_eq!(found, Some(("manifest-0011223344556677", revision)), "{:?}", read.metadata);
    } else {
        // Without the root's source the stamps name no full origin; the root and revision remain.
        assert_eq!(read.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("manifest"));
        assert_eq!(read.metadata.annotations[MANIFEST_RECONCILER_ROOT_ANNOTATION], "manifest-0011223344556677");
        assert_eq!(read.metadata.annotations[MANIFEST_REVISION_ANNOTATION], revision);
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
    assert_manifest(&read, "r1");
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
    assert_manifest(&read, "r2");

    // A deleted definition is gone from every read.
    forges.delete("ops").await.expect("delete");
    assert!(forges.get("ops").await.is_err());
    assert_eq!(forges.list().await.expect("list").len(), 1);

    // The built-in templates are served from the code, whichever store holds the others.
    let templates = backend.definitions::<WorkflowTemplate>(NS).list().await.expect("templates");
    assert!(templates.iter().any(|template| flotilla_resources::builtins::is_builtin("WorkflowTemplate", &template.metadata.name)));
    // A selection runs over stored definitions only: it never selects a built-in, even by the
    // built-ins' own label (#3824 layer 2a).
    let builtin_label = BTreeMap::from([(MANAGED_BY_LABEL.to_owned(), flotilla_resources::builtins::BUILTIN_MANAGER.to_owned())]);
    let selected = backend
        .definitions::<WorkflowTemplate>(NS)
        .list_selected(StoreQuery::labels(&builtin_label, true, false))
        .await
        .expect("select built-ins");
    assert!(selected.is_empty(), "{:?}", selected.iter().map(|template| &template.metadata.name).collect::<Vec<_>>());
}

#[tokio::test]
async fn the_old_store_keeps_the_definition_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

async fn engine_backend(directory: &tempfile::TempDir) -> ResourceBackend {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    // The kinds a definition's change items name.
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    hold_root(&service).await;
    let records = EngineDefinitions::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    backend
}

/// Declares the live root [`manifest`] stamps, at this home with its source.
async fn hold_root(service: &Arc<Service>) {
    use flotilla_provisioning::engine::manifest_root::EngineManifestRoots;
    use flotilla_resources::manifest_roots::{ManifestRoots, RootDeclaration};

    let roots = EngineManifestRoots::open(Arc::clone(service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open roots");
    let declaration =
        RootDeclaration { host: "alpha".into(), path: "/manifests".into(), source: "project-map".into(), binding: None, ops: false };
    roots.declare(NS, "manifest-0011223344556677", declaration).await.expect("declare");
}

// Behaviour (ADR 0053, ADR 0047): a previous generation's retired image layer still applies on the
// engine, so its manifest is not refused during the roll, and it is inert: nothing reads it, and a
// current layer of that name it replaces is removed, as the import drops retired layers.
#[tokio::test]
async fn on_the_engine_a_retired_layer_applies_and_holds_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let backend = engine_backend(&directory).await;
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
    let backend = engine_backend(&directory).await;
    contract(&backend).await;
    // Nothing reached the old store.
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<Forge>(), NS).await.expect("old rows").items.is_empty());
}

// Behaviour (#4024): a definition row holds only its fields; why it was written is its change. A
// root's documents at one revision are one change however many writes carry them, an unchanged
// document declared again at that revision writes nothing, and at a new revision it is written
// again under the new change, which names the root at that revision.
#[tokio::test]
async fn on_the_engine_one_root_revision_is_one_change() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    // The kinds a definition's change items name.
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    hold_root(&service).await;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(
        EngineDefinitions::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open"),
    ));
    let forges = backend.definitions::<Forge>(NS);
    let last_change = |name: &str| {
        service
            .with_store(|store| {
                let mut read = store.read()?;
                let id = read.resolve(
                    <flotilla_provisioning::stored::definition::ForgeDefinition as flotilla_schema::Kind>::CODE,
                    name,
                    "alpha",
                )?;
                read.last_change(id.expect("stored"))
            })
            .expect("read")
            .expect("a change")
    };
    for name in ["lab", "lab2", "lab3"] {
        forges.apply(&meta(name, &manifest("r1")), &forge(name)).await.expect("apply");
    }
    let first = last_change("lab");
    assert_eq!(first.writer, "manifest-reconciler");
    assert_eq!(first.items.len(), 1);
    assert_eq!((first.items[0].record.name.as_str(), first.items[0].revision.as_deref()), ("manifest-0011223344556677", Some("r1")));
    assert!(["lab2", "lab3"].iter().all(|name| last_change(name).seq == first.seq), "one change for the root's pass");

    let version = forges.get("lab").await.expect("get").metadata.resource_version;
    forges.apply(&meta("lab", &manifest("r1")), &forge("lab")).await.expect("again");
    assert_eq!(forges.get("lab").await.expect("get").metadata.resource_version, version, "the same reason writes nothing");

    forges.apply(&meta("lab", &manifest("r2")), &forge("lab")).await.expect("next revision");
    let next = last_change("lab");
    assert_ne!(next.seq, first.seq);
    assert_eq!(next.items[0].revision.as_deref(), Some("r2"));
    assert_manifest(&forges.get("lab").await.expect("get"), "r2");
}

// Behaviour (#4024, ADR 0064): a definition's manifest source is read from the root its change
// names, by the name the root was declared under. A root declared again after it retired is that
// name's next generation, so the source read is the live generation's, not the first's.
#[tokio::test]
async fn a_manifest_definition_reads_the_source_of_its_roots_live_generation() {
    use flotilla_provisioning::engine::manifest_root::EngineManifestRoots;
    use flotilla_resources::{
        definition_provenance::MANIFEST_SOURCE_ANNOTATION,
        manifest_roots::{ManifestRoots, RootDeclaration},
    };

    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let clock = Arc::new(SystemClock) as Arc<dyn Clock>;
    let roots = EngineManifestRoots::open(Arc::clone(&service), Arc::clone(&clock)).expect("open roots");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(EngineDefinitions::open(service, clock).expect("open")));

    // An ops root whose source moves retires and opens the next generation: the declared name,
    // then `.1`, then `.2`.
    let root = "manifest-0011223344556677";
    for source in ["first-map", "second-map", "third-map"] {
        let declaration =
            RootDeclaration { host: "feta".into(), path: "/manifests".into(), source: source.into(), binding: None, ops: true };
        roots.declare(NS, root, declaration).await.expect("declare");
    }
    assert_eq!(roots.root(NS, root).await.expect("read").expect("live").spec.source, "third-map");

    let forges = backend.definitions::<Forge>(NS);
    forges.apply(&meta("lab", &manifest("r1")), &forge("lab")).await.expect("apply");
    let read = forges.get("lab").await.expect("get");
    assert_eq!(read.metadata.annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str), Some("third-map"));
}

// Behaviour (ADR 0047): a previous-generation stamp named the declaring host, not its root. An
// apply carrying one is written under the one live root here with that host and source, and reads
// back with that root's name; a host no live root here matches with that source, or several do,
// is refused, each with its own reason.
#[tokio::test]
async fn an_apply_stamped_with_a_host_names_the_root_that_host_declares() {
    use flotilla_provisioning::engine::manifest_root::EngineManifestRoots;
    use flotilla_resources::manifest_roots::{ManifestRoots, RootDeclaration};

    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let clock = Arc::new(SystemClock) as Arc<dyn Clock>;
    let roots = EngineManifestRoots::open(Arc::clone(&service), Arc::clone(&clock)).expect("open roots");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(EngineDefinitions::open(service, clock).expect("open")));
    for (name, path, source) in [
        ("manifest-0011223344556677", "/manifests", "project-map"),
        ("manifest-8899aabbccddeeff", "/first", "twice"),
        ("manifest-0123456789abcdef", "/second", "twice"),
    ] {
        let declaration = RootDeclaration { host: "feta".into(), path: path.into(), source: source.into(), binding: None, ops: false };
        roots.declare(NS, name, declaration).await.expect("declare");
    }
    let legacy = |source: &str| DefinitionOrigin::Manifest {
        root: "feta".into(),
        source: Some(source.into()),
        path: Some("forges/lab.yaml".into()),
        revision: "r1".into(),
        charter: None,
        materialized_project: None,
    };

    let forges = backend.definitions::<Forge>(NS);
    forges.apply(&meta("lab", &legacy("project-map")), &forge("lab")).await.expect("apply");
    assert_manifest(&forges.get("lab").await.expect("get"), "r1");
    let unmatched = forges.apply(&meta("other", &legacy("elsewhere")), &forge("other")).await.expect_err("no root matches");
    assert!(unmatched.to_string().contains("no live root at its home declares host `feta`"), "{unmatched}");
    let doubled = forges.apply(&meta("other", &legacy("twice")), &forge("other")).await.expect_err("two roots match");
    assert!(doubled.to_string().contains("2 live roots at its home declare host `feta`"), "{doubled}");
}

// Behaviour (#3108): manifest stamps belong to the manifest reconciler at the root's home. An
// apply stamped with a root only another home holds is refused, naming the root and the home it
// is declared at; at the root's home the reconciler's write passes, and so does a re-apply of the
// document it reads back.
#[tokio::test]
async fn an_apply_stamped_with_a_root_another_home_holds_is_refused() {
    use flotilla_provisioning::engine::manifest_root::EngineManifestRoots;
    use flotilla_resources::{
        manifest_roots::{ManifestRoots, PassReport, RootDeclaration},
        DocumentKey, DocumentPhase, DocumentState,
    };

    let directory = tempfile::tempdir().expect("temp dir");
    let open = |home: &str| {
        let mut kinds = flotilla_schema::KindRegistry::new();
        flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
        flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
        flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
        flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
        let service = Arc::new(Service::open(&directory.path().join(format!("{home}.sqlite")), home, kinds).expect("store"));
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend.install_engine_records(Arc::new(
            EngineDefinitions::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open"),
        ));
        (service, backend)
    };
    let (alpha, alpha_backend) = open("alpha");
    let (beta, beta_backend) = open("beta");
    let roots = EngineManifestRoots::open(Arc::clone(&beta), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open roots");
    let declaration =
        RootDeclaration { host: "beta".into(), path: "/manifests".into(), source: "project-map".into(), binding: None, ops: false };
    roots.declare(NS, "manifest-0011223344556677", declaration).await.expect("declare");
    flotilla_provisioning_testkit::replicate_home(&beta, "beta", &alpha);

    let refused =
        alpha_backend.definitions::<Forge>(NS).apply(&meta("lab", &manifest("r1")), &forge("lab")).await.expect_err("not this home's root");
    assert!(
        refused.to_string().contains(
            "names root `manifest-0011223344556677`, which this home does not hold: the record is declared at the root's home (beta)"
        ),
        "{refused}"
    );

    let forges = beta_backend.definitions::<Forge>(NS);
    forges.apply(&meta("lab", &manifest("r1")), &forge("lab")).await.expect("the reconciler's write");
    // The reconciler then records its pass, whose baseline gives the document's path back.
    let key = DocumentKey { path: "forges/lab.yaml".into(), kind: "Forge".into(), namespace: NS.into(), name: "lab".into() };
    let state = DocumentState {
        phase: DocumentPhase::Applied,
        reason: None,
        live_hash: Some("h".into()),
        desired_hash: Some("h".into()),
        baseline_hash: Some("h".into()),
        observed_at: chrono::Utc::now(),
        resolved_token: None,
        resolution_outcome: None,
        removed_in: None,
    };
    let pass = PassReport { revision: "r1".into(), complete: true, documents: [(key, state)].into(), at: chrono::Utc::now() };
    roots.record_pass(NS, "manifest-0011223344556677", pass).await.expect("record the pass");
    let read = forges.get("lab").await.expect("get");
    forges.apply(&InputMeta::from(&read.metadata), &read.spec).await.expect("a re-apply of the read document");
    assert_manifest(&forges.get("lab").await.expect("get"), "r1");
}

// Behaviour (#3108): a definition is written at its home. Another home reads it as that home's,
// by name and in a listing, and refuses an apply or delete of it there, naming the home, instead
// of writing a copy of its own or finding nothing to delete; its home writes it.
#[tokio::test]
async fn another_homes_definition_reads_as_that_homes_and_is_written_only_there() {
    use flotilla_resources::{ResourceError, ResourceProvenance};
    use flotilla_store::{apply_resource_document, delete_resource_kind};

    let directory = tempfile::tempdir().expect("temp dir");
    let open = |home: &str| {
        let mut kinds = flotilla_schema::KindRegistry::new();
        flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
        flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
        flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
        flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
        let service = Arc::new(Service::open(&directory.path().join(format!("{home}.sqlite")), home, kinds).expect("store"));
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend.install_engine_records(Arc::new(
            EngineDefinitions::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("open"),
        ));
        (service, backend)
    };
    let (alpha, alpha_backend) = open("alpha");
    let (beta, beta_backend) = open("beta");
    let document = |repository: &str| {
        serde_json::json!({
            "kind": "WorkflowTemplate",
            "metadata": {"name": "homed"},
            "spec": {"vessels": [], "repository_refs": [repository]},
        })
    };
    apply_resource_document(&beta_backend, NS, document("first")).await.expect("declared at its home");
    flotilla_provisioning_testkit::replicate_home(&beta, "beta", &alpha);

    let beta_root = flotilla_protocol::NodeId::new("beta");
    let read = alpha_backend.including_replicas::<WorkflowTemplate>(NS).get("homed").await.expect("read here");
    assert!(matches!(&read.provenance, ResourceProvenance::Replica { origin_root, .. } if *origin_root == beta_root), "{read:?}");
    let listed = alpha_backend.including_replicas::<WorkflowTemplate>(NS).list().await.expect("list here");
    let homed = listed.items.iter().find(|item| item.object.metadata.name == "homed").expect("listed");
    assert!(matches!(&homed.provenance, ResourceProvenance::Replica { origin_root, .. } if *origin_root == beta_root));

    let refused = apply_resource_document(&alpha_backend, NS, document("second")).await.expect_err("not this home's to write");
    assert!(refused.to_string().contains("WorkflowTemplate homed is homed at `beta`; write it there"), "{refused}");
    let refused = delete_resource_kind(&alpha_backend, NS, "workflowtemplates", "homed").await.expect_err("not this home's to delete");
    assert!(refused.to_string().contains("WorkflowTemplate homed is homed at `beta`; delete it there"), "{refused}");
    assert!(matches!(alpha_backend.using::<WorkflowTemplate>(NS).get("homed").await, Err(ResourceError::NotFound { .. })));

    apply_resource_document(&beta_backend, NS, document("second")).await.expect("its home edits it");
    let edited = beta_backend.definitions::<WorkflowTemplate>(NS).get("homed").await.expect("home's copy");
    assert_eq!(edited.spec.repository_refs, Some(vec![flotilla_resources::RepositoryKey("second".into())]));
    delete_resource_kind(&beta_backend, NS, "workflowtemplates", "homed").await.expect("its home deletes it");
}

// Behaviour (#3108 item B): every watcher of the definitions on a daemon is woken by one shared
// follower. Each watcher gets the tick for a write after it subscribed, and once every watcher has
// gone a new one is still woken.
#[tokio::test]
async fn watchers_of_the_definitions_share_one_follower_and_each_hears_a_write() {
    use flotilla_resources::engine_records::KindRecords;

    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::definition::register(&mut kinds).expect("register");
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register roots");
    flotilla_provisioning::stored::repository::register(&mut kinds).expect("register repositories");
    flotilla_provisioning::stored::project::register(&mut kinds).expect("register projects");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let records = Arc::new(EngineDefinitions::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::clone(&records));
    let forges = backend.definitions::<Forge>(NS);
    async fn tick(stream: &mut futures::stream::BoxStream<'static, ()>) {
        tokio::time::timeout(Duration::from_secs(10), stream.next()).await.expect("a tick").expect("the stream stays open")
    }
    let changes = || KindRecords::<Forge>::changes(&*records);

    let mut first = changes().await.expect("first watcher");
    let mut second = changes().await.expect("second watcher");
    forges.apply(&meta("lab", &DefinitionOrigin::Operator), &forge("lab")).await.expect("apply");
    tick(&mut first).await;
    tick(&mut second).await;

    drop((first, second));
    let mut later = changes().await.expect("a watcher after the others went");
    forges.apply(&meta("lab2", &DefinitionOrigin::Operator), &forge("lab2")).await.expect("apply");
    tick(&mut later).await;
}
