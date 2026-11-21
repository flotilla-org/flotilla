//! AccessDelivery's reads and writes through the store's typed paths, served by the engine's
//! records, the daemon's only path for the kind. The access layer's writers and readers hold to
//! one contract; it also runs against the plain in-memory store that tests stand in with.

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

    // Another namespace holds none of these records: every read and watch through it is empty.
    let elsewhere = backend.using::<AccessDelivery>("elsewhere");
    assert!(matches!(elsewhere.get("access-a").await, Err(flotilla_resources::ResourceError::NotFound { .. })));
    assert!(backend.including_replicas::<AccessDelivery>("elsewhere").get("access-a").await.is_err());
    assert!(elsewhere.list().await.expect("list").items.is_empty());
    assert!(elsewhere.query_declared(&StoreQuery::default()).await.expect("query").items.is_empty());
    let mut elsewhere_watch = watch_resource_kind_including_replicas(backend, "elsewhere", "AccessDelivery").await.expect("watch");
    assert!(elsewhere_watch.initial.is_empty(), "{:?}", elsewhere_watch.initial);
    records.create(&meta("access-c", "demo"), &spec("demo", "/home/c")).await.expect("create a third");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = watch.stream.next().await.expect("open").expect("event");
            if event["object"]["metadata"]["name"] == "access-c" {
                break;
            }
        }
    })
    .await
    .expect("the third's event");
    assert!(tokio::time::timeout(Duration::from_millis(200), elsewhere_watch.stream.next()).await.is_err(), "no event elsewhere");
    records.delete("access-c").await.expect("delete the third");

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

/// The engine serves the kind in [`NS`] alone: a create, update or delete through another namespace
/// is refused, naming it, and leaves the records as they were.
async fn writes_through_another_namespace_are_refused(backend: &ResourceBackend) {
    let records = backend.using::<AccessDelivery>(NS);
    let elsewhere = backend.using::<AccessDelivery>("elsewhere");
    let held = records.create(&meta("access-held", "demo"), &spec("demo", "/home/held")).await.expect("create");
    let refused = |result: Result<_, flotilla_resources::ResourceError>, verb: &str| match result {
        Err(flotilla_resources::ResourceError::Invalid { message }) => {
            assert_eq!(message, format!("AccessDelivery is served in namespace {NS}"), "{verb}")
        }
        other => panic!("{verb} through another namespace was not refused: {other:?}"),
    };
    refused(elsewhere.create(&meta("access-new", "demo"), &spec("demo", "/home/new")).await.map(|_| ()), "create");
    let changed = spec("demo", "/home/changed");
    refused(elsewhere.update(&meta("access-held", "demo"), &held.metadata.resource_version, &changed).await.map(|_| ()), "update");
    refused(elsewhere.delete("access-held").await, "delete");
    assert!(records.get("access-new").await.is_err(), "nothing was created");
    let still = records.get("access-held").await.expect("not deleted");
    assert_eq!((still.metadata.resource_version, still.spec), (held.metadata.resource_version, held.spec), "not changed");
    records.delete("access-held").await.expect("delete");
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
    backend.serve_engine_in(NS);
    backend.install_engine_records(Arc::new(records));
    contract(&backend).await;
    writes_through_another_namespace_are_refused(&backend).await;
    assert!(backend.store().list_raw(flotilla_store::ResourceKind::of::<AccessDelivery>(), NS).await.expect("old rows").items.is_empty());
}

// #4415: the access layer writes its deliveries through the plain kind's log, and the engine
// controller reads the ones homed here as realised lifecycles: a release removes one, again a
// no-op, and a write from a stale version is refused.
#[test]
fn the_controller_reads_this_homes_deliveries_as_realised_and_a_release_removes_one() {
    use flotilla_provisioning::engine::{access_deliveries::AccessDeliveries, controller::ControlledRecords, Step};
    use flotilla_state_machines::machines::lifecycle::LifecycleState;

    let directory = tempfile::tempdir().expect("temp dir");
    let service =
        Service::open(&directory.path().join("decided.sqlite"), "alpha", flotilla_resources::access_delivery::registry().expect("kinds"))
            .expect("store");
    let deliveries = AccessDeliveries::new(service.store());
    let controlled = deliveries.controlled();
    let version = deliveries.write("access-a", None, &spec("demo-1", "/h"), Timestamp::ZERO).expect("created");
    deliveries.write("access-b", None, &spec("demo-2", "/h"), Timestamp::ZERO).expect("created");
    assert!(deliveries.write("access-a", None, &spec("demo-1", "/h"), Timestamp::ZERO).is_err(), "created once");

    assert_eq!(controlled.live().expect("live"), ["access-a", "access-b"]);
    let held = controlled.get("access-a").expect("read").expect("held");
    assert_eq!((held.spec(), held.state()), (&spec("demo-1", "/h"), &LifecycleState::Realised { identity: "access-a".into() }));

    let edited = deliveries.write("access-a", Some(version), &spec("demo-1", "/moved"), Timestamp::ZERO).expect("edited");
    assert!(
        deliveries.write("access-a", Some(version), &spec("demo-1", "/stale"), Timestamp::ZERO).is_err(),
        "refused from a stale version"
    );
    assert_eq!(deliveries.get("access-a").expect("read").map(|held| (held.version, held.spec.home)), Some((edited, "/moved".into())));

    controlled.step("access-a", &Step::Released, Timestamp::ZERO).expect("released");
    assert_eq!(controlled.get("access-a").expect("read"), None, "removed");
    controlled.step("access-a", &Step::Released, Timestamp::ZERO).expect("already gone");
    assert_eq!(controlled.live().expect("live"), ["access-b"]);
}
