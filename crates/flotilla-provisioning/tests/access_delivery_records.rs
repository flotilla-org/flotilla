//! AccessDelivery's reads and writes through the store's typed paths: one contract, run against
//! the old store and against the engine's records installed behind `[primitives]`, so the access
//! layer's writers and readers keep their behaviour on either.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_provisioning::engine::access_deliveries::EngineAccessDeliveries;
use flotilla_resources::{
    access_delivery::{access_key, DeliveredNeed, DeliveryMechanism, DeliveryRetry},
    AccessDelivery, AccessDeliverySpec, Clock, InputMeta, SystemClock, CONVOY_LABEL,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn spec(convoy: &str, home: &str) -> AccessDeliverySpec {
    AccessDeliverySpec {
        access: access_key("flotilla", convoy, "implement", "coder"),
        place: "env-a".into(),
        harness: "claude-code".into(),
        home: home.into(),
        needs: BTreeMap::from([(
            "home".to_owned(),
            DeliveredNeed { mechanism: DeliveryMechanism::AgentPush, digest: "ab12".into(), at: Timestamp::ZERO },
        )]),
        retry: None,
    }
}

fn meta(name: &str, convoy: &str) -> InputMeta {
    InputMeta::builder().name(name.into()).labels(BTreeMap::from([(CONVOY_LABEL.to_owned(), convoy.to_owned())])).build()
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend) {
    let records = backend.using::<AccessDelivery>(NS);
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "AccessDelivery").await.expect("watch");

    // A launch creates the record; every read finds it with its spec.
    let created = records.create(&meta("access-a", "demo"), &spec("demo", "/home/a")).await.expect("create");
    records.create(&meta("access-b", "elsewhere"), &spec("elsewhere", "/home/b")).await.expect("create another");
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");
    assert_eq!(backend.including_replicas::<AccessDelivery>(NS).get("access-a").await.expect("read").object.spec, spec("demo", "/home/a"));
    assert!(records.create(&meta("access-a", "demo"), &spec("demo", "/home/a")).await.is_err());

    // A convoy's records are found by its label, which explain and the retire pass select by.
    let by_convoy = BTreeMap::from([(CONVOY_LABEL.to_owned(), "demo".to_owned())]);
    let found = backend
        .query_declared::<AccessDelivery>(NS, &StoreQuery { labels: by_convoy, include_replicas: true, ..Default::default() })
        .await
        .expect("query")
        .items
        .into_iter()
        .map(|item| item.object.metadata.name)
        .collect::<Vec<_>>();
    assert_eq!(found, ["access-a"]);
    assert_eq!(records.get("access-a").await.expect("get").metadata.labels.get(CONVOY_LABEL).map(String::as_str), Some("demo"));

    // A place's records are found by the place.
    let by_place = StoreQuery { fields: BTreeMap::from([("spec.place".into(), "env-a".into())]), ..Default::default() };
    assert_eq!(records.query_declared(&by_place).await.expect("query").items.len(), 2);

    // A delivery that failed records its retry; a write at a version that moved on is refused.
    let mut failed = spec("demo", "/home/a");
    failed.retry = Some(DeliveryRetry { need: "home".into(), attempts: 1, error: "refused".into(), next_at: Timestamp::MAX });
    records.update(&meta("access-a", "demo"), &created.metadata.resource_version, &failed).await.expect("update");
    assert_eq!(records.get("access-a").await.expect("get").spec, failed);
    assert!(records.update(&meta("access-a", "demo"), &created.metadata.resource_version, &spec("demo", "/home/a")).await.is_err());

    // The retire pass deletes a record; it is gone from every read.
    records.delete("access-a").await.expect("delete");
    assert!(records.get("access-a").await.is_err());
    assert!(backend.including_replicas::<AccessDelivery>(NS).get("access-a").await.is_err());
    assert_eq!(records.list().await.expect("list").items.len(), 1);
}

#[tokio::test]
async fn the_old_store_keeps_the_access_delivery_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn the_engine_keeps_the_access_delivery_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let service = Arc::new(
        Service::open(&directory.path().join("decided.sqlite"), "alpha", flotilla_resources::access_delivery::registry().expect("kinds"))
            .expect("store"),
    );
    let records = EngineAccessDeliveries::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    contract(&backend).await;
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<AccessDelivery>(), NS).await.expect("old rows").items.is_empty());
}
