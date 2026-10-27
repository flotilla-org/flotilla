//! Both carriers exercise the same router scenario without binding a listener.
use super::{table_api, CallerAuth, HttpRouter, LoginStore};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use flotilla_protocol::table::Message;
use flotilla_resources::{Convoy, ConvoySpec, InputMeta};
use flotilla_store::{ReadObserver, ReadOperation, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

#[derive(Debug, Default)]
struct Reads(Mutex<Vec<(ReadOperation, usize)>>);
impl ReadObserver for Reads {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn read(&self, _kind: &str, operation: ReadOperation, objects: usize) {
        self.0.lock().unwrap().push((operation, objects));
    }
}
fn router(backend: ResourceBackend) -> Router {
    HttpRouter::builder()
        .backend(backend)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router()
}
async fn seed(backend: &ResourceBackend, name: &str, label: &str) {
    backend
        .using::<Convoy>("test")
        .create(
            &InputMeta::builder().name(name.into()).labels(std::collections::BTreeMap::from([("group".into(), label.into())])).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).build(),
        )
        .await
        .unwrap();
}
async fn call(router: &Router, path: &str, cbor: bool) -> axum::response::Response {
    let mut request = Request::builder()
        .uri(path)
        .header("accept", if cbor { "application/cbor-seq" } else { "application/json" })
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(CallerAuth::Bearer);
    router.clone().oneshot(request).await.unwrap()
}
fn decode(bytes: &[u8], cbor: bool) -> Vec<Message> {
    if !cbor {
        return bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
    }
    // Find each self-delimiting item through the profile decoder. No framing bytes are
    // permitted: this deliberately decodes the RFC 8742 sequence, not length-prefixed frames.
    let mut remaining = bytes;
    let mut messages = vec![];
    while !remaining.is_empty() {
        let (size, message) = (1..=remaining.len())
            .find_map(|size| {
                flotilla_cbor::from_slice::<Message>(&remaining[..size], &Default::default()).ok().map(|message| (size, message))
            })
            .expect("complete CBOR item");
        messages.push(message);
        remaining = &remaining[size..];
    }
    messages
}
async fn error_body(response: axum::response::Response) -> serde_json::Value {
    let cbor = response.headers().get("content-type").is_some_and(|value| value == "application/cbor");
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    if cbor {
        flotilla_cbor::from_slice(&bytes, &Default::default()).unwrap()
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}
async fn messages(router: &Router, path: &str, cbor: bool) -> Vec<Message> {
    let response = call(router, path, cbor).await;
    assert_eq!(response.status(), StatusCode::OK);
    decode(&to_bytes(response.into_body(), 8 * 1024 * 1024).await.unwrap(), cbor)
}

// ADR 0067: listing, typed descriptions, projection, sorting, count, portable keyset
// pages and resync have identical decoded meaning in JSON Lines and CBOR sequences.
#[tokio::test]
async fn table_carriers_share_paging_filter_count_and_resync_contract() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    for (name, label) in [("a", "yes"), ("z", "no"), ("b", "yes")] {
        seed(&backend, name, label).await;
    }
    let reads = Arc::new(Reads::default());
    let router = router(backend.with_read_observer(reads.clone()));
    let listed = call(&router, "/tables", false).await;
    let listed: serde_json::Value = serde_json::from_slice(&to_bytes(listed.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert!(listed["kinds"].as_array().unwrap().iter().any(|kind| kind["root"] == "convoys"));
    let description = call(&router, "/tables/convoys", false).await;
    let description: serde_json::Value = serde_json::from_slice(&to_bytes(description.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(description["primaryKey"], json!(["metadata.name"]));
    let path="/tables/convoys/rows?namespace=test&where=.metadata.labels.group%20%3D%3D%20yes&limit=1&sort=-metadata.name&columns=spec&count=exact";
    let json = messages(&router, path, false).await;
    let cbor = messages(&router, path, true).await;
    assert_eq!(json, cbor);
    assert!(json.iter().any(|message| matches!(message, Message::Count { count: 2, estimate: false, .. })));
    let cursor = json
        .iter()
        .find_map(|message| {
            if let Message::Rows { rows, cursor, partial, .. } = message {
                assert!(*partial);
                assert_eq!(rows[0]["metadata.name"], "b");
                assert_eq!(rows[0].len(), 2);
                cursor.clone()
            } else {
                None
            }
        })
        .unwrap();
    for cbor in [false, true] {
        let next = messages(&router, &format!("{path}&cursor={cursor}"), cbor).await;
        assert!(next.iter().any(|message| matches!(message,Message::Rows{rows,cursor:None,..} if rows[0]["metadata.name"]=="a")));
        let reset = messages(&router, &format!("{path}&cursor=broken"), cbor).await;
        assert!(matches!(reset[0], Message::Resync { .. }));
        let contradiction = messages(&router, &format!("{path}&where=.metadata.labels.group%20%3D%3D%20no"), cbor).await;
        assert!(contradiction.iter().any(|message| matches!(message, Message::Count { count: 0, .. })));
        let offset = messages(&router, "/tables/convoys/rows?namespace=test&offset=2&count=estimate", cbor).await;
        assert!(offset.iter().any(|message| matches!(message,Message::Rows{rows,partial:true,..} if rows[0]["metadata.name"]=="z")));
        let empty = messages(&router, "/tables/convoys/rows?namespace=empty", cbor).await;
        assert!(empty.iter().any(|message| matches!(message,Message::Rows{rows,partial:false,..} if rows.is_empty())));
    }
    // The indexed store read returned two matches, rather than listing three and filtering.
    assert!(reads.0.lock().unwrap().iter().any(|(operation, count)| *operation == ReadOperation::Query && *count == 2));
    reads.0.lock().unwrap().clear();
    let refused = call(&router, "/tables/convoys/rows?namespace=test&where=.metadata.name%20%21%3D%20a", false).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let refused: serde_json::Value = serde_json::from_slice(&to_bytes(refused.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(refused["code"], "unsupported_filter");
    assert!(reads.0.lock().unwrap().is_empty());
}

// Table reads inherit the exact user-caller gate of the resource routes.
#[tokio::test]
async fn tables_do_not_bypass_resource_caller_access() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    seed(&backend, "private", "yes").await;
    let router = router(backend);
    for path in ["/tables", "/tables/convoys", "/tables/convoys/rows?namespace=test", "/apis/flotilla.work/v1/namespaces/test/convoys"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        assert_eq!(router.clone().oneshot(request).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(CallerAuth::Environment { environment: flotilla_protocol::EnvironmentId::new("env-work"), peer: None });
        assert_eq!(router.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
    }
}

// The snapshot boundary hands off without a listener; creates and deletes after synced
// emit the same natural keys and sequence numbers on both carriers.
#[tokio::test]
async fn table_watch_delivers_live_changes_on_both_carriers() {
    for cbor in [false, true] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        seed(&backend, "before", "yes").await;
        let router = router(backend.clone());
        let response = call(&router, "/tables/convoys/rows?namespace=test&watch&count=exact", cbor).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        for _ in 0..4 {
            stream.next().await.unwrap().unwrap();
        }
        seed(&backend, "after", "yes").await;
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(&decode(&bytes,cbor)[0],Message::Upsert{key,..} if key==&vec![json!("after")]));
        let bytes = stream.next().await.unwrap().unwrap();
        assert!(matches!(decode(&bytes, cbor)[0], Message::Count { count: 2, .. }));
        backend.using::<Convoy>("test").delete("after").await.unwrap();
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(&decode(&bytes,cbor)[0],Message::Delete{key,..} if key==&vec![json!("after")]));
    }
}

// ADR 0067 ordering: null sorts first ascending, and supplementary characters compare
// by Unicode code point rather than UTF-16 code units. One boundary example suffices.
#[test]
fn ordering_obeys_null_and_codepoint_contract() {
    table_api::test_ordering();
}

#[derive(flotilla_schema::Stored)]
#[store(kind = "TableSample", code = "ts")]
struct TableSample {
    #[store(intern = "tt")]
    text: String,
    number: i64,
    bytes: Vec<u8>,
    reference: Option<flotilla_schema::Ref<TableSample>>,
    #[store(child)]
    tags: Vec<String>,
}

// ADR 0067: a derived source uses its existing descriptor, resolves interned text,
// exposes every child/log table, and applies predicates inside the real SQLite store.
#[tokio::test]
async fn derived_table_router_uses_native_values_and_sql_predicates() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = flotilla_schema::KindRegistry::new();
    registry.register::<TableSample>().unwrap();
    let database = directory.path().join("table.db");
    let mut remote_registry = flotilla_schema::KindRegistry::new();
    remote_registry.register::<TableSample>().unwrap();
    let mut remote =
        flotilla_schema_sqlite::Store::open(rusqlite::Connection::open(&database).unwrap(), "remote", remote_registry).unwrap();
    let mut write = remote.write().unwrap();
    write
        .insert(
            "one",
            &TableSample { text: "remote target".into(), number: 99, bytes: vec![], reference: None, tags: vec![] },
            &flotilla_schema::NewChange::new("test", flotilla_schema::Timestamp(1)),
        )
        .unwrap();
    write.commit().unwrap();
    drop(remote);
    let service = Arc::new(flotilla_schema_sqlite::stream::Service::open(&database, "home", registry).unwrap());
    service
        .with_store(|store| {
            let mut write = store.write()?;
            for (name, text, number) in [("one", "match", 1), ("two", "other", 2)] {
                write.insert(
                    name,
                    &TableSample {
                        text: text.into(),
                        number,
                        bytes: vec![0, 255, 1],
                        reference: Some(flotilla_schema::Ref::new("one", "remote")),
                        tags: vec!["tag".into()],
                    },
                    &flotilla_schema::NewChange::new("test", flotilla_schema::Timestamp(1)),
                )?;
            }
            write.commit()
        })
        .unwrap();
    service
        .with_store(|store| {
            let bounded = store.table_read("home", "table_sample.log", &[], 1)?;
            assert_eq!(bounded.rows.len(), 1, "SQL must apply the log limit before materializing rows");
            for (field, value) in [
                ("sequence", flotilla_schema::Value::Text("abc".into())),
                ("cause", flotilla_schema::Value::Int(3)),
                ("at", flotilla_schema::Value::Text("not-a-time".into())),
                ("sequence", flotilla_schema::Value::Null),
            ] {
                let filters = [flotilla_schema_sqlite::TableFilter::Compare { field: field.into(), operator: "==".into(), value }];
                assert!(store.table_read("home", "table_sample.log", &filters, 1).is_err(), "{field}");
            }
            let null_edge = [flotilla_schema_sqlite::TableFilter::Compare {
                field: "edge".into(),
                operator: "==".into(),
                value: flotilla_schema::Value::Null,
            }];
            assert_eq!(store.table_read("home", "table_sample.log", &null_edge, 1)?.rows.len(), 1);
            assert!(store.table_read("home", "table_sample.log", &[], 0).is_err());
            assert!(store.table_read("home", "table_sample.log", &[], 10_002).is_err());
            Ok(())
        })
        .unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let router = HttpRouter::builder()
        .backend(backend)
        .table_store(service)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router();
    let path = "/tables/table_sample/rows?where=.text%20%3D%3D%20match&count=exact";
    let json = messages(&router, path, false).await;
    assert_eq!(json, messages(&router, path, true).await);
    assert!(json.iter().any(|message|matches!(message,Message::Rows{rows,..} if rows.len()==1&&rows[0]["text"]=="match"&&rows[0]["resource.name"]=="one"&&rows[0]["number"]==1&&rows[0]["bytes"]=="AP8B"&&rows[0]["reference.home"]=="remote"&&rows[0]["reference"]=="one")));
    let projected = messages(&router, "/tables/table_sample/rows?columns=reference", true).await;
    assert!(projected
        .iter()
        .any(|message| matches!(message,Message::Rows{rows,..}if rows[0]["reference.home"]=="remote"&&rows[0]["reference"]=="one")));
    let target = messages(&router, "/tables/table_sample/rows?namespace=remote&where=.resource.name%20%3D%3D%20one", false).await;
    assert!(target.iter().any(|message|matches!(message,Message::Rows{rows,..}if rows.len()==1&&rows[0]["text"]=="remote target"&&rows[0]["resource.home"]=="remote")));
    let ranged = messages(
        &router,
        "/tables/table_sample/rows?where=.number%20%3E%3D%201&where=.number%20%3C%3D%202&where=.number%20%21%3D%201",
        false,
    )
    .await;
    assert!(ranged.iter().any(|message| matches!(message,Message::Rows{rows,..}if rows.len()==1&&rows[0]["number"]==2)));
    let log_range = messages(&router, "/tables/table_sample.log/rows?where=.sequence%20%3E%200", true).await;
    assert!(log_range.iter().any(|message| matches!(message,Message::Rows{rows,..}if !rows.is_empty())));
    let schema = call(&router, "/tables/table_sample", false).await;
    let schema: serde_json::Value = serde_json::from_slice(&to_bytes(schema.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert!(schema["foreignKeys"].as_array().unwrap().iter().any(|fk| fk["fields"] == json!(["reference.home", "reference"])
        && fk["reference"]["fields"] == json!(["resource.home", "resource.name"])
        && fk["reference"]["resource"] == "table_sample"));
    for cbor in [false, true] {
        let child = messages(&router, "/tables/table_sample_tags/rows", cbor).await;
        assert!(child.iter().any(|message| matches!(message,Message::Rows{rows,..} if rows.len()==2&&rows[0]["value"]=="tag")));
        let log = messages(&router, "/tables/table_sample.log/rows?limit=1", cbor).await;
        assert!(log.iter().any(|message| matches!(message, Message::Rows { rows, partial: true, cursor: None, .. } if rows.len() == 1)));
        assert!(log.iter().any(|message| matches!(message,Message::Rows{rows,..} if rows[0]["cause"]=="create")));
    }
    for cbor in [false, true] {
        for (path, code) in [
            ("/tables/table_sample/rows?where=.metadata.labels.group%20%3D%3D%20match", "unsupported_filter"),
            ("/tables/table_sample_tags/rows?where=.metadata.labels.group%20%3D%3D%20match", "unsupported_filter"),
            ("/tables/table_sample/rows?sort=bytes", "unsupported_sort"),
            ("/tables/table_sample.log/rows?watch", "unsupported_watch"),
            ("/tables/table_sample.log/rows?count=exact", "unsupported_log_paging"),
            ("/tables/table_sample.log/rows?cursor=anything", "unsupported_log_paging"),
            ("/tables/table_sample.log/rows?offset=1", "unsupported_log_paging"),
            ("/tables/table_sample.log/rows?sort=-sequence", "unsupported_log_paging"),
        ] {
            let response = call(&router, path, cbor).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let body = error_body(response).await;
            assert_eq!(body["code"], code, "{path}");
        }
    }
    let empty =
        messages(&router, "/tables/table_sample/rows?where=.text%20%3D%3D%20match&where=.text%20%3D%3D%20other&count=exact", false).await;
    assert!(empty.iter().any(|message| matches!(message, Message::Count { count: 0, .. })));
    let markdown = call(&router, "/tables/table_sample?format=markdown", false).await;
    assert_eq!(markdown.status(), StatusCode::OK);
    assert!(String::from_utf8(to_bytes(markdown.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap().contains("table_sample"));
}

// ADR 0067 renderings come from MachineDef and committed log entries, and primitive
// rows describe and return the same natural values on both HTTP carriers.
#[tokio::test]
async fn primitive_diagrams_describe_declared_and_observed_transitions() {
    use flotilla_state_machines::{
        machines::agent_session::{AgentSession, End, EndReason, MemoryAgentSessionWorld},
        Home,
    };
    let directory = tempfile::tempdir().unwrap();
    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(
            &directory.path().join("primitives.db"),
            "home",
            flotilla_engine_store::registry().unwrap(),
        )
        .unwrap(),
    );
    let mut sessions =
        Home::<AgentSession, flotilla_engine_store::StoreLog>::with_log("home", flotilla_engine_store::StoreLog::new(service.store()))
            .unwrap();
    let mut world = MemoryAgentSessionWorld::default();
    let observed = flotilla_state_machines::Ref::new("end");
    world.ends.put(&observed, EndReason::Completed);
    sessions.create("session", (), &world, flotilla_schema::Timestamp(1)).unwrap();
    sessions.apply("session", End { reason: EndReason::Completed, observed }, &world, flotilla_schema::Timestamp(2)).unwrap();
    let router = HttpRouter::builder()
        .backend(ResourceBackend::new(InMemoryBackend::default()))
        .table_store(service)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router();
    for format in ["mermaid", "observed"] {
        let response = call(&router, &format!("/tables/agent_session?format={format}&name=session&namespace=default"), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let text = String::from_utf8(to_bytes(response.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap();
        assert!(text.contains("Live --> Ended"), "{text}");
        if format == "observed" {
            assert!(text.contains(": 1"));
        }
    }
    for cbor in [false, true] {
        let response = call(&router, "/tables/agent_session/rows?watch", cbor).await;
        let mut stream = response.into_body().into_data_stream();
        for _ in 0..3 {
            stream.next().await.unwrap().unwrap();
        }
        let name = if cbor { "new-cbor" } else { "new-json" };
        sessions.create(name, (), &world, flotilla_schema::Timestamp(3)).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(&decode(&event,cbor)[0],Message::Upsert{row,..} if row["resource.name"]==name));
    }
    let json = messages(&router, "/tables/agent_session/rows", false).await;
    assert_eq!(json, messages(&router, "/tables/agent_session/rows", true).await);
    assert!(json
        .iter()
        .any(|message| matches!(message,Message::Rows{rows,..}if rows.len()==3&&rows.iter().any(|row|row["resource.name"]=="session"))));
}

// A quiet watch proves liveness without requiring any resource mutation.
#[tokio::test(start_paused = true)]
async fn quiet_table_watch_sends_progress() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let router = router(backend);
    let response = call(&router, "/tables/convoys/rows?namespace=test&watch", false).await;
    let mut stream = response.into_body().into_data_stream();
    for _ in 0..3 {
        stream.next().await.unwrap().unwrap();
    }
    let heartbeat = stream.next().await.unwrap().unwrap();
    assert!(matches!(decode(&heartbeat, false)[0], Message::Progress { sequence: 0 }));
}

// Unknown columns, incompatible types, unsupported pushdown, invalid paging and
// unacceptable carriers are distinct public refusals, independent of stored rows.
#[tokio::test]
async fn table_request_errors_and_content_negotiation_are_explicit() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    for (query, code) in [
        ("where=.unknown%20%3D%3D%20x", "unknown_column"),
        ("where=.spec%20%3D%3D%20x", "type_mismatch"),
        ("where=.status.phase%20%21%3D%20x", "unsupported_filter"),
        ("limit=0", "invalid_paging"),
        ("columns=absent", "unknown_column"),
    ] {
        let response = call(&router, &format!("/tables/convoys/rows?{query}"), false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = error_body(response).await;
        assert_eq!(body["code"], code);
    }
    let response = call(&router, "/tables/convoys?format=markdown", false).await;
    let markdown = String::from_utf8(to_bytes(response.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap();
    assert!(markdown.contains("`metadata.name`"));
    assert!(!markdown.contains("`id`"));
    let mut request = Request::builder()
        .uri("/tables/convoys/rows")
        .header("accept", "application/cbor-seq;q=0, application/x-ndjson;q=0.5")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(CallerAuth::Bearer);
    assert_eq!(router.clone().oneshot(request).await.unwrap().headers()["content-type"], "application/x-ndjson");
    for (accept, status, media) in [
        ("Application/CBOR-SEQ; q = 0.5, application/json; q=0", StatusCode::OK, "application/cbor-seq"),
        ("application/x-ndjson;q=0, application/json;q=1", StatusCode::OK, "application/json"),
        ("application/json;q=no", StatusCode::NOT_ACCEPTABLE, "application/json"),
        ("Application/CBOR-SEQ; q = no", StatusCode::NOT_ACCEPTABLE, "application/cbor"),
    ] {
        let mut request = Request::builder().uri("/tables/convoys/rows").header("accept", accept).body(Body::empty()).unwrap();
        request.extensions_mut().insert(CallerAuth::Bearer);
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["content-type"], media);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let response = call(&router, "/tables/convoys/rows?format=arrow", false).await;
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
}

// Concurrent commits may overtake consumption. The live stream reconciles one
// pushed-down snapshot, so stale queued events cannot reintroduce a departed row.
#[tokio::test(start_paused = true)]
async fn table_watch_interleaving_preserves_filter_membership() {
    for cbor in [false, true] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        seed(&backend, "before", "yes").await;
        let router = router(backend.clone());
        let response =
            call(&router, "/tables/convoys/rows?namespace=test&watch&count=exact&where=.metadata.labels.group%20%3D%3D%20yes", cbor).await;
        let mut stream = response.into_body().into_data_stream();
        for _ in 0..4 {
            stream.next().await.unwrap().unwrap();
        }
        let current = backend.using::<Convoy>("test").get("before").await.unwrap();
        backend
            .using::<Convoy>("test")
            .update(
                &InputMeta::builder()
                    .name("before".into())
                    .labels(std::collections::BTreeMap::from([("group".into(), "no".into())]))
                    .build(),
                &current.metadata.resource_version,
                &current.spec,
            )
            .await
            .unwrap();
        seed(&backend, "transient", "yes").await;
        backend.using::<Convoy>("test").delete("transient").await.unwrap();
        let event = stream.next().await.unwrap().unwrap();
        assert!(matches!(&decode(&event,cbor)[0],Message::Delete{key,..}if key==&vec![json!("before")]));
        let event = stream.next().await.unwrap().unwrap();
        assert!(matches!(decode(&event, cbor)[0], Message::Count { count: 0, .. }));
        let event = stream.next().await.unwrap().unwrap();
        assert!(matches!(decode(&event, cbor)[0], Message::Progress { .. }));
    }
}

// Resource versions stay opaque strings: numeric-looking tokens must not advertise
// lexicographic sorting as though it were version ordering (review finding 1).
#[tokio::test]
async fn resource_version_sort_is_explicitly_unsupported() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    for number in 0..12 {
        seed(&backend, &format!("row-{number}"), "yes").await;
    }
    let router = router(backend);
    let response = call(&router, "/tables/convoys/rows?namespace=test&sort=metadata.resourceVersion", false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = error_body(response).await;
    assert_eq!(body["code"], "unsupported_sort");
    let response = call(&router, "/tables/convoys", false).await;
    let schema: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(
        schema["fields"].as_array().unwrap().iter().find(|field| field["name"] == "metadata.resourceVersion").unwrap()["sortable"],
        false
    );
}

// Paging bounds only the initial page. A table watch follows the entire filtered set;
// positioned windows belong to ADR 0067 view subscriptions (review finding 5).
#[tokio::test]
async fn table_watch_tracks_rows_outside_its_initial_page() {
    for cbor in [false, true] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        seed(&backend, "a", "yes").await;
        seed(&backend, "z", "yes").await;
        let router = router(backend.clone());
        let response = call(&router, "/tables/convoys/rows?namespace=test&limit=1&watch", cbor).await;
        let mut stream = response.into_body().into_data_stream();
        stream.next().await.unwrap().unwrap();
        let page = stream.next().await.unwrap().unwrap();
        assert!(matches!(&decode(&page,cbor)[0],Message::Rows{rows,partial:true,..}if rows.len()==1&&rows[0]["metadata.name"]=="a"));
        stream.next().await.unwrap().unwrap();
        let current = backend.using::<Convoy>("test").get("z").await.unwrap();
        backend
            .using::<Convoy>("test")
            .update(
                &InputMeta::builder()
                    .name("z".into())
                    .labels(std::collections::BTreeMap::from([("group".into(), "changed".into())]))
                    .build(),
                &current.metadata.resource_version,
                &current.spec,
            )
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(&decode(&event,cbor)[0],Message::Upsert{row,..}if row["metadata.name"]=="z"));
    }
}
