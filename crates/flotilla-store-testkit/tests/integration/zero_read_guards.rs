//! #3141 / WORKLOAD.md: warm delivery and subscription have a zero-crossing
//! budget. Exercise real adapters, synchronizing on delivered events, not time.
use flotilla_protocol::NodeId;
use flotilla_resources::{Convoy, InputMeta, ReadWatchEvent, WatchEvent, WatchStart};
use flotilla_store::{ResourceBackend, StoreExt, WatchView};
use flotilla_store_testkit::{fixtures::contract::*, memory_backend, sqlite_backend, ReadCountsBackendExt, ZeroReadGuard};
use futures::{Stream, StreamExt};
use std::time::Duration;

async fn next<S: Stream + Unpin>(stream: &mut S) -> S::Item {
    tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("delivery deadline").expect("live watch")
}

fn backend(adapter: &str) -> ResourceBackend {
    match adapter {
        "memory" => memory_backend(),
        "sqlite" => sqlite_backend().expect("SQLite adapter"),
        _ => unreachable!(),
    }
    .with_local_root(NodeId::new("self"))
}

// Fan-out delivery includes Added, Modified and Deleted, with each subscriber
// drained after each event so this measures steady state rather than recovery.
#[rstest::rstest]
#[case("memory", 1)]
#[case("memory", 8)]
#[case("sqlite", 1)]
#[case("sqlite", 8)]
#[tokio::test]
async fn local_watch_delivery_reads_zero(#[case] adapter: &str, #[case] clients: usize) {
    let writer = backend(adapter);
    let counted = writer.clone().with_read_counts();
    let reader = counted.using::<Convoy>("flotilla");
    let mut watches = Vec::new();
    for _ in 0..clients {
        watches.push(reader.watch(WatchStart::Now).await.expect("watch"));
    }
    let guard = ZeroReadGuard::after_warm_up(&counted);
    let objects = writer.using::<Convoy>("flotilla");
    for step in 0..3 {
        match step {
            0 => {
                objects.create(&ConvoyFixture::meta("work"), &ConvoyFixture::spec()).await.expect("create");
            }
            1 => {
                let object = objects.get("work").await.expect("fixture read");
                objects
                    .update(&InputMeta::from(&object.metadata), &object.metadata.resource_version, &ConvoyFixture::updated_spec())
                    .await
                    .expect("update");
            }
            _ => {
                objects.delete("work").await.expect("delete");
            }
        }
        for watch in &mut watches {
            let event = next(watch).await.expect("event");
            assert!(matches!(
                (step, event),
                (0, WatchEvent::Added(_)) | (1, WatchEvent::Modified(_)) | (2, WatchEvent::Deleted(_)) | (2, WatchEvent::DeletedByName(_))
            ));
        }
        guard.assert_zero("local watch fan-out");
    }
}

// Both raw replica delivery and the public replica-inclusive watch must keep
// their zero budget. Include the serving root: self-origin shadow checks still
// do addressed gets today, so that public path is an active expected failure.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn replica_watch_delivery_reads_zero(#[case] adapter: &str) {
    replica_delivery(adapter, "peer").await;
}

#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
#[should_panic(expected = "zero durable-store read budget exceeded")]
async fn self_origin_replica_watch_zero_reads_pending_3120(#[case] adapter: &str) {
    // #3120 and successors: resident identity lookup replaces self-origin gets.
    replica_delivery(adapter, "self").await;
}

async fn replica_delivery(adapter: &str, origin: &str) {
    let writer = backend(adapter);
    let counted = writer.clone().with_read_counts();
    let mut watches = Vec::new();
    let mut raw = Vec::new();
    for _ in 0..4 {
        watches.push(counted.including_replicas::<Convoy>("flotilla").watch().await.expect("merged watch"));
        raw.push(counted.watch_replicas_typed::<Convoy>("flotilla").await.expect("raw replica watch"));
    }
    let source = memory_backend();
    let objects = source.using::<Convoy>("flotilla");
    let created = objects.create(&ConvoyFixture::meta("remote"), &ConvoyFixture::spec()).await.expect("peer object");
    let updated = objects
        .update(&InputMeta::from(&created.metadata), &created.metadata.resource_version, &ConvoyFixture::updated_spec())
        .await
        .expect("peer update");
    let guard = ZeroReadGuard::after_warm_up(&counted);
    for event in [WatchEvent::Added(created), WatchEvent::Modified(updated.clone()), WatchEvent::Deleted(updated)] {
        writer.replica_writer::<Convoy>(NodeId::new(origin), "flotilla").apply(event, chrono::Utc::now()).await.expect("replication");
        for stream in &mut raw {
            next(stream).await.expect("raw event");
        }
        guard.assert_zero("raw replica delivery");
        for stream in &mut watches {
            let delivered = next(stream).await.expect("merged event");
            assert!(matches!(
                delivered,
                ReadWatchEvent::Added(_) | ReadWatchEvent::Modified(_) | ReadWatchEvent::Deleted(_) | ReadWatchEvent::DeletedByName { .. }
            ));
        }
        guard.assert_zero("replica-inclusive delivery");
    }
}

// A warm informer serves new subscriptions' snapshots and subsequent deltas
// without re-reading storage. Empty and populated snapshots are both covered.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
async fn warm_view_subscriptions_read_zero(#[case] adapter: &str) {
    let writer = backend(adapter);
    let counted = writer.clone().with_read_counts();
    let view = WatchView::<Convoy>::start(counted.clone(), "flotilla").await.expect("bootstrap");
    let guard = ZeroReadGuard::after_warm_up(&counted);
    for index in 0..8 {
        let mut subscription = view.subscribe();
        assert_eq!(view.shared_list().await.expect("warm snapshot").items.len(), index);
        subscription.borrow_and_update();
        writer
            .using::<Convoy>("flotilla")
            .create(&ConvoyFixture::meta(&format!("work-{index}")), &ConvoyFixture::spec())
            .await
            .expect("write");
        tokio::time::timeout(Duration::from_secs(5), subscription.changed()).await.expect("delivery deadline").expect("invalidation");
        let snapshot = view.shared_list().await.expect("updated snapshot");
        assert_eq!(snapshot.items.len(), index + 1);
        assert!(snapshot.items.iter().any(|item| item.object.metadata.name == format!("work-{index}")));
        guard.assert_zero("warm view subscription and delivery");
    }
}

// #3120 and successors: merged definition delivery resolves the winning source
// through durable gets today. Keep this separate from raw replica delivery.
#[rstest::rstest]
#[case("memory")]
#[case("sqlite")]
#[tokio::test]
#[should_panic(expected = "zero durable-store read budget exceeded")]
async fn merged_definition_delivery_zero_reads_pending_3120(#[case] adapter: &str) {
    use flotilla_resources::{Project, ProjectSpec};
    let writer = backend(adapter);
    let counted = writer.clone().with_read_counts();
    let mut watch = flotilla_store::watch_resource_kind_including_replicas(&counted, "flotilla", "projects").await.expect("bootstrap");
    let guard = ZeroReadGuard::after_warm_up(&counted);
    writer
        .store()
        .create_typed::<Project>(
            "flotilla",
            &InputMeta::builder().name("project".into()).build(),
            &ProjectSpec::builder().display_name("Project".into()).build(),
        )
        .await
        .expect("local definition");
    let event = next(&mut watch.stream).await.expect("definition event");
    assert_eq!(event["object"]["metadata"]["name"], "project");
    guard.assert_zero("merged definition subscription delivery");
}
