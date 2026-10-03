//! Shared merged-watch contract for the real memory and SQLite stores.
use std::collections::BTreeSet;

use flotilla_protocol::NodeId;
use futures::StreamExt;
use hegel::generators as gs;
use serde_json::Value;

use crate::{
    watch_resource_kind_including_replicas, DynamicResourceWatch, InMemoryBackend, InputMeta, Project, ProjectSpec, ResourceBackend,
    ResourceTombstone, SqliteBackend, WatchEvent,
};

fn apply(view: &mut BTreeSet<String>, event: &Value) {
    // Definitions have one logical identity in both snapshot and live events,
    // regardless of which source's final removal triggered the event.
    assert!(
        event["object"]["metadata"]["annotations"].get("flotilla.work/origin-root").is_none(),
        "merged provenance must agree with the snapshot: {event}"
    );
    let name = event["object"]["metadata"]["name"].as_str().expect("member name").to_string();
    if event["type"] == "DELETED" {
        view.remove(&name);
    } else {
        view.insert(name);
    }
}
async fn bootstrap(backend: &ResourceBackend) -> (DynamicResourceWatch, BTreeSet<String>) {
    let watch = watch_resource_kind_including_replicas(backend, "flotilla", "projects").await.expect("merged watch");
    let mut view = BTreeSet::new();
    for initial in &watch.initial {
        apply(&mut view, initial);
    }
    (watch, view)
}
async fn contract(backend: ResourceBackend, operations: &[usize]) {
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let meta = InputMeta::builder().name("platform".into()).build();
    let spec = ProjectSpec::builder().display_name("Platform".into()).default_workflow_ref("dev".into()).build();
    let object = source.using::<Project>("flotilla").create(&meta, &spec).await.expect("source");
    let (mut watch, mut view) = bootstrap(&backend).await;
    let mut local = false;
    for (step, operation) in operations.iter().copied().enumerate() {
        let sync = chrono::DateTime::from_timestamp(step as i64 + 1, 0).expect("sync timestamp");
        match operation {
            0 | 2 => {
                let root = NodeId::new(if operation == 0 { "a" } else { "b" });
                backend
                    .replica_writer::<Project>(root, "flotilla")
                    .apply(WatchEvent::Added(object.clone()), sync)
                    .await
                    .expect("upsert replica");
            }
            1 | 3 | 7 => {
                let root = NodeId::new(if operation == 3 { "b" } else { "a" });
                let event = if operation == 7 {
                    WatchEvent::DeletedByName(ResourceTombstone {
                        name: "platform".into(),
                        namespace: "flotilla".into(),
                        resource_version: step.to_string(),
                        annotations: Default::default(),
                    })
                } else {
                    WatchEvent::Deleted(object.clone())
                };
                backend.replica_writer::<Project>(root, "flotilla").apply(event, sync).await.expect("remove replica");
            }
            4 if !local => {
                // Seed physical local sources independently, like replication does:
                // definition admission would intentionally see the merged replica.
                match &backend {
                    ResourceBackend::InMemory(store) => store.create_typed::<Project>("flotilla", &meta, &spec).await,
                    ResourceBackend::Sqlite(store) => store.create_typed::<Project>("flotilla", &meta, &spec).await,
                    ResourceBackend::Http(_) => unreachable!("local stores"),
                }
                .expect("local member");
                local = true;
            }
            5 if local => {
                match &backend {
                    ResourceBackend::InMemory(store) => store.delete_typed::<Project>("flotilla", "platform").await,
                    ResourceBackend::Sqlite(store) => store.delete_typed::<Project>("flotilla", "platform").await,
                    ResourceBackend::Http(_) => unreachable!("local stores"),
                }
                .expect("local removal");
                local = false;
            }
            6 => {
                (watch, view) = bootstrap(&backend).await;
            }
            _ => continue,
        }
        if operation != 6 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), watch.stream.next())
                .await
                .expect("watch event")
                .expect("live watch")
                .expect("valid event");
            apply(&mut view, &event);
        }
        let expected = backend
            .including_replicas::<Project>("flotilla")
            .list()
            .await
            .expect("current view")
            .items
            .into_iter()
            .map(|item| item.object.metadata.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(view, expected, "no stale or lost member after step {step} ({operation})");
    }
}

// #2523: after each local/replica mutation and each fresh bootstrap, the held
// membership equals the current merged view. Draw duplicates, empty deletions,
// local shadows, two replica roots, name-only tombstones and reconnects.
#[hegel::test]
fn generated_merged_watch_membership(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    let operations = (0..count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(7))).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        contract(ResourceBackend::InMemory(InMemoryBackend::default()), &operations).await;
        contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")), &operations).await;
    });
}

// Pin the final-replica removal and tombstone failures independently of seeds.
#[tokio::test]
async fn merged_watch_removes_last_replica_and_preserves_surviving_sources() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))]
    {
        contract(backend, &[0, 0, 2, 1, 3, 6, 0, 7, 4, 0, 1, 5, 6]).await;
    }
}

// #2523: changes queued after snapshot capture but before the consumer applies
// it must still be replayed. The initial member is removed and a different
// member arrives, so either dropping a deletion or an addition fails this test.
#[tokio::test]
async fn merged_watch_replays_changes_queued_during_bootstrap() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))]
    {
        let source = ResourceBackend::InMemory(InMemoryBackend::default());
        let spec = ProjectSpec::builder().display_name("Platform".into()).default_workflow_ref("dev".into()).build();
        let before =
            source.using::<Project>("flotilla").create(&InputMeta::builder().name("before".into()).build(), &spec).await.expect("before");
        let after =
            source.using::<Project>("flotilla").create(&InputMeta::builder().name("after".into()).build(), &spec).await.expect("after");
        let writer = backend.replica_writer::<Project>(NodeId::new("remote"), "flotilla");
        writer.apply(WatchEvent::Added(before.clone()), chrono::Utc::now()).await.expect("initial replica");
        let mut watch = watch_resource_kind_including_replicas(&backend, "flotilla", "projects").await.expect("watch initial snapshot");
        writer.apply(WatchEvent::Deleted(before), chrono::Utc::now()).await.expect("bootstrap deletion");
        writer.apply(WatchEvent::Added(after), chrono::Utc::now()).await.expect("bootstrap arrival");
        let mut view = BTreeSet::new();
        for initial in &watch.initial {
            apply(&mut view, initial);
        }
        assert_eq!(view, BTreeSet::from(["before".into()]), "snapshot was captured before the changes");
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), watch.stream.next())
                .await
                .expect("queued update")
                .expect("stream")
                .expect("event");
            apply(&mut view, &event);
        }
        assert_eq!(view, BTreeSet::from(["after".into()]), "neither stale removal nor lost arrival");
    }
}
