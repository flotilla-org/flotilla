//! All adapters run the same existing CRUD/list/watch scenarios. HTTP uses a
//! stand-in for the resource HTTP process, enforcing paths and Kubernetes bodies.
use axum::{
    body::{Body, Bytes},
    extract::{Path, RawQuery, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use flotilla_resources::*;
use flotilla_store::{ResourceBackend, ResourceKind};
use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use flotilla_store_testkit::{fixtures::contract::*, ReadCountsBackendExt, ReadOperation, ReadSnapshot, ReadTally};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

struct HttpFixture {
    backend: ResourceBackend,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HttpFixture {
    async fn new() -> Self {
        let authority = ResourceBackend::new(InMemoryBackend::default());
        let app = Router::new()
            .route("/apis/{group}/{version}/namespaces/{namespace}/{plural}", any(request))
            .route("/apis/{group}/{version}/namespaces/{namespace}/{plural}/{name}", any(request))
            .route("/apis/{group}/{version}/namespaces/{namespace}/{plural}/{name}/{subresource}", any(request))
            .with_state(authority);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind HTTP fixture");
        let url = format!("http://{}", listener.local_addr().expect("fixture address"));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve fixture");
        });
        Self { backend: ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), url)), task }
    }
}
async fn request(
    State(backend): State<ResourceBackend>,
    Path(path): Path<BTreeMap<String, String>>,
    RawQuery(query): RawQuery,
    method: Method,
    bytes: Bytes,
) -> Response {
    let query: BTreeMap<String, String> =
        url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes()).into_owned().collect();
    let kind = match path["plural"].as_str() {
        "convoys" => ResourceKind::of::<Convoy>(),
        "demands" => ResourceKind::of::<Demand>(),
        "regards" => ResourceKind::of::<Regard>(),
        other => panic!("unexpected fixture resource {other}"),
    };
    assert_eq!(path["group"], kind.paths.group);
    assert_eq!(path["version"], kind.paths.version);
    let namespace = &path["namespace"];
    let name = path.get("name");
    if method == Method::GET && query.get("watch").is_some_and(|v| v == "true") {
        let start = query.get("resourceVersion").map(|v| WatchStart::FromVersion(v.clone())).unwrap_or(WatchStart::Now);
        let stream = match backend.store().watch_raw(kind, namespace, start).await {
            Ok(stream) => stream,
            Err(error) => return error_response(error),
        };
        let body = stream.map(move |event| {
                let value = match event?.decode_raw(kind)? {
                    WatchEvent::Added(o) => json!({"type":"ADDED","object":kind.encode_object(&o)?}),
                    WatchEvent::Modified(o) => json!({"type":"MODIFIED","object":kind.encode_object(&o)?}),
                    WatchEvent::Deleted(o) => json!({"type":"DELETED","object":kind.encode_object(&o)?}),
                    WatchEvent::DeletedByName(t) => json!({"type":"DELETED","object":{"apiVersion":api_version(kind.paths),"kind":kind.paths.kind,"metadata":{"name":t.name,"namespace":t.namespace,"resourceVersion":t.resource_version,"annotations":t.annotations}}}),
                };
                Ok::<_,ResourceError>(format!("{value}\n"))
            });
        return Body::from_stream(body).into_response();
    }
    let result: Result<Value, ResourceError> = async {
        let store = backend.store();
        if method == Method::GET {
            return match name {
                Some(name) => kind.encode_object(&store.get_raw(kind, namespace, name).await?),
                None => {
                    let list = store.list_raw(kind, namespace).await?;
                    Ok(json!({"apiVersion":api_version(kind.paths),"kind":format!("{}List",kind.paths.kind),"metadata":{"resourceVersion":list.resource_version,"generation":list.generation},"items":list.items.iter().map(|o|kind.encode_object(o)).collect::<Result<Vec<_>,_>>()?}))
                }
            };
        }
        if method == Method::DELETE {
            store.delete_raw(kind, namespace, name.expect("delete name")).await?;
            return Ok(json!({}));
        }
        let body: Value = serde_json::from_slice(&bytes).expect("Kubernetes request JSON");
        assert_eq!(body["apiVersion"], api_version(kind.paths));
        assert_eq!(body["kind"], kind.paths.kind);
        let mut metadata = body["metadata"].clone();
        // K8s input metadata carries the deletion fence in camelCase.
        if let Some(timestamp) = metadata.as_object_mut().expect("metadata object").remove("deletionTimestamp") {
            metadata["deletion_timestamp"] = timestamp;
        }
        let meta: InputMeta = serde_json::from_value(metadata).expect("request metadata");
        let version = body["metadata"]["resourceVersion"].as_str();
        let object = if method == Method::POST {
            assert!(name.is_none());
            assert!(version.is_none());
            store.create_raw(kind, namespace, &meta, &body["spec"]).await?
        } else {
            assert_eq!(method, Method::PUT);
            assert_eq!(name.map(String::as_str), Some(meta.name.as_str()));
            if path.get("subresource").is_some_and(|s| s == "status") {
                store.update_status_raw(kind, namespace, &meta.name, version.expect("status version"), &body["status"]).await?
            } else {
                store.update_raw(kind, namespace, &meta, version.expect("update version"), &body["spec"]).await?
            }
        };
        kind.encode_object(&object)
    }.await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => error_response(error),
    }
}
fn error_response(error: ResourceError) -> Response {
    let status = match error {
        ResourceError::NotFound { .. } => StatusCode::NOT_FOUND,
        ResourceError::Conflict { .. } => StatusCode::CONFLICT,
        ResourceError::WatchExpired { .. } => StatusCode::GONE,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, axum::Json(json!({"message":error.to_string()}))).into_response()
}
async fn backend(name: &str) -> (ResourceBackend, Option<HttpFixture>) {
    match name {
        "memory" => (ResourceBackend::new(InMemoryBackend::default()), None),
        "sqlite" => (ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite")), None),
        "http" => {
            let fixture = HttpFixture::new().await;
            (fixture.backend.clone(), Some(fixture))
        }
        _ => unreachable!(),
    }
}
macro_rules! contract {
    ($module:ident, $fixture:ty) => {
        mod $module {
            use super::*;
            // Existing behavioral contracts must hold across all storage adapters,
            // including optimistic concurrency, no-ops, metadata and watch replay.
            #[rstest::rstest]
            #[case("memory")]
            #[case("sqlite")]
            #[case("http")]
            #[tokio::test]
            async fn shared_contract(#[case] adapter: &str, #[values(0, 1, 2, 3, 4, 5, 6, 7, 8)] scenario: u8) {
                if cfg!(feature = "skip-no-sandbox-tests") && adapter == "http" {
                    return;
                }
                let (backend, _server) = backend(adapter).await;
                match scenario {
                    0 => assert_create_get_list_roundtrip_with_backend::<$fixture>(backend).await,
                    1 => assert_stale_resource_version_conflicts_with_backend::<$fixture>(backend).await,
                    2 => assert_identical_update_is_noop_with_backend::<$fixture>(backend).await,
                    3 => assert_delete_emits_event_with_backend::<$fixture>(backend).await,
                    4 => assert_watch_from_version_replays_with_backend::<$fixture>(backend).await,
                    5 => assert_watch_now_semantics_with_backend::<$fixture>(backend).await,
                    6 => assert_namespace_isolation_with_backend::<$fixture>(backend).await,
                    7 => assert_metadata_roundtrip_with_backend::<$fixture>(backend).await,
                    8 => assert_repeated_delete_with_pending_finalizers_is_noop_with_backend::<$fixture>(backend).await,
                    _ => unreachable!(),
                }
            }
        }
    };
}
contract!(convoy, ConvoyFixture);
contract!(demand, DemandFixture);
contract!(regard, RegardFixture);

// A request-scoped observer reports the same decoded objects and read calls
// through every adapter, while an unobserved handle has no counters attached.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("http")]
#[tokio::test]
async fn read_observers_cross_the_store_trait(#[case] adapter: &str) {
    if cfg!(feature = "skip-no-sandbox-tests") && adapter == "http" {
        return;
    }
    let (plain, _server) = backend(adapter).await;
    let observed = plain.clone().with_read_counts();
    let resolver = observed.using::<Convoy>("flotilla");
    resolver.create(&ConvoyFixture::meta("counted"), &ConvoyFixture::spec()).await.expect("create");
    resolver.get("counted").await.expect("point read");
    resolver.list().await.expect("collection read");
    assert_eq!(observed.read_counts().get("Convoy"), Some(&2));
    assert_eq!(observed.read_calls().get("Convoy"), Some(&2));
    assert!(plain.read_counts().is_empty());
    // Every adapter classifies the operation the same way.
    let reads = observed.read_snapshot();
    assert_eq!(reads.tally("Convoy", ReadOperation::Get), ReadTally { calls: 1, objects: 1 });
    assert_eq!(reads.tally("Convoy", ReadOperation::List), ReadTally { calls: 1, objects: 1 });
}

// Selector-bounded reads are queries and unselective ones are lists, whichever
// API spells them; replica-inclusive reads keep the same classification. (The
// HTTP stand-in serves only the resource API, so this runs on local adapters.)
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn read_operations_classify_selectors(#[case] adapter: &str) {
    use flotilla_store::StoreQuery;
    let (plain, _server) = backend(adapter).await;
    let resolver = plain.using::<Convoy>("flotilla");
    for name in ["first", "second"] {
        let meta = InputMeta::builder().name(name.into()).labels(BTreeMap::from([("pick".to_string(), name.to_string())])).build();
        resolver.create(&meta, &ConvoyFixture::spec()).await.expect("create");
    }
    let observed = plain.clone().with_read_counts();
    let measure = |reads: ReadSnapshot| reads.entries().map(|(_, operation, tally)| (operation, tally)).collect::<Vec<_>>();
    let one = |operation, objects| vec![(operation, ReadTally { calls: 1, objects })];
    let resolver = observed.using::<Convoy>("flotilla");
    let replicas = observed.including_replicas::<Convoy>("flotilla");
    let picked = StoreQuery::labels(&BTreeMap::from([("pick".to_string(), "first".to_string())]), false, true);
    let cases: Vec<(&str, Vec<(ReadOperation, ReadTally)>)> = vec![
        ("labels", one(ReadOperation::Query, 1)),
        ("declared", one(ReadOperation::Query, 1)),
        ("unselective", one(ReadOperation::List, 2)),
        ("replica get", one(ReadOperation::Get, 1)),
        ("replica list", vec![(ReadOperation::List, ReadTally { calls: 2, objects: 2 })]),
    ];
    for (case, expected) in cases {
        let before = observed.read_snapshot();
        match case {
            "labels" => {
                resolver.list_matching_labels(&picked.labels).await.expect("labels");
            }
            "declared" => {
                resolver.query_declared(&picked).await.expect("declared");
            }
            "unselective" => {
                resolver.query_declared(&StoreQuery::default()).await.expect("unselective");
            }
            "replica get" => {
                replicas.get("first").await.expect("replica get");
            }
            "replica list" => {
                replicas.list().await.expect("replica list");
            }
            _ => unreachable!(),
        }
        assert_eq!(measure(observed.read_snapshot().since(&before)), expected, "{adapter}: {case}");
    }
}

// Default refusal messages describe the port; the HTTP adapter preserves its
// legacy public errors for operations it cannot author locally.
#[tokio::test]
async fn unsupported_defaults_are_neutral_and_http_refusals_are_unchanged() {
    let memory = ResourceBackend::new(InMemoryBackend::default());
    assert_eq!(memory.store().unsupported_operation("replicas"), ResourceError::invalid("this store does not support replicas"));
    let http = ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), "http://127.0.0.1:1"));
    assert!(matches!(http.store().list_replicas_raw(ResourceKind::of::<Convoy>(), "flotilla").await,
        Err(ResourceError::Invalid { message }) if message == "HTTP backends cannot hold replicas"));
    assert!(matches!(http.local_namespaces::<Convoy>().await,
        Err(ResourceError::Invalid { message }) if message == "HTTP backends cannot enumerate local resource namespaces"));
}

// A named read sees the same provenances, in the same order, as the matching
// rows of a namespace list: local first, then replicas by origin. Callers that
// replaced a list-and-find with `get_all` keep choosing the same copy.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn get_all_orders_provenances_like_list(#[case] adapter: &str) {
    use flotilla_protocol::NodeId;
    use flotilla_store::StoreExt;
    let (plain, _server) = backend(adapter).await;
    let backend = plain.with_local_root(NodeId::new("self"));
    backend.using::<Convoy>("flotilla").create(&ConvoyFixture::meta("alpha"), &ConvoyFixture::spec()).await.expect("local");
    // Insert origins out of order so the read cannot rely on insertion order.
    for origin in ["peer-c", "peer-a", "peer-b"] {
        let peer = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new(origin));
        for name in ["alpha", "beta"] {
            peer.using::<Convoy>("flotilla").create(&ConvoyFixture::meta(name), &ConvoyFixture::spec()).await.expect("peer row");
        }
        let listed = peer.using::<Convoy>("flotilla").list().await.expect("peer list");
        backend
            .replace_replicas_typed::<Convoy>(&NodeId::new(origin), "flotilla", &listed, chrono::Utc::now(), None)
            .await
            .expect("replicas");
    }
    let replicas = backend.including_replicas::<Convoy>("flotilla");
    let provenances = |items: Vec<flotilla_resources::ReadResourceObject<Convoy>>| {
        items.into_iter().filter(|item| item.object.metadata.name == "alpha").map(|item| item.provenance).collect::<Vec<_>>()
    };
    let named = provenances(replicas.get_all("alpha").await.expect("named").items);
    let listed = provenances(replicas.list_replica_sources().await.expect("listed").items);
    assert_eq!(named.len(), 4, "{adapter}: local plus three origins");
    assert_eq!(named, listed, "{adapter}");
    assert_eq!(
        replicas.get_all("alpha").await.expect("named").items.first().map(|item| item.provenance.clone()),
        replicas
            .list()
            .await
            .expect("list")
            .items
            .into_iter()
            .find(|item| item.object.metadata.name == "alpha")
            .map(|item| item.provenance),
        "{adapter}: the first visible copy is the one list-and-find chose"
    );
}

#[tokio::test]
async fn replica_burst_recovers_in_memory() {
    assert_replica_burst_recovers(ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn replica_burst_recovers_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let backend = SqliteBackend::open(directory.path().join("queue.sqlite")).unwrap();
    assert_replica_burst_recovers(ResourceBackend::new(backend)).await;
}

// Missing objects still cross the store boundary: zero decoded objects does
// not mean zero calls. All adapters obey the same accounting contract.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("http")]
#[tokio::test]
async fn read_counts_include_missing_gets(#[case] adapter: &str) {
    let (plain, _server) = backend(adapter).await;
    let counted = plain.with_read_counts();
    assert!(matches!(counted.using::<Convoy>("flotilla").get("absent").await, Err(ResourceError::NotFound { .. })));
    assert_eq!(counted.read_snapshot().tally("Convoy", ReadOperation::Get), ReadTally { calls: 1, objects: 0 });
}

// Collection membership is set-valued even when the stored subject list repeats
// a subject under the same or different relationships (#3171).
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("sqlite-reopen")]
#[tokio::test]
async fn duplicate_collection_values(#[case] adapter: &str) {
    use flotilla_store::StoreQuery;
    let directory = (adapter == "sqlite-reopen").then(|| tempfile::tempdir().unwrap());
    let path = directory.as_ref().map(|dir| dir.path().join("store.sqlite"));
    let (backend, _server) =
        if let Some(path) = &path { (ResourceBackend::new(SqliteBackend::open(path).unwrap()), None) } else { backend(adapter).await };
    let resolver = backend.using::<Convoy>("flotilla");
    let mut meta = ConvoyFixture::meta("duplicates");
    meta.labels.insert("index-contract".into(), "same".into());
    let owner = OwnerReference { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "owner".into(), controller: false };
    meta.owner_references = vec![owner.clone(), OwnerReference { controller: true, ..owner.clone() }, owner];
    let created = resolver.create(&meta, &ConvoyFixture::spec()).await.expect("domain-accepted duplicate owners apply locally");
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "2524".into(),
    };
    let mut status = created.status.unwrap_or_default();
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Produces,
        SubjectDiscoverySource::Operator,
        chrono::Utc::now(),
    );
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Supersedes,
        SubjectDiscoverySource::Operator,
        chrono::Utc::now(),
    );
    status.subjects.push(status.subjects[0].clone());
    let updated =
        resolver.update_status("duplicates", &created.metadata.resource_version, &status).await.expect("apply duplicate local subjects");
    let origin = flotilla_protocol::NodeId::new("peer");
    backend
        .replica_writer::<Convoy>(origin, "remote")
        .replace(
            &ResourceList { items: vec![updated.clone()], resource_version: updated.metadata.resource_version.clone(), generation: None },
            chrono::Utc::now(),
        )
        .await
        .expect("apply duplicate replica subjects");
    drop(resolver);
    let backend = if let Some(path) = &path {
        drop(backend);
        // Simulate an older/unconfigured writer leaving projections empty while
        // duplicate subjects are already durable. Open must rebuild both sides.
        let raw = rusqlite::Connection::open(path).unwrap();
        // Legacy/external JSON can repeat a label key. The domain decoder takes
        // its last value; rebuilding the lookup must accept the same body.
        for table in ["resource_objects", "replica_objects"] {
            let body: String =
                raw.query_row(&format!("SELECT body_json FROM {table} WHERE name='duplicates'"), [], |row| row.get(0)).unwrap();
            let body = body.replace("\"index-contract\":\"same\"", "\"index-contract\":\"old\",\"index-contract\":\"same\"");
            raw.execute(&format!("UPDATE {table} SET body_json=?1 WHERE name='duplicates'"), [body]).unwrap();
        }
        for table in [
            "resource_objects_Convoy_status_subjects___subject_values",
            "replica_objects_Convoy_status_subjects___subject_values",
            "resource_objects_labels",
            "resource_objects_owners",
            "replica_objects_labels",
            "replica_objects_owners",
        ] {
            raw.execute(&format!("DELETE FROM {table}"), []).unwrap();
        }
        drop(raw);
        ResourceBackend::new(SqliteBackend::open(path).expect("reopen repairs duplicate projections"))
    } else {
        backend
    };
    for namespace in ["flotilla", "remote"] {
        let result = backend
            .query_declared::<Convoy>(
                namespace,
                &StoreQuery {
                    fields: BTreeMap::from([
                        ("status.subjects.*.subject".into(), serde_json::to_value(&subject).unwrap().to_string()),
                        (
                            "status.phase".into(),
                            serde_json::to_value(updated.status.as_ref().unwrap().phase).unwrap().as_str().unwrap().into(),
                        ),
                    ]),
                    labels: BTreeMap::from([("index-contract".into(), "same".into())]),
                    owner: Some(flotilla_store::OwnerQuery {
                        api_version: "flotilla.work/v1".into(),
                        kind: "Project".into(),
                        name: "owner".into(),
                    }),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 1, "membership returns the convoy once");
        assert_eq!(
            &result.items[0].object.status.as_ref().unwrap().subjects,
            &updated.status.as_ref().unwrap().subjects,
            "relationship history remains unchanged"
        );
        assert_eq!(result.items[0].object.metadata.owner_references.len(), 3, "owner-reference bodies are unchanged");
    }
}

// One undecodable object in a replica delivery is quarantined by identity;
// valid objects on both sides still apply, and a corrected delivery clears it.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn replica_batch_quarantines_one_object(#[case] adapter: &str, #[values(false, true)] snapshot: bool) {
    use flotilla_store::{SerializedResource, StoreExt};
    let (backend, _server) = backend(adapter).await;
    let source = ResourceBackend::new(InMemoryBackend::default());
    let mut objects = Vec::new();
    for name in ["before", "bad", "after"] {
        let object = source.using::<Convoy>("flotilla").create(&ConvoyFixture::meta(name), &ConvoyFixture::spec()).await.unwrap();
        objects.push(flotilla_store::transcode::<_, ResourceObject<SerializedResource>>(object).unwrap());
    }
    let corrected = objects[1].clone();
    objects[1].spec = json!(null);
    let rejected = objects[1].clone();
    let origin = flotilla_protocol::NodeId::new("peer");
    let kind = ResourceKind::of::<Convoy>();
    let at = chrono::Utc::now();
    if snapshot {
        backend
            .store()
            .replace_replicas_raw(
                kind,
                &origin,
                "flotilla",
                &ResourceList { items: objects, resource_version: "3".into(), generation: None },
                at,
                None,
            )
            .await
            .unwrap();
    } else {
        let events = objects.into_iter().map(|object| (WatchEvent::Added(object), at)).collect::<Vec<_>>();
        backend.store().apply_replica_batch_raw(kind, &origin, "flotilla", &events).await.unwrap();
    }
    let names = backend
        .including_replicas::<Convoy>("flotilla")
        .list()
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|item| item.object.metadata.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names, std::collections::BTreeSet::from(["before".into(), "after".into()]));
    let diagnostics = backend.store().diagnostics().await.unwrap().unwrap();
    assert_eq!(diagnostics.replica_quarantines.len(), 1);
    assert_eq!(diagnostics.replica_quarantines[0].name, "bad");
    assert_eq!(diagnostics.replica_quarantines[0].origin_root, origin);
    assert!(!diagnostics.replica_quarantines[0].error.is_empty());
    // Re-delivering the same rejected body preserves the first rejection time.
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    backend
        .store()
        .apply_replica_batch_raw(kind, &origin, "flotilla", &[(WatchEvent::Added(rejected.clone()), at + chrono::Duration::seconds(1))])
        .await
        .unwrap();
    assert_eq!(
        backend.store().diagnostics().await.unwrap().unwrap().replica_quarantines[0].quarantined_at,
        diagnostics.replica_quarantines[0].quarantined_at
    );

    backend
        .store()
        .apply_replica_batch_raw(kind, &origin, "flotilla", &[(WatchEvent::Modified(corrected), at + chrono::Duration::seconds(1))])
        .await
        .unwrap();
    assert_eq!(backend.store().list_replicas_typed::<Convoy>("flotilla").await.unwrap().len(), 3);
    assert!(backend.store().diagnostics().await.unwrap().unwrap().replica_quarantines.is_empty());
    // An authoritative snapshot omission clears this origin's quarantine while
    // preserving a rejection with the same name at another origin.
    let other = flotilla_protocol::NodeId::new("other-peer");
    for origin in [&origin, &other] {
        backend
            .store()
            .apply_replica_batch_raw(kind, origin, "flotilla", &[(WatchEvent::Added(rejected.clone()), at + chrono::Duration::seconds(2))])
            .await
            .unwrap();
    }
    backend
        .store()
        .replace_replicas_raw(
            kind,
            &origin,
            "flotilla",
            &ResourceList { items: Vec::new(), resource_version: "4".into(), generation: None },
            at + chrono::Duration::seconds(3),
            None,
        )
        .await
        .unwrap();
    let quarantines = backend.store().diagnostics().await.unwrap().unwrap().replica_quarantines;
    assert_eq!(quarantines.len(), 1);
    assert_eq!(quarantines[0].origin_root, other);
}

// A valid decoded object can still fail in SQLite maintenance. Its savepoint
// must undo the object and every trigger side effect while the batch continues.
#[rstest::rstest]
#[tokio::test]
async fn replica_sql_failure_rolls_back_only_one_record(#[values(false, true)] snapshot: bool) {
    use flotilla_store::StoreExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite");
    let sqlite = SqliteBackend::open(&path).unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("CREATE TRIGGER reject_bad_replica AFTER INSERT ON replica_objects WHEN NEW.name='bad' BEGIN SELECT RAISE(FAIL, 'test unappliable replica'); END;").unwrap();
    drop(raw);
    let backend = ResourceBackend::new(sqlite);
    let source = ResourceBackend::new(InMemoryBackend::default());
    for name in ["before", "bad", "after"] {
        source.using::<Convoy>("flotilla").create(&ConvoyFixture::meta(name), &ConvoyFixture::spec()).await.unwrap();
    }
    let origin = flotilla_protocol::NodeId::new("peer");
    let listed = source.using::<Convoy>("flotilla").list().await.unwrap();
    let mut watch = backend.including_replicas::<Convoy>("flotilla").watch().await.unwrap();
    if snapshot {
        backend.replica_writer::<Convoy>(origin.clone(), "flotilla").replace(&listed, chrono::Utc::now()).await.unwrap();
    } else {
        let events = listed.items.into_iter().map(WatchEvent::Added).collect::<Vec<_>>();
        backend.replica_writer::<Convoy>(origin.clone(), "flotilla").apply_batch(&events, chrono::Utc::now()).await.unwrap();
    }
    for _ in 0..2 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next()).await.unwrap().unwrap().unwrap();
        let ReadWatchEvent::Added(item) = event else { panic!("expected committed addition") };
        assert_ne!(item.object.metadata.name, "bad", "rejected record must never publish");
    }
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), watch.next()).await.is_err(), "only committed objects publish");
    drop(watch);
    let names = backend
        .store()
        .list_replicas_typed::<Convoy>("flotilla")
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.object.metadata.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names, std::collections::BTreeSet::from(["before".into(), "after".into()]));
    drop(backend);
    let raw = rusqlite::Connection::open(&path).unwrap();
    let digest_count: usize =
        raw.query_row("SELECT count(*) FROM digest_entries WHERE origin='peer' AND name='bad'", [], |row| row.get(0)).unwrap();
    assert_eq!(digest_count, 0, "trigger projection rolled back with rejected object");
    drop(raw);
    let reopened = ResourceBackend::new(SqliteBackend::open(&path).unwrap());
    let diagnostics = reopened.store().diagnostics().await.unwrap().unwrap();
    assert_eq!(diagnostics.replica_quarantines.len(), 1, "quarantine survives restart");
    assert!(diagnostics.replica_quarantines[0].error.contains("test unappliable replica"));
}

// A bucket snapshot proves absence only within that bucket; a full snapshot
// subsequently clears the remaining rejected name for the same origin.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn replica_snapshot_quarantine_pruning_is_bucket_scoped(#[case] adapter: &str) {
    use flotilla_store::SerializedResource;
    let (backend, _server) = backend(adapter).await;
    let source = ResourceBackend::new(InMemoryBackend::default());
    let first = "bad";
    let second = (0..256)
        .map(|i| format!("other-{i}"))
        .find(|name| flotilla_resources::digest_bucket(name) != flotilla_resources::digest_bucket(first))
        .unwrap();
    let kind = ResourceKind::of::<Convoy>();
    let origin = flotilla_protocol::NodeId::new("peer");
    let at = chrono::Utc::now();
    for name in [first, &second] {
        let object = source.using::<Convoy>("flotilla").create(&ConvoyFixture::meta(name), &ConvoyFixture::spec()).await.unwrap();
        let mut object = flotilla_store::transcode::<_, ResourceObject<SerializedResource>>(object).unwrap();
        object.spec = json!(null);
        backend.store().apply_replica_batch_raw(kind, &origin, "flotilla", &[(WatchEvent::Added(object), at)]).await.unwrap();
    }
    let empty = ResourceList { items: Vec::new(), resource_version: "3".into(), generation: None };
    backend
        .store()
        .replace_replicas_raw(kind, &origin, "flotilla", &empty, at, Some(flotilla_resources::digest_bucket(first)))
        .await
        .unwrap();
    let quarantines = backend.store().diagnostics().await.unwrap().unwrap().replica_quarantines;
    assert_eq!(quarantines.len(), 1);
    assert_eq!(quarantines[0].name, second);
    backend.store().replace_replicas_raw(kind, &origin, "flotilla", &empty, at, None).await.unwrap();
    assert!(backend.store().diagnostics().await.unwrap().unwrap().replica_quarantines.is_empty());
}
