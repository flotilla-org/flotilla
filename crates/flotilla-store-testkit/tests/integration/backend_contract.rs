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
