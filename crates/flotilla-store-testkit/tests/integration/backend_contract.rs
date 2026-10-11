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
use flotilla_store_testkit::{fixtures::contract::*, ReadCountsBackendExt};
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
