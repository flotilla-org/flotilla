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
        Self { backend: ResourceBackend::new(HttpBackend::new(rigging_tls::client(), url)), task }
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
        "environments" => ResourceKind::of::<Environment>(),
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
    let http = ResourceBackend::new(HttpBackend::new(rigging_tls::client(), "http://127.0.0.1:1"));
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

// Admission metadata must seed status atomically across adapters: the create
// response, first Added event and stored object all carry the same decision.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("http")]
#[tokio::test]
async fn admission_placement_is_published_with_convoy(#[case] adapter: &str) {
    use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
    use flotilla_resources::convoy::ADMISSION_PLACEMENT_ANNOTATION;
    let (backend, _fixture) = backend(adapter).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let decision = PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: "admitted".into(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("home"), display_name: "Home".into() },
        minimal_alternatives: vec!["other-policy".into()],
        escalation_reason: Some("admission evidence".into()),
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };
    let spec = flotilla_store_testkit::fixtures::valid_convoy_spec();
    // An old-style create remains statusless; malformed admission data is
    // refused without publishing a partial resource or consuming a version.
    let bare_meta = InputMeta::builder().name("bare".into()).build();
    let bare = convoys.create(&bare_meta, &spec).await.expect("legacy create");
    assert!(bare.status.is_none());
    let malformed = InputMeta::builder()
        .name("invalid".into())
        .annotations(BTreeMap::from([(ADMISSION_PLACEMENT_ANNOTATION.into(), "null".into())]))
        .build();
    assert!(matches!(convoys.create(&malformed, &spec).await, Err(ResourceError::Invalid { .. })));
    assert!(matches!(convoys.get("invalid").await, Err(ResourceError::NotFound { .. })));
    assert_eq!(convoys.current_position().await.expect("position").resource_version, bare.metadata.resource_version);

    let meta = InputMeta::builder()
        .name("admitted".into())
        .annotations(BTreeMap::from([(ADMISSION_PLACEMENT_ANNOTATION.into(), serde_json::to_string(&decision).unwrap())]))
        .build();
    let created = convoys.create(&meta, &spec).await.expect("admitted create");
    let mut watch = convoys.watch(WatchStart::FromVersion(bare.metadata.resource_version)).await.expect("watch replay");
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
        .await
        .expect("Added deadline")
        .expect("Added event")
        .expect("watch success");
    let WatchEvent::Added(added) = event else { panic!("expected Added") };
    assert_eq!(serde_json::to_value(&added).unwrap(), serde_json::to_value(&created).unwrap());
    assert_eq!(created.status.as_ref().and_then(|status| status.placement_decision.as_ref()), Some(&decision));
    assert_eq!(
        serde_json::to_value(convoys.get("admitted").await.expect("stored convoy")).unwrap(),
        serde_json::to_value(&created).unwrap()
    );
    assert!(matches!(convoys.create(&meta, &spec).await, Err(ResourceError::Conflict { .. })));

    // Later metadata/spec writes must preserve runtime state and never rerun
    // creation initialization, even if the admission annotation is changed.
    let mut changed_meta = InputMeta::from(&created.metadata);
    let mut changed_decision = decision;
    changed_decision.kind = "later".into();
    changed_meta.annotations.insert(ADMISSION_PLACEMENT_ANNOTATION.into(), serde_json::to_string(&changed_decision).unwrap());
    let updated = convoys.update(&changed_meta, &created.metadata.resource_version, &spec).await.expect("metadata update");
    assert_eq!(updated.status, created.status);
}

// ADR 0016: a peer joining after a deletion must learn the retained deletion
// fence, including after a complete snapshot removed the last live replica.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("sqlite-reopened")]
#[case("observed-durable")]
#[tokio::test]
async fn late_relay_watch_includes_retained_deletions(#[case] adapter: &str, #[values("object", "name", "snapshot")] deletion: &str) {
    let temp = tempfile::tempdir().expect("create relay test directory");
    let path = temp.path().join("relay.sqlite");
    let (backend, _server) = if adapter == "sqlite-reopened" {
        (ResourceBackend::new(SqliteBackend::open(&path).expect("open persistent relay store")), None)
    } else if adapter == "observed-durable" {
        (
            ResourceBackend::new(InMemoryBackend::observed_with_durable_replicas(
                SqliteBackend::open(&path).expect("open durable observation replicas"),
            )),
            None,
        )
    } else {
        backend(adapter).await
    };
    let origin = flotilla_protocol::NodeId::new("origin");
    let source = ResourceBackend::new(InMemoryBackend::default());
    let object = source
        .using::<Convoy>("flotilla")
        .create(&ConvoyFixture::meta("gone"), &ConvoyFixture::spec())
        .await
        .expect("create source record");
    let at = chrono::Utc::now();
    let writer = backend.replica_writer::<Convoy>(origin.clone(), "flotilla");
    writer.apply(WatchEvent::Added(object.clone()), at - chrono::Duration::seconds(1)).await.expect("seed stale replica");
    if deletion == "snapshot" {
        writer
            .replace(&ResourceList { items: vec![], resource_version: "2".into(), generation: None }, at)
            .await
            .expect("apply empty authority snapshot");
    } else if deletion == "name" {
        writer
            .apply(
                WatchEvent::DeletedByName(ResourceTombstone {
                    name: "gone".into(),
                    namespace: "flotilla".into(),
                    resource_version: "2".into(),
                    annotations: BTreeMap::new(),
                }),
                at,
            )
            .await
            .expect("apply name-only deletion");
    } else {
        writer.apply(WatchEvent::Deleted(object), at).await.expect("apply object deletion");
    }
    // Fences in another namespace must not leak into this relay.
    backend
        .replica_writer::<Convoy>(origin.clone(), "other")
        .apply(
            WatchEvent::DeletedByName(ResourceTombstone {
                name: "other-only".into(),
                namespace: "other".into(),
                resource_version: "2".into(),
                annotations: BTreeMap::new(),
            }),
            at,
        )
        .await
        .expect("seed unrelated namespace deletion");
    drop(writer);
    let backend = if adapter == "sqlite-reopened" {
        drop(backend);
        ResourceBackend::new(SqliteBackend::open(&path).expect("reopen relay store"))
    } else {
        backend
    };
    let writer = backend.replica_writer::<Convoy>(origin.clone(), "flotilla");
    let watch =
        flotilla_store::watch_resource_kind_replica_sources(&backend, "flotilla", "convoys").await.expect("watch late relay snapshot");
    assert_eq!(watch.initial.len(), 1, "late subscribers receive the retained deletion");
    let event = &watch.initial[0];
    assert_eq!(event["type"], "DELETED");
    assert_eq!(event["object"]["metadata"]["name"], "gone");
    assert_eq!(event["object"]["metadata"]["annotations"]["flotilla.work/origin-root"], origin.to_string());
    assert_eq!(event["object"]["metadata"]["annotations"]["flotilla.work/last-synced-at"], at.to_rfc3339());
    assert!(
        backend.including_replicas::<Convoy>("flotilla").list().await.expect("list live replica rows").items.is_empty(),
        "deletion fences are not live rows"
    );
    // A subsequent recreation supersedes the fence in the next relay snapshot.
    let object = source.using::<Convoy>("flotilla").get("gone").await.expect("read source fixture");
    writer.apply(WatchEvent::Added(object), at + chrono::Duration::seconds(1)).await.expect("recreate replica after deletion");
    let watch =
        flotilla_store::watch_resource_kind_replica_sources(&backend, "flotilla", "convoys").await.expect("watch recreated replica");
    assert_eq!(watch.initial.len(), 1);
    assert_eq!(watch.initial[0]["type"], "ADDED");
}

// The registry builds watches only for local stores. HTTP-backed clients use
// Store::watch_replica_sources_raw to consume the remote resource API instead.
#[tokio::test]
async fn replica_source_registry_rejects_http_backed_stores_before_io() {
    let backend = ResourceBackend::new(HttpBackend::new(rigging_tls::client(), "http://unused.invalid"));
    let error = flotilla_store::watch_resource_kind_replica_sources(&backend, "flotilla", "convoys")
        .await
        .err()
        .expect("registry must reject remote view stores");
    assert!(error.to_string().contains("HTTP replica-source watches use the resource API"), "{error}");
}

// Both adapters expose exactly the absent identities, in stable origin/name
// order. A live row and its deletion fence must never coexist in a quiescent snapshot.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn relay_deletions_are_sorted_and_disjoint_from_live_rows(#[case] adapter: &str) {
    use flotilla_store::StoreExt;
    let (backend, _server) = backend(adapter).await;
    let source = ResourceBackend::new(InMemoryBackend::default());
    let at = chrono::Utc::now();
    for name in ["a", "b", "c"] {
        source.using::<Convoy>("flotilla").create(&ConvoyFixture::meta(name), &ConvoyFixture::spec()).await.expect("source fixture");
    }
    for (origin, name) in [("z", "b"), ("a", "c"), ("z", "a"), ("a", "a")] {
        let object = source.using::<Convoy>("flotilla").get(name).await.expect("source object");
        let writer = backend.replica_writer::<Convoy>(flotilla_protocol::NodeId::new(origin), "flotilla");
        writer.apply(WatchEvent::Deleted(object), at).await.expect("retain deletion");
    }
    let object = source.using::<Convoy>("flotilla").get("a").await.expect("recreation fixture");
    backend
        .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("a"), "flotilla")
        .apply(WatchEvent::Added(object), at + chrono::Duration::seconds(1))
        .await
        .expect("recreate one identity");
    let deleted = backend.store().list_replica_deletions_raw(ResourceKind::of::<Convoy>(), "flotilla").await.expect("deletion fences");
    let identities = deleted.iter().map(|row| (row.origin_root.to_string(), row.name.clone())).collect::<Vec<_>>();
    assert_eq!(identities, [("a", "c"), ("z", "a"), ("z", "b")].map(|(origin, name)| (origin.to_string(), name.to_string())));
    let live = backend.store().list_replicas_typed::<Convoy>("flotilla").await.expect("live replicas");
    assert_eq!(live.len(), 1, "one recreated origin/name is live");
    for row in live {
        let ResourceProvenance::Replica { origin_root, .. } = row.provenance else { panic!("replica provenance") };
        assert!(!identities.contains(&(origin_root.to_string(), row.object.metadata.name)), "live identities cannot also be fenced");
    }
}

// ADR 0047/0058: old records can bind their instance only in metadata. Every
// typed read/write adapter must preserve that binding and converge after one
// canonical write; equality of canonical specs must not cause update loops.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[case("http")]
#[tokio::test]
async fn legacy_environment_instance_survives_read_write_and_noop(#[case] adapter: &str, #[values(false, true)] direct: bool) {
    if cfg!(feature = "skip-no-sandbox-tests") && adapter == "http" {
        return;
    }
    let (backend, _server) = backend(adapter).await;
    let meta = InputMeta::builder()
        .name("legacy-environment".into())
        .labels(BTreeMap::from([("flotilla.work/environment-provider-instance".into(), "custom-instance".into())]))
        .build();
    let legacy = if direct {
        json!({"host_direct":{"host_ref":"home", "repo_default_dir":"/work"}})
    } else {
        json!({"docker":{"host_ref":"home", "image":"image"}})
    };
    // Raw seed models the preceding generation's stored bytes, not a new author.
    backend.store().create_raw(ResourceKind::of::<Environment>(), "flotilla", &meta, &legacy).await.expect("old stored record");
    let api = backend.using::<Environment>("flotilla");
    let read = api.get(&meta.name).await.expect("typed legacy read");
    let expected = "host:home/provider:custom-instance";
    assert_eq!(read.spec.provider_address.as_ref().expect("metadata binding resolved").as_str(), expected);
    let unresolved: EnvironmentSpec = serde_json::from_value(legacy.clone()).expect("old standalone spec");
    assert!(read.matches_update(&meta, &unresolved).expect("canonical no-op comparison"));
    let updated = api.update(&meta, &read.metadata.resource_version, &unresolved).await.expect("legacy write");
    let raw = backend.store().get_raw(ResourceKind::of::<Environment>(), "flotilla", &meta.name).await.expect("canonical stored bytes");
    assert_eq!(raw.spec["provider_address"], expected);
    assert!(raw.spec[if direct { "host_direct" } else { "docker" }].get("host_ref").is_none());
    let unchanged = api.update(&meta, &updated.metadata.resource_version, &unresolved).await.expect("repeat unresolved write");
    assert_eq!(unchanged.metadata.resource_version, updated.metadata.resource_version, "canonical updates converge after one write");
    let old_document = json!({"kind":"Environment", "metadata":{"labels":meta.labels}, "spec":legacy});
    let current_document = json!({"kind":"Environment", "metadata":{"labels":meta.labels}, "spec":raw.spec});
    assert_eq!(
        flotilla_store::resource_document_spec_hash(&old_document).expect("old hash"),
        flotilla_store::resource_document_spec_hash(&current_document).expect("current hash")
    );
}

// Behaviour (#3824 layer 2a, owner ruling 2026-10-26): built-in definitions come from the code.
// Every merged read sees each built-in workflow template and stdlib image layer at the built-in
// version, with nothing stored; a stored copy an earlier generation seeded is shadowed; a write,
// metadata edit or delete under a built-in's name is refused; other definitions read beside them.
// (An HTTP backend authors no definitions, so this runs on the local adapters.)
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn built_in_definitions_are_read_from_the_code_and_never_written(#[case] adapter: &str) {
    use flotilla_resources::{builtins::builtin_definitions, ImageLayer, WorkflowTemplate};
    let (backend, _server) = backend(adapter).await;
    let templates = backend.definitions::<WorkflowTemplate>("flotilla");
    let builtin_names =
        |kind: &str| builtin_definitions(kind).expect("built-ins load").iter().map(|builtin| builtin.name.clone()).collect::<Vec<_>>();
    let template_names = builtin_names("WorkflowTemplate");
    assert!(template_names.contains(&"single-agent".to_string()));
    let single = templates.get("single-agent").await.expect("a built-in reads with nothing stored");
    assert_eq!(single.spec, flotilla_resources::single_agent_workflow_spec());
    assert_eq!(single.metadata.resource_version, flotilla_store::BUILTIN_VERSION);
    // A user template reads beside the built-ins; listing and a named read see both.
    let user = flotilla_resources::single_agent_workflow_spec();
    let labelled = BTreeMap::from([("team".to_string(), "alpha".to_string())]);
    templates
        .apply(&InputMeta::builder().name("variant".to_string()).labels(labelled.clone()).build(), &user)
        .await
        .expect("a new name is stored");
    let mut listed: Vec<_> = templates.list().await.expect("list").into_iter().map(|template| template.metadata.name).collect();
    listed.sort();
    let mut expected = template_names.clone();
    expected.push("variant".into());
    expected.sort();
    assert_eq!(listed, expected);
    let named = templates.list_names(["single-agent".to_string(), "variant".to_string()]).await.expect("named");
    assert_eq!(named.len(), 2);
    // A selection by label is a semijoin over stored rows: it selects stored definitions only.
    let selected = templates.list_selected(flotilla_store::StoreQuery::labels(&labelled, true, false)).await.expect("selected");
    assert_eq!(selected.into_iter().map(|template| template.metadata.name).collect::<Vec<_>>(), ["variant"]);
    // Writes under a built-in's name are refused, whatever the path.
    let refused = templates.apply(&InputMeta::builder().name("single-agent".to_string()).build(), &user).await;
    assert!(matches!(refused, Err(ResourceError::Invalid { .. })), "{refused:?}");
    let created = templates.create(&InputMeta::builder().name("implement-review".to_string()).build(), &user).await;
    assert!(matches!(created, Err(ResourceError::Invalid { .. })), "{created:?}");
    assert!(templates.delete("single-agent").await.is_err());
    assert!(templates.update_metadata(&InputMeta::builder().name("single-agent".to_string()).build()).await.is_err());
    // A copy an earlier generation seeded under a built-in's name is shadowed by the built-in.
    let mut stale = flotilla_resources::single_agent_workflow_spec();
    stale.roles.clear();
    let stale = serde_json::to_value(&stale).expect("stale spec");
    let seeded = InputMeta::builder().name("single-agent-shepherd".to_string()).build();
    backend.store().create_raw(ResourceKind::of::<WorkflowTemplate>(), "flotilla", &seeded, &stale).await.expect("a legacy stored copy");
    let shadowed = templates.get("single-agent-shepherd").await.expect("read");
    assert_eq!(shadowed.spec, flotilla_resources::single_agent_shepherd_workflow_spec());
    assert_eq!(
        templates.list().await.expect("list").iter().filter(|template| template.metadata.name == "single-agent-shepherd").count(),
        1
    );
    // The stdlib image layers read the same way.
    let layers = backend.definitions::<ImageLayer>("flotilla");
    let layer_names = builtin_names("ImageLayer");
    assert!(!layer_names.is_empty(), "the release ships stdlib layers");
    assert!(layers.get(&layer_names[0]).await.is_ok());
    assert_eq!(layers.list().await.expect("list").len(), layer_names.len());
}
