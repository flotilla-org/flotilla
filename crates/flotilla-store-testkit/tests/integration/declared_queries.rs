use axum::{
    extract::{Path, State},
    http::Uri,
    routing::get,
    Json, Router,
};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    Convoy, InputMeta, Message, MessagePhase, MessageRelation, MessageSpec, MessageStatus, MessageSubmission, OwnerReference,
    ResourceProvenance,
};
use flotilla_store::{query_resource_kind, MessageQuery, OwnerQuery, ResourceBackend, StoreExt, StoreQuery};
use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use flotilla_store_testkit::fixtures::{convoy_meta, convoy_spec};
use flotilla_store_testkit::ReadCountsBackendExt;
use std::collections::BTreeMap;

// HTTP boundary stand-in: it rejects missing/invalid query parameters and
// executes the received contract against an independent real memory store.
async fn endpoint(
    State(backend): State<ResourceBackend>,
    Path((namespace, kind)): Path<(String, String)>,
    uri: Uri,
) -> Json<serde_json::Value> {
    let parameters: BTreeMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect();
    assert_eq!(parameters.len(), 1, "query protocol sends exactly storeQuery");
    let query: StoreQuery = serde_json::from_str(parameters.get("storeQuery").expect("required storeQuery")).expect("valid query");
    Json(query_resource_kind(&backend, &namespace, &kind, &query).await.expect("enforce query"))
}
async fn seed(backend: &ResourceBackend) {
    for (name, label) in [("a", "target"), ("b", "other"), ("shadow", "other")] {
        let mut meta = convoy_meta(name);
        meta.labels.insert("team".into(), label.into());
        meta.labels.insert("role".into(), "coder".into());
        meta.owner_references.push(OwnerReference {
            api_version: "flotilla.work/v1".into(),
            kind: "Project".into(),
            name: "project".into(),
            controller: false,
        });
        backend.using::<Convoy>("flotilla").create(&meta, &convoy_spec("template")).await.unwrap();
    }
    let messages = backend.using::<Message>("flotilla");
    let at = chrono::DateTime::from_timestamp(1, 0).unwrap();
    for name in ["active", "audit"] {
        let spec = MessageSpec::builder()
            .sender("flotilla/sender".into())
            .receiver("flotilla/receiver".into())
            .relation(MessageRelation::Peer)
            .body("body".into())
            .in_reply_to("parent".into())
            .build();
        let created = messages.create(&InputMeta::builder().name(name.into()).build(), &spec).await.unwrap();
        let mut status = MessageStatus::builder()
            .phase(if name == "active" { MessagePhase::Accepted } else { MessagePhase::Expired })
            .since(at)
            .reason("fixture".into())
            .build();
        if name == "active" {
            status.submission = Some(
                MessageSubmission::builder()
                    .batch_id("batch".into())
                    .crew_id("crew".into())
                    .session("session".into())
                    .started_at(at)
                    .members(vec!["active".into()])
                    .build(),
            );
        }
        messages.update_status(name, &created.metadata.resource_version, &status).await.unwrap();
    }

    let source = ResourceBackend::new(InMemoryBackend::default());
    for name in ["shadow", "remote", "0-first"] {
        let mut meta = convoy_meta(name);
        meta.labels.insert("team".into(), "target".into());
        source.using::<Convoy>("flotilla").create(&meta, &convoy_spec("template")).await.unwrap();
    }
    let listed = source.using::<Convoy>("flotilla").list().await.unwrap();
    backend.replace_replicas_typed::<Convoy>(&NodeId::new("local"), "flotilla", &listed, chrono::Utc::now(), None).await.unwrap();
}
async fn contract(backend: &ResourceBackend, authority: &ResourceBackend) {
    // Equality predicates combine with AND; empty selections return all local rows.
    let query = StoreQuery::default();
    assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 3);
    for (fields, labels, expected) in [
        (BTreeMap::new(), BTreeMap::from([("team".into(), "target".into()), ("role".into(), "coder".into())]), vec!["a"]),
        (BTreeMap::from([("metadata.name".into(), "b".into())]), BTreeMap::new(), vec!["b"]),
        (BTreeMap::new(), BTreeMap::from([("team".into(), "missing".into())]), vec![]),
    ] {
        let result = backend.query_declared::<Convoy>("flotilla", &StoreQuery { fields, labels, ..Default::default() }).await.unwrap();
        assert_eq!(result.items.iter().map(|o| o.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
    }
    // Owner identity is API version + kind + name, and undeclared fields are refused.
    assert_eq!(
        backend
            .query_declared::<Convoy>(
                "flotilla",
                &StoreQuery {
                    owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "project".into() }),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    assert!(backend
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery { fields: BTreeMap::from([("spec.unknown".into(), "x".into())]), ..Default::default() }
        )
        .await
        .is_err());
    // Local existence suppresses self-origin replicas even when local labels differ.
    let mut query = StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true);
    let result = backend.query_declared::<Convoy>("flotilla", &query).await.unwrap();
    assert_eq!(result.items.iter().map(|o| o.object.metadata.name.as_str()).collect::<Vec<_>>(), vec!["0-first", "a", "remote"]);
    assert!(result.items.iter().any(|o| matches!(o.provenance, ResourceProvenance::Replica { .. })));
    query.suppress_self_origin = false;
    assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 4);
    // MessageQuery is a case of the same vocabulary on every transport, with
    // history excluded before decoding and an exclusive audit time boundary.
    for (message, expected) in [
        (MessageQuery::active(), 1),
        (MessageQuery::Active { receiver: Some("flotilla/receiver".into()) }, 1),
        (MessageQuery::Active { receiver: Some("flotilla/other".into()) }, 0),
        (MessageQuery::ReplyTo { name: "parent".into() }, 2),
        (MessageQuery::Batch { id: "batch".into() }, 1),
        (MessageQuery::AuditBefore { before: chrono::DateTime::from_timestamp(1, 0).unwrap() }, 0),
        (MessageQuery::AuditBefore { before: chrono::DateTime::from_timestamp(2, 0).unwrap() }, 1),
    ] {
        let observed = backend.clone().with_read_counts();
        let result =
            observed.query_declared::<Message>("flotilla", &StoreQuery { message: Some(message), ..Default::default() }).await.unwrap();
        assert_eq!(result.items.len(), expected);
        assert_eq!(observed.read_counts().get("Message"), Some(&expected));
    }
    for (path, value, expected) in [
        ("spec.receiver", "flotilla/receiver", 2),
        ("spec.in_reply_to", "parent", 2),
        ("status.phase", "accepted", 1),
        ("status.submission.batch_id", "batch", 1),
    ] {
        let result = backend
            .query_declared::<Message>(
                "flotilla",
                &StoreQuery { fields: BTreeMap::from([(path.into(), value.into())]), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), expected, "declared field {path}");
    }
    assert!(backend.query_declared::<Convoy>("different", &StoreQuery::default()).await.unwrap().items.is_empty());
    assert!(backend
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery {
                owner: Some(OwnerQuery { api_version: "other/v1".into(), kind: "Project".into(), name: "project".into() }),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty());
    // Indexed queries decode only selected rows, and updates/deletes remove old
    // label, owner and field index entries in the same write transaction.
    let observed = backend.clone().with_read_counts();
    observed
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery { fields: BTreeMap::from([("metadata.name".into(), "a".into())]), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(observed.read_counts().get("Convoy"), Some(&1));
    let resolver = authority.using::<Convoy>("flotilla");
    let old = resolver.get("a").await.unwrap();
    let mut meta = flotilla_resources::InputMeta::from(&old.metadata);
    meta.labels.insert("team".into(), "changed".into());
    meta.owner_references.clear();
    resolver.update(&meta, &old.metadata.resource_version, &old.spec).await.unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), false, true))
        .await
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        backend
            .query_declared::<Convoy>(
                "flotilla",
                &StoreQuery {
                    owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "project".into() }),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    resolver.delete("a").await.unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "changed".into())]), false, true))
        .await
        .unwrap()
        .items
        .is_empty());
    // A full replacement removes absent replica names from the index.
    authority
        .replace_replicas_typed::<Convoy>(
            &NodeId::new("local"),
            "flotilla",
            &flotilla_resources::ResourceList { items: vec![], resource_version: "0".into(), generation: None },
            chrono::Utc::now(),
            None,
        )
        .await
        .unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true))
        .await
        .unwrap()
        .items
        .is_empty());
}
#[tokio::test]
async fn shared_query_contract() {
    let sqlite = ResourceBackend::new(SqliteBackend::open_in_memory().unwrap());
    let memory = ResourceBackend::new(InMemoryBackend::default());
    for backend in [&sqlite, &memory] {
        seed(backend).await;
        contract(backend, backend).await;
    }
    // Durable local names deliberately differ from the composing memory store:
    // remote exists only durably; shadow exists only in the memory local set.
    let durable = ResourceBackend::new(SqliteBackend::open_in_memory().unwrap());
    durable.using::<Convoy>("flotilla").create(&convoy_meta("remote"), &convoy_spec("template")).await.unwrap();
    durable.using::<Convoy>("flotilla").create(&convoy_meta("durable-only"), &convoy_spec("template")).await.unwrap();
    let hybrid = ResourceBackend::new(InMemoryBackend::observed_with_durable_replicas(durable));
    seed(&hybrid).await;
    contract(&hybrid, &hybrid).await;
    let server = ResourceBackend::new(InMemoryBackend::default());
    seed(&server).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/apis/flotilla.work/v1/namespaces/{namespace}/{kind}", get(endpoint)).with_state(server.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), format!("http://{addr}")));
    contract(&http, &server).await;
    task.abort();
}

// Index results agree with direct equality semantics after every generated
// insert/update/delete, including duplicate labels and empty/missing fields.
#[hegel::test]
fn inverted_index_tracks_mutations(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let mut index = flotilla_store::QueryIndex::default();
    let mut objects = BTreeMap::new();
    // Small key/value spaces deliberately cause collisions, repeated updates,
    // missing-label transitions, and repeated deletes; zero covers an empty store.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    for _ in 0..steps {
        let name = format!("n{}", tc.draw(gs::integers::<usize>().min_value(0).max_value(3)));
        if tc.draw(gs::booleans()) {
            let label = if tc.draw(gs::booleans()) { "a" } else { "b" };
            let value = serde_json::json!({"kind":"Convoy","metadata":{"name":name,"labels":{"team":label}},"status":{"phase":"running"}});
            index.update(&name, Some(&value));
            objects.insert(name, value);
        } else {
            index.update(&name, None);
            objects.remove(&name);
        }
        for label in ["a", "b", "absent"] {
            let query = StoreQuery::labels(&BTreeMap::from([("team".into(), label.into())]), false, true);
            let expected = objects.iter().filter(|(_, v)| v["metadata"]["labels"]["team"] == label).map(|(k, _)| k.clone()).collect();
            assert_eq!(index.names(&query), expected);
        }
        assert_eq!(index.names(&StoreQuery::default()), objects.keys().cloned().collect());
        for name in ["n0", "n1", "n2", "n3"] {
            let query = StoreQuery { fields: BTreeMap::from([("metadata.name".into(), name.into())]), ..Default::default() };
            let expected = objects.keys().filter(|key| key.as_str() == name).cloned().collect();
            assert_eq!(index.names(&query), expected);
        }
    }
}
