use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{Path, State},
    http::Uri,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    Convoy, InputMeta, OwnerReference, ReadResourceList, ReadWatchEvent, ResourceError, ResourceList, ResourceProvenance, WatchStart,
    WatchStream,
};
use flotilla_store::{LocalViewSource, OwnerQuery, ResourceBackend, StoreQuery, StoreViewSource, ViewSource, WatchView};
use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use flotilla_store_testkit::fixtures::{convoy_meta, convoy_spec};
use futures::{stream::BoxStream, StreamExt};

// HTTP boundary stand-in enforces the actual query and resumable watch protocol
// against a real memory backend; it never substitutes an arbitrary response.
async fn endpoint(State(backend): State<ResourceBackend>, Path((namespace, kind)): Path<(String, String)>, uri: Uri) -> Response {
    let query: BTreeMap<String, String> = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect();
    if let Some(query) = query.get("storeQuery") {
        let query: StoreQuery = serde_json::from_str(query).unwrap();
        return Json(flotilla_store::query_resource_kind(&backend, &namespace, &kind, &query).await.unwrap()).into_response();
    }
    if query.is_empty() {
        return Json(flotilla_store::list_resource_kind(&backend, &namespace, &kind).await.unwrap().value).into_response();
    }
    assert_eq!(query.get("watch").map(String::as_str), Some("true"));
    let events = if query.get("replicaSources").map(String::as_str) == Some("true") {
        flotilla_store::watch_resource_kind_replica_sources(&backend, &namespace, &kind).await.unwrap()
    } else {
        let version = query.get("resourceVersion").expect("local watch must resume from snapshot").clone();
        let start = match query.get("generation") {
            Some(generation) => WatchStart::FromVersionInGeneration { generation: generation.clone(), resource_version: version },
            None => WatchStart::FromVersion(version),
        };
        flotilla_store::watch_resource_kind_from(&backend, &namespace, &kind, start).await.unwrap()
    };
    let body = events.stream.map(|event| {
        let value = match event {
            Ok(value) => value,
            Err(ResourceError::WatchExpired { .. }) => serde_json::json!({"type":"ERROR", "object":{"code":410,"message":"expired"}}),
            Err(error) => panic!("unexpected watch error: {error}"),
        };
        Ok::<_, std::io::Error>(format!("{value}\n"))
    });
    Body::from_stream(body).into_response()
}

struct GapSource {
    source: StoreViewSource<Convoy>,
    gap: Arc<AtomicBool>,
}
#[async_trait]
impl ViewSource<Convoy> for GapSource {
    async fn list_and_watch(
        &self,
    ) -> Result<(ReadResourceList<Convoy>, BoxStream<'static, Result<ReadWatchEvent<Convoy>, ResourceError>>), ResourceError> {
        let (listed, events) = self.source.list_and_watch().await?;
        let gap = self.gap.clone();
        Ok((
            listed,
            events
                .map(move |event| {
                    if gap.swap(false, Ordering::SeqCst) {
                        Err(ResourceError::WatchExpired { requested_version: "injected gap".into(), compacted_through: None })
                    } else {
                        event
                    }
                })
                .boxed(),
        ))
    }
}

struct RacingLocalSource {
    backend: ResourceBackend,
    writer: ResourceBackend,
}
#[async_trait]
impl LocalViewSource<Convoy> for RacingLocalSource {
    async fn list(&self) -> Result<ResourceList<Convoy>, ResourceError> {
        self.backend.using::<Convoy>("flotilla").list().await
    }
    async fn watch(&self, start: WatchStart) -> Result<WatchStream<Convoy>, ResourceError> {
        self.writer.using::<Convoy>("flotilla").create(&convoy_meta("racing"), &convoy_spec("template")).await?;
        self.backend.using::<Convoy>("flotilla").watch(start).await
    }
}

async fn contract(reader: ResourceBackend, writer: ResourceBackend) {
    let source = RacingLocalSource { backend: reader.clone(), writer: writer.clone() };
    let (listed, mut watch) = flotilla_store::recover_local_view(&source).await.unwrap();
    assert!(listed.items.is_empty());
    // A mutation between list and watch is replayed from the list's position.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next()).await.unwrap().unwrap().unwrap();
    assert!(matches!(event, flotilla_resources::WatchEvent::Added(object) if object.metadata.name == "racing"));
    drop(watch);
    writer.using::<Convoy>("flotilla").delete("racing").await.unwrap();

    let convoys = writer.using::<Convoy>("flotilla");
    let mut meta = convoy_meta("alpha");
    meta.labels.insert("team".into(), "old".into());
    meta.owner_references.push(OwnerReference {
        api_version: "flotilla.work/v1".into(),
        kind: "Project".into(),
        name: "parent".into(),
        controller: false,
    });
    let local = convoys.create(&meta, &convoy_spec("template")).await.unwrap();
    writer
        .replica_writer::<Convoy>(NodeId::new("local-root"), "flotilla")
        .replace(&ResourceList { items: vec![local.clone()], resource_version: "1".into(), generation: None }, chrono::Utc::now())
        .await
        .unwrap();
    writer
        .replica_writer::<Convoy>(NodeId::new("remote-root"), "flotilla")
        .replace(&ResourceList { items: vec![local.clone()], resource_version: "1".into(), generation: None }, chrono::Utc::now())
        .await
        .unwrap();
    let gap = Arc::new(AtomicBool::new(false));
    let view = WatchView::from_source(
        Arc::new(GapSource { source: StoreViewSource::new(reader.clone(), "flotilla"), gap: gap.clone() }),
        Some(NodeId::new("local-root")),
    )
    .await
    .unwrap();
    // Self-origin replicas are shadowed by unfiltered local existence; remote
    // same-name sources retain distinct identities and local sorts first.
    let listed = view.list().await.unwrap();
    assert_eq!(listed.items.len(), 2);
    assert!(matches!(listed.items[0].provenance, ResourceProvenance::Local));
    let mut changes = view.subscribe();
    let mut meta = InputMeta::from(&local.metadata);
    meta.labels.insert("team".into(), "new".into());
    let updated = convoys.update(&meta, &local.metadata.resource_version, &local.spec).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed()).await.unwrap().unwrap();
    // Label, declared-field and owner selectors intersect maintained indexes;
    // changing a label removes the old membership without a store read.
    let query = StoreQuery {
        labels: BTreeMap::from([("team".into(), "new".into())]),
        fields: BTreeMap::from([("metadata.name".into(), "alpha".into())]),
        owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "parent".into() }),
        include_replicas: true,
        ..StoreQuery::default()
    };
    assert_eq!(view.query(&query).await.unwrap().items.len(), 1);
    // The immutable Store adapter exposes the same declared indexed selectors
    // and refuses writes/missing kinds instead of falling back to live reads.
    let mut snapshot = flotilla_store::ViewSnapshot::new(Some(NodeId::new("local-root")));
    snapshot
        .insert(
            "flotilla",
            view.query(&StoreQuery { include_replicas: true, suppress_self_origin: false, ..StoreQuery::default() }).await.unwrap(),
        )
        .unwrap();
    let cached = ResourceBackend::from_arc(Arc::new(snapshot));
    assert_eq!(cached.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 1);
    assert!(cached.using::<Convoy>("flotilla").delete("alpha").await.is_err());
    assert!(cached.using::<flotilla_resources::Project>("flotilla").list().await.is_err());
    assert_eq!(cached.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 1);
    let old = StoreQuery { labels: BTreeMap::from([("team".into(), "old".into())]), include_replicas: true, ..StoreQuery::default() };
    assert_eq!(view.query(&old).await.unwrap().items.len(), 1);
    assert_eq!(
        cached.query_declared::<Convoy>("flotilla", &old).await.unwrap().items.len(),
        1,
        "snapshot shadowing uses unfiltered local existence"
    );
    // A gap hides the deletion event: recovery must replace the snapshot, remove
    // the deleted local/index membership, and reveal its self-origin replica.
    gap.store(true, Ordering::SeqCst);
    convoys.delete(&updated.metadata.name).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed()).await.unwrap().unwrap();
    assert!(view.query(&query).await.unwrap().items.is_empty());
    let listed = view.list().await.unwrap();
    assert_eq!(listed.items.len(), 2);
    assert!(listed.items.iter().all(|item| matches!(item.provenance, ResourceProvenance::Replica { .. })));
    // Replica deletions clean up identity and index memberships too.
    for root in ["local-root", "remote-root"] {
        writer
            .replica_writer::<Convoy>(NodeId::new(root), "flotilla")
            .replace(&ResourceList { items: vec![], resource_version: "2".into(), generation: None }, chrono::Utc::now())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed()).await.unwrap().unwrap();
    }
    assert!(view.list().await.unwrap().items.is_empty());
    // Definitions retain causal merge semantics. A view update must agree with
    // the existing resolver, including replica-only fields and deletion state.
    let projects = writer.definitions::<flotilla_resources::Project>("flotilla");
    let project = projects
        .create(
            &InputMeta::builder().name("project".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("Local".into()).build(),
        )
        .await
        .unwrap();
    let project_view = WatchView::from_source(
        Arc::new(StoreViewSource::<flotilla_resources::Project>::new(reader, "flotilla")),
        Some(NodeId::new("local-root")),
    )
    .await
    .unwrap();
    let mut changes = project_view.subscribe();
    let mut replica = project.clone();
    replica.metadata.merge = None;
    replica.metadata.resource_version = "100".into();
    replica.spec.display_name = "Remote".into();
    writer
        .replica_writer::<flotilla_resources::Project>(NodeId::new("remote-root"), "flotilla")
        .replace(&ResourceList { items: vec![replica], resource_version: "100".into(), generation: None }, chrono::Utc::now())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed()).await.unwrap().unwrap();
    let expected = projects.get("project").await.unwrap();
    let actual = project_view.list().await.unwrap().items.remove(0).object;
    assert_eq!(serde_json::to_value(actual.to_k8s_object()).unwrap(), serde_json::to_value(expected.to_k8s_object()).unwrap());
}

#[tokio::test]
async fn memory_watch_view_contract() {
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("local-root"));
    contract(backend.clone(), backend).await;
}
#[tokio::test]
async fn sqlite_watch_view_contract() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        ResourceBackend::new(SqliteBackend::open(directory.path().join("store.db")).unwrap()).with_local_root(NodeId::new("local-root"));
    contract(backend.clone(), backend).await;
}
#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires TCP bind")]
async fn http_watch_view_contract() {
    let writer = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("local-root"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/apis/flotilla.work/v1/namespaces/{namespace}/{kind}", get(endpoint)).with_state(writer.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let reader = ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), url));
    contract(reader, writer).await;
    server.abort();
}
