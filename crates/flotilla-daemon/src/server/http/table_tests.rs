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
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
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
/// A `where` condition as a query value, every byte but a letter or digit percent-encoded.
fn condition(text: &str) -> String {
    text.bytes().map(|byte| if byte.is_ascii_alphanumeric() { char::from(byte).to_string() } else { format!("%{byte:02X}") }).collect()
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
    let path = format!(
        "/tables/convoys/rows?namespace=test&where={}&limit=1&sort=-metadata.name&columns=spec&count=exact",
        condition(r#"metadata.labels["group"] == "yes""#)
    );
    let path = path.as_str();
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
        let contradiction = messages(&router, &format!("{path}&where={}", condition(r#"metadata.labels["group"] == "no""#)), cbor).await;
        assert!(contradiction.iter().any(|message| matches!(message, Message::Count { count: 0, .. })));
        let offset = messages(&router, "/tables/convoys/rows?namespace=test&offset=2&count=estimate", cbor).await;
        assert!(offset.iter().any(|message| matches!(message,Message::Rows{rows,partial:true,..} if rows[0]["metadata.name"]=="z")));
        let empty = messages(&router, "/tables/convoys/rows?namespace=empty", cbor).await;
        assert!(empty.iter().any(|message| matches!(message,Message::Rows{rows,partial:false,..} if rows.is_empty())));
    }
    // The indexed store read returned two matches, rather than listing three and filtering.
    assert!(reads.0.lock().unwrap().iter().any(|(operation, count)| *operation == ReadOperation::Query && *count == 2));
    reads.0.lock().unwrap().clear();
    // #4108: the former spelling is refused before any read.
    let refused = call(&router, "/tables/convoys/rows?namespace=test&where=.metadata.name%20%21%3D%20a", false).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let refused: serde_json::Value = serde_json::from_slice(&to_bytes(refused.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(refused["code"], "invalid_filter");
    assert!(reads.0.lock().unwrap().is_empty());
    // What the store does not index is evaluated over the rows it returns, before paging.
    let path = format!("/tables/convoys/rows?namespace=test&where={}&count=exact", condition(r#"metadata.name != "a""#));
    let others = messages(&router, &path, false).await;
    assert!(others.iter().any(|message| matches!(message, Message::Count { count: 2, .. })));
    assert!(others
        .iter()
        .any(|message| matches!(message, Message::Rows { rows, .. } if rows.iter().all(|row| row["metadata.name"] != "a"))));
}

// #4108: a resource kind evaluates what its store does not index over every row it reads, before
// paging, so a selective in-memory filter still fills each page and its cursor walks the rest. A
// null cell is absent: `!=` does not keep it (the condition is unknown), and `!has` selects it.
#[tokio::test]
async fn in_memory_filters_page_fully_and_read_null_cells_as_absent() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    for name in ["a", "b", "c", "d", "e"] {
        seed(&backend, name, "yes").await;
    }
    let router = router(backend);
    let names = |messages: &[Message]| -> Vec<serde_json::Value> {
        messages
            .iter()
            .flat_map(|message| if let Message::Rows { rows, .. } = message { rows.clone() } else { vec![] })
            .map(|row| row["metadata.name"].clone())
            .collect()
    };
    let selective = condition(r#"metadata.name in ["b", "d", "e"]"#);
    let mut seen = Vec::new();
    let mut path = format!("/tables/convoys/rows?namespace=test&where={selective}&limit=2");
    loop {
        let page = messages(&router, &path, false).await;
        let rows = names(&page);
        let cursor = page.iter().find_map(|message| if let Message::Rows { cursor, .. } = message { cursor.clone() } else { None });
        if cursor.is_some() {
            assert_eq!(rows.len(), 2, "a page with more to come is full");
        }
        seen.extend(rows);
        match cursor {
            Some(cursor) => path = format!("/tables/convoys/rows?namespace=test&where={selective}&limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen, [json!("b"), json!("d"), json!("e")]);
    let unequal =
        messages(&router, &format!("/tables/convoys/rows?namespace=test&where={}", condition(r#"status.phase != "Running""#)), false).await;
    assert!(names(&unequal).is_empty(), "a null phase is unknown under !=");
    let missing = messages(&router, &format!("/tables/convoys/rows?namespace=test&where={}", condition("!has(status.phase)")), false).await;
    assert_eq!(names(&missing).len(), 5);
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
            let bounded = store.table_read("home", "table_sample.log", None, 1)?;
            assert_eq!(bounded.rows.len(), 1, "SQL must apply the log limit before materializing rows");
            let fields = bounded.descriptor.filterable();
            let check =
                |text: &str| rigging_condition::Checked::new(rigging_condition::Condition::parse_strict(text).unwrap(), fields.schema());
            for text in [r#"sequence == "abc""#, "cause == 3", r#"at == "not-a-time""#, r#"cause == "Nope""#] {
                assert!(check(text).is_err(), "{text}");
            }
            let null_edge = check("!has(edge)").unwrap();
            assert_eq!(store.table_read("home", "table_sample.log", Some(&null_edge), 1)?.rows.len(), 1);
            assert!(store.table_read("home", "table_sample.log", None, 0).is_err());
            assert!(store.table_read("home", "table_sample.log", None, 10_002).is_err());
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
    let path = format!("/tables/table_sample/rows?where={}&count=exact", condition(r#"text == "match""#));
    let json = messages(&router, &path, false).await;
    assert_eq!(json, messages(&router, &path, true).await);
    assert!(json.iter().any(|message|matches!(message,Message::Rows{rows,..} if rows.len()==1&&rows[0]["text"]=="match"&&rows[0]["resource.name"]=="one"&&rows[0]["number"]==1&&rows[0]["bytes"]=="AP8B"&&rows[0]["reference.home"]=="remote"&&rows[0]["reference"]=="one")));
    let projected = messages(&router, "/tables/table_sample/rows?columns=reference", true).await;
    assert!(projected
        .iter()
        .any(|message| matches!(message,Message::Rows{rows,..}if rows[0]["reference.home"]=="remote"&&rows[0]["reference"]=="one")));
    let target =
        messages(&router, &format!("/tables/table_sample/rows?namespace=remote&where={}", condition(r#"resource.name == "one""#)), false)
            .await;
    assert!(target.iter().any(|message|matches!(message,Message::Rows{rows,..}if rows.len()==1&&rows[0]["text"]=="remote target"&&rows[0]["resource.home"]=="remote")));
    let ranged = messages(
        &router,
        &format!("/tables/table_sample/rows?where={}&where={}", condition("number >= 1 && number <= 2"), condition("number != 1")),
        false,
    )
    .await;
    assert!(ranged.iter().any(|message| matches!(message,Message::Rows{rows,..}if rows.len()==1&&rows[0]["number"]==2)));
    let log_range = messages(&router, &format!("/tables/table_sample.log/rows?where={}", condition("sequence > 0")), true).await;
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
        assert!(log.iter().any(|message| matches!(message,Message::Rows{rows,..} if rows[0]["cause"]=="Create")));
    }
    for cbor in [false, true] {
        let labels = condition(r#"metadata.labels["group"] == "match""#);
        for (path, code) in [
            (format!("/tables/table_sample/rows?where={labels}"), "unknown_column"),
            (format!("/tables/table_sample_tags/rows?where={labels}"), "unknown_column"),
            (format!("/tables/table_sample/rows?where={}", condition(r#"bytes == "AP8B""#)), "unsupported_filter"),
            (format!("/tables/table_sample/rows?where={}", condition(r#"text.under("project:flotilla")"#)), "type_mismatch"),
        ]
        .into_iter()
        .chain(
            [
                ("/tables/table_sample/rows?sort=bytes", "unsupported_sort"),
                ("/tables/table_sample.log/rows?watch", "unsupported_watch"),
                ("/tables/table_sample.log/rows?count=exact", "unsupported_log_paging"),
                ("/tables/table_sample.log/rows?cursor=anything", "unsupported_log_paging"),
                ("/tables/table_sample.log/rows?offset=1", "unsupported_log_paging"),
                ("/tables/table_sample.log/rows?sort=-sequence", "unsupported_log_paging"),
            ]
            .map(|(path, code)| (path.to_owned(), code)),
        ) {
            let path = path.as_str();
            let response = call(&router, path, cbor).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let body = error_body(response).await;
            assert_eq!(body["code"], code, "{path}");
        }
    }
    let empty = messages(
        &router,
        &format!(
            "/tables/table_sample/rows?where={}&where={}&count=exact",
            condition(r#"text == "match""#),
            condition(r#"text == "other""#)
        ),
        false,
    )
    .await;
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

async fn json(router: &Router, path: &str) -> serde_json::Value {
    let response = call(router, path, false).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

// #4214: every table the daemon serves types its address columns `address` and declares each
// enum column's value set as `constraints.enum`, the column's readable names in code order; a
// rows read of each table carries only names from that set, and `where` compares names. Message's
// conditions share the address domain but are no addresses.
#[tokio::test]
async fn served_tables_type_addresses_and_carry_enum_names() {
    use flotilla_schema::{ColumnType, ADDRESS_DOMAIN};
    use flotilla_state_machines::{
        machines::agent_session::{AgentSession, End, EndReason, MemoryAgentSessionWorld},
        Home,
    };
    let directory = tempfile::tempdir().unwrap();
    let registry = crate::runtime::decided_registry().unwrap();
    let service =
        Arc::new(flotilla_schema_sqlite::stream::Service::open(&directory.path().join("served.db"), "home", registry.clone()).unwrap());
    let mut sessions =
        Home::<AgentSession, flotilla_engine_store::StoreLog>::with_log("home", flotilla_engine_store::StoreLog::new(service.store()))
            .unwrap();
    let mut world = MemoryAgentSessionWorld::default();
    let observed = flotilla_state_machines::Ref::new("end");
    world.ends.put(&observed, EndReason::Completed);
    sessions.create("ended", (), &world, flotilla_schema::Timestamp(1)).unwrap();
    sessions.apply("ended", End { reason: EndReason::Completed, observed }, &world, flotilla_schema::Timestamp(2)).unwrap();
    sessions.create("live", (), &world, flotilla_schema::Timestamp(3)).unwrap();
    let router = HttpRouter::builder()
        .backend(ResourceBackend::new(InMemoryBackend::default()))
        .table_store(service)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router();
    let listed = json(&router, "/tables").await;
    let tables: Vec<String> = listed["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|kind| kind["tables"].as_array().unwrap().iter().map(|table| table.as_str().unwrap().to_owned()))
        .collect();
    let mut addresses = Vec::new();
    for table in &tables {
        let stored = registry.iter().find_map(|set| {
            set.tables
                .iter()
                .find(|stored| stored.name == *table)
                .cloned()
                .or_else(|| (set.log_projection().name == *table).then(|| set.log_projection()))
        });
        let schema = json(&router, &format!("/tables/{table}")).await;
        let mut enums = BTreeMap::new();
        for field in schema["fields"].as_array().unwrap() {
            let name = field["name"].as_str().unwrap();
            let column = stored.as_ref().and_then(|stored| stored.key.iter().chain(&stored.columns).find(|column| column.name == name));
            // #4108: every scalar column is filterable; none is dropped for a path it cannot spell.
            // A resource kind's labels are the one object filtered, as a map.
            let filterable = (field["type"].as_str().is_some_and(|ty| !["object", "array"].contains(&ty)) && field["format"] != "binary")
                || name == "metadata.labels";
            assert_eq!(
                field["whereOperators"].as_array().is_some_and(|ops| !ops.is_empty()),
                filterable,
                "{table}.{name} whereOperators: {field}"
            );
            if field["type"] == "address" {
                assert_eq!(column.map(|column| &column.ty), Some(&ColumnType::Interned(ADDRESS_DOMAIN)), "{table}.{name}");
                addresses.push(format!("{table}.{name}"));
            }
            match column.map(|column| (&column.ty, &column.categories)) {
                Some((ColumnType::Code(_), categories)) => {
                    let names: Vec<_> = categories.iter().map(|(_, name)| json!(name)).collect();
                    assert_eq!(field["constraints"]["enum"], json!(names), "{table}.{name}");
                    enums.insert(name.to_owned(), names);
                }
                // A derived table has no stored columns; its declared value set is what its rows must use.
                _ if stored.is_none() => {
                    if let Some(names) = field["constraints"]["enum"].as_array() {
                        enums.insert(name.to_owned(), names.clone());
                    }
                }
                _ => assert!(field["constraints"].get("enum").is_none(), "{table}.{name} is no enum"),
            }
        }
        for message in messages(&router, &format!("/tables/{table}/rows"), false).await {
            let Message::Rows { rows, .. } = message else { continue };
            for row in rows {
                for (name, names) in &enums {
                    assert!(row[name].is_null() || names.contains(&row[name]), "{table}.{name}: {} is not one of {names:?}", row[name]);
                }
            }
        }
    }
    for column in ["inbox", "sender", "reason_by", "reason_authority"] {
        assert!(addresses.contains(&format!("message.{column}")), "message.{column} in {addresses:?}");
    }
    assert!(!addresses.iter().any(|address| address.ends_with("_condition")), "{addresses:?}");
    let rows = |messages: Vec<Message>| -> Vec<flotilla_protocol::table::Row> {
        messages.into_iter().flat_map(|message| if let Message::Rows { rows, .. } = message { rows } else { vec![] }).collect()
    };
    let ended = rows(messages(&router, &format!("/tables/agent_session/rows?where={}", condition(r#"state == "Ended""#)), false).await);
    assert_eq!(
        ended.iter().map(|row| (row["resource.name"].clone(), row["state"].clone(), row["reason"].clone())).collect::<Vec<_>>(),
        [(json!("ended"), json!("Ended"), json!("Completed"))]
    );
    let response = call(&router, &format!("/tables/agent_session/rows?where={}", condition(r#"state == "en""#)), false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_body(response).await["code"], "type_mismatch", "a stored code is no wire value");
    let log = rows(messages(&router, "/tables/agent_session.log/rows", false).await);
    let entries: Vec<_> =
        log.iter().map(|row| (row["cause"].clone(), row["edge"].clone(), row["from"].clone(), row["to"].clone())).collect();
    assert!(entries.contains(&(json!("Create"), json!(null), json!(null), json!(null))), "{entries:?}");
    assert!(entries.contains(&(json!("Edge"), json!("End"), json!("Live"), json!("Ended"))), "{entries:?}");
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
        (format!("where={}", condition(r#"unknown == "x""#)), "unknown_column"),
        (format!("where={}", condition(r#"spec == "x""#)), "unsupported_filter"),
        (format!("where={}", condition(r#"metadata.creationTimestamp == "x""#)), "type_mismatch"),
        (format!("where={}", condition(r#"status.phase < 3"#)), "type_mismatch"),
        (format!("where={}", condition("status.phase == Running")), "invalid_filter"),
        ("where=.status.phase%20%3D%3D%20x".into(), "invalid_filter"),
        ("limit=0".into(), "invalid_paging"),
        ("columns=absent".into(), "unknown_column"),
    ] {
        let response = call(&router, &format!("/tables/convoys/rows?{query}"), false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = error_body(response).await;
        assert_eq!(body["code"], code, "{query}");
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
        // Streamed rows and refusals alike vary by Accept.
        assert_eq!(response.headers()["vary"], "Accept");
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
        let response = call(
            &router,
            &format!("/tables/convoys/rows?namespace=test&watch&count=exact&where={}", condition(r#"metadata.labels["group"] == "yes""#)),
            cbor,
        )
        .await;
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

const TABLES_DOC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/tables.md"));

fn documented(list: &str) -> Vec<&'static str> {
    let opening = format!("```list {list}\n");
    let body = TABLES_DOC.split_once(&opening).unwrap_or_else(|| panic!("docs/tables.md has no `list {list}` block")).1;
    body.split_once("```").expect("list block is closed").0.lines().collect()
}

fn sorted(mut names: Vec<&str>) -> Vec<&str> {
    names.sort();
    names
}

#[test]
fn tables_doc_lists_the_message_kinds_of_the_code() {
    fn kind(message: &Message) -> &'static str {
        match message {
            Message::Table { .. } => "table",
            Message::Rows { .. } => "rows",
            Message::Synced { .. } => "synced",
            Message::Upsert { .. } => "upsert",
            Message::Delete { .. } => "delete",
            Message::Progress { .. } => "progress",
            Message::Resync { .. } => "resync",
            Message::Count { .. } => "count",
        }
    }
    let samples = [
        Message::Table { table: String::new(), schema: json!({}), incarnation: String::new(), sequence: 0 },
        Message::Rows { rows: vec![], sequence: 0, partial: false, cursor: None },
        Message::Synced { sequence: 0 },
        Message::Upsert { key: vec![], row: Default::default(), sequence: 0 },
        Message::Delete { key: vec![], sequence: 0 },
        Message::Progress { sequence: 0 },
        Message::Resync { reason: String::new() },
        Message::Count { count: 0, estimate: false, sequence: 0 },
    ];
    for sample in &samples {
        assert_eq!(serde_json::to_value(sample).unwrap()["type"], kind(sample));
    }
    assert_eq!(sorted(samples.iter().map(kind).collect()), sorted(documented("messages")));
}

#[test]
fn tables_doc_lists_the_column_types_of_the_code() {
    use flotilla_schema::ColumnType;
    fn name(ty: &ColumnType) -> &'static str {
        match ty {
            ColumnType::Integer => "integer",
            ColumnType::Number => "number",
            ColumnType::Bool => "bool",
            ColumnType::Text => "text",
            ColumnType::Blob => "blob",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Code(_) => "code",
            ColumnType::Interned(_) => "interned",
            ColumnType::Ref => "ref",
            ColumnType::Frozen => "frozen",
            ColumnType::Cbor => "cbor",
        }
    }
    let all = [
        ColumnType::Integer,
        ColumnType::Number,
        ColumnType::Bool,
        ColumnType::Text,
        ColumnType::Blob,
        ColumnType::Timestamp,
        ColumnType::Code(vec![]),
        ColumnType::Interned(flotilla_schema::HOME_DOMAIN),
        ColumnType::Ref,
        ColumnType::Frozen,
        ColumnType::Cbor,
    ];
    assert_eq!(sorted(all.iter().map(name).collect()), sorted(documented("column-types")));
}

// #4214: the field types and constraints the description renders, over every column type and an
// address column, are the ones `docs/tables.md` lists.
#[test]
fn tables_doc_lists_the_field_types_and_constraints_of_the_code() {
    use flotilla_schema::{ColumnType, Layout};
    let mut layout = Layout::description("sample");
    for (index, ty) in [
        ColumnType::Integer,
        ColumnType::Number,
        ColumnType::Bool,
        ColumnType::Text,
        ColumnType::Blob,
        ColumnType::Timestamp,
        ColumnType::Code(vec!["a".into()]),
        ColumnType::Interned(flotilla_schema::HOME_DOMAIN),
        ColumnType::Ref,
        ColumnType::Frozen,
        ColumnType::Cbor,
        ColumnType::Interned(flotilla_schema::ADDRESS_DOMAIN),
    ]
    .into_iter()
    .enumerate()
    {
        let address = ty == ColumnType::Interned(flotilla_schema::ADDRESS_DOMAIN);
        layout.column(&format!("c{index}"), ty, false);
        if address {
            layout.address();
        }
    }
    let schema = layout.finish().tables.remove(0).frictionless();
    let fields = schema["fields"].as_array().unwrap();
    let types: BTreeSet<&str> = fields.iter().map(|field| field["type"].as_str().unwrap()).collect();
    let constraints: BTreeSet<&str> =
        fields.iter().flat_map(|field| field["constraints"].as_object().unwrap().keys().map(String::as_str)).collect();
    assert_eq!(types, documented("field-types").into_iter().collect());
    assert_eq!(constraints, documented("constraints").into_iter().collect());
}

#[test]
fn tables_doc_lists_the_routes_of_the_code() {
    let source = include_str!("mod.rs");
    let routes: Vec<&str> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix(".route(\"")?.split('"').next())
        .filter(|path| path.starts_with("/tables"))
        .collect();
    assert_eq!(routes.len(), source.matches("\"/tables").count());
    assert_eq!(sorted(routes), sorted(documented("routes")));
}

// #4108: `where` speaks the condition language. A message's `inbox` is an address column whose
// `whereOperators` admit `under`; `under` a convoy keeps the messages of the convoy's roles,
// not a sibling convoy whose name extends it (`fix-37740`) nor the same convoy in another project.
// A page's cursor binds the canonical text, so any spelling of the same condition continues it,
// while a cursor of the former `where` (version 1, which embedded comparisons) answers `resync`.
#[tokio::test]
async fn messages_filter_under_a_convoy_address() {
    use flotilla_engine_store::MessageLog;
    use flotilla_schema::{Address, Timestamp};
    use flotilla_state_machines::{
        machines::message::{MemoryMessageWorld, Message as Mail, MessageBody, MessageSpec, Relation},
        Home,
    };
    let directory = tempfile::tempdir().unwrap();
    let registry = crate::runtime::decided_registry().unwrap();
    let service = Arc::new(flotilla_schema_sqlite::stream::Service::open(&directory.path().join("messages.db"), "home", registry).unwrap());
    let mut mail = Home::<Mail, MessageLog>::with_log("home", MessageLog::new(service.store())).unwrap();
    let world = MemoryMessageWorld::default();
    let inboxes = [
        "project:flotilla/convoy:fix-3774/role:coder",
        "project:flotilla/convoy:fix-3774/role:reviewer",
        "project:flotilla/convoy:fix-37740/role:coder",
        "project:flotilla2/convoy:fix-3774/role:coder",
    ];
    for (index, inbox) in inboxes.iter().enumerate() {
        let inbox: Address = inbox.parse().unwrap();
        let sender = "project:flotilla/role:governor".parse().unwrap();
        let spec = MessageSpec::new(sender, inbox.clone(), Relation::System, MessageBody::text("hello").unwrap());
        let name = Address::message(&inbox, &format!("m{index}")).unwrap();
        mail.create(name.as_str(), spec, &world, Timestamp(1)).unwrap();
    }
    let router = HttpRouter::builder()
        .backend(ResourceBackend::new(InMemoryBackend::default()))
        .table_store(service)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router();
    let schema = json(&router, "/tables/message").await;
    let field = |name: &str| schema["fields"].as_array().unwrap().iter().find(|field| field["name"] == name).unwrap().clone();
    assert_eq!(field("inbox")["type"], "address");
    assert_eq!(field("inbox")["whereOperators"], json!(["==", "!=", "in", "under", "has"]));
    assert_eq!(field("admitted_seq")["whereOperators"], json!(["==", "!=", "<", "<=", ">", ">=", "in", "has"]));
    assert_eq!(field("state")["whereOperators"], json!(["==", "!=", "in", "has"]));
    assert_eq!(field("body")["whereOperators"], json!([]));

    let inbox_of = |messages: &[Message]| -> Vec<String> {
        let mut inboxes: Vec<String> = messages
            .iter()
            .flat_map(|message| if let Message::Rows { rows, .. } = message { rows.clone() } else { vec![] })
            .map(|row| row["inbox"].as_str().unwrap().to_owned())
            .collect();
        inboxes.sort();
        inboxes
    };
    let under = condition(r#"inbox.under("project:flotilla/convoy:fix-3774")"#);
    let selected = messages(&router, &format!("/tables/message/rows?where={under}&count=exact"), false).await;
    assert_eq!(inbox_of(&selected), ["project:flotilla/convoy:fix-3774/role:coder", "project:flotilla/convoy:fix-3774/role:reviewer"]);
    assert!(selected.iter().any(|message| matches!(message, Message::Count { count: 2, .. })));
    let outside = condition(r#"!inbox.under("project:flotilla/convoy:fix-3774") && state == "Pending""#);
    let outside = messages(&router, &format!("/tables/message/rows?where={outside}"), true).await;
    assert_eq!(inbox_of(&outside), ["project:flotilla/convoy:fix-37740/role:coder", "project:flotilla2/convoy:fix-3774/role:coder"]);

    let first = messages(&router, &format!("/tables/message/rows?where={under}&limit=1"), false).await;
    let cursor = first.iter().find_map(|message| if let Message::Rows { cursor, .. } = message { cursor.clone() } else { None }).unwrap();
    let respelled = condition(r#"inbox.under( 'project:flotilla/convoy:fix-3774' )"#);
    let next = messages(&router, &format!("/tables/message/rows?where={respelled}&limit=1&cursor={cursor}"), false).await;
    assert!(!matches!(next[0], Message::Resync { .. }), "the canonical text binds the cursor");
    assert_eq!(inbox_of(&[first, next].concat()).len(), 2);

    let former = json!({
        "version": 1,
        "table": "message",
        "namespace": "default",
        "sort": [["resource.home", false], ["resource.name", false]],
        "filter": [{"field": "inbox", "operator": "==", "literal": "project:flotilla/convoy:fix-3774"}],
        "values": ["home", "m0"],
    });
    let former = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(flotilla_cbor::to_vec(&former, &Default::default()).unwrap())
    };
    let resumed = messages(&router, &format!("/tables/message/rows?where={under}&cursor={former}"), false).await;
    assert!(matches!(resumed[0], Message::Resync { .. }), "{resumed:?}");
    assert_eq!(inbox_of(&resumed).len(), 2, "a fresh first page follows the resync");
}

#[test]
fn tables_doc_lists_the_where_operators_of_the_code() {
    use rigging_condition::Type;
    let mut covered = BTreeSet::new();
    for line in documented("where-operators") {
        let (names, operators) = line.split_once(": ").expect("`types: operators`");
        for name in names.split(", ") {
            let ty = match name {
                "integer" => Type::Int,
                "number" => Type::Number,
                "string" => Type::String,
                "datetime" => Type::Timestamp,
                "boolean" => Type::Bool,
                "enum" => Type::enumeration(["Pending"]),
                "address" => Type::Address,
                "map" => Type::Map(Box::new(Type::String)),
                other => panic!("docs/tables.md lists an unknown type {other}"),
            };
            assert_eq!(flotilla_schema::where_operators(Some(&ty)).join(" "), operators, "{name}");
            covered.insert(name);
        }
    }
    assert_eq!(covered.len(), 8, "every filterable type is listed");
}
