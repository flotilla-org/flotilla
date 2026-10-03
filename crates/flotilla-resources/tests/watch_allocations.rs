//! Allocation workload for #2507. Run with --nocapture to compare revisions.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::BTreeMap,
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{Convoy, ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend, SqliteBackend, WatchEvent, WatchStart};
use futures::StreamExt;

struct CountingAllocator;
thread_local! {
    static COUNT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

fn record(bytes: usize) {
    let _ = COUNT.try_with(|count| {
        if let Some((calls, total)) = count.get() {
            count.set(Some((calls + 1, total + bytes)));
        }
    });
}

// This test-only allocator delegates every operation to System; the scoped,
// thread-local counter excludes producers and SQLite's connection thread.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

async fn measured<F: std::future::Future>(label: &str, operation: F) -> (F::Output, (usize, usize)) {
    COUNT.with(|count| count.set(Some((0, 0))));
    let output = operation.await;
    let (calls, bytes) = COUNT.with(|count| count.replace(None)).expect("measurement active");
    eprintln!("{label}: {calls} allocations/reallocations, {bytes} requested bytes");
    (output, (calls, bytes))
}

// Delivered local, replica and replay objects must retain their own data after
// the backend is dropped. Measure just delivery of a 16 KiB/64-label object.
#[tokio::test(flavor = "current_thread")]
async fn delivered_watch_allocation_workload() {
    let directory = tempfile::tempdir().expect("sqlite directory");
    let sqlite = SqliteBackend::open(directory.path().join("watch.db")).expect("sqlite");
    for (name, backend) in [("memory", ResourceBackend::InMemory(InMemoryBackend::default())), ("sqlite", ResourceBackend::Sqlite(sqlite))]
    {
        let labels: BTreeMap<_, _> = (0..64).map(|index| (format!("label-{index}"), "x".repeat(64))).collect();
        let meta = InputMeta::builder().name("payload".into()).labels(labels).build();
        let spec = ConvoySpec::builder().workflow_ref("w".repeat(16 * 1024)).build();
        let resolver = backend.using::<Convoy>("allocation");
        let mut local = resolver.watch(WatchStart::Now).await.expect("local watch");
        let object = resolver.create(&meta, &spec).await.expect("create");
        let stored = serde_json::to_value(object.to_k8s_object()).expect("stored JSON");
        let (_, legacy_cost) = measured(&format!("{name}/legacy-clone-decode"), async {
            let decoded: flotilla_resources::K8sResourceObject<Convoy> = serde_json::from_value(stored.clone()).expect("legacy decode");
            flotilla_resources::ResourceObject::from_k8s_object(decoded).expect("legacy object")
        })
        .await;
        let (event, local_cost) = measured(&format!("{name}/local"), local.next()).await;
        let event = event.expect("event").expect("decode");
        assert!(local_cost.0 < legacy_cost.0 && local_cost.1 < legacy_cost.1, "delivery avoids the JSON-tree clone");
        assert!(matches!(&event, WatchEvent::Added(delivered) if delivered.spec == spec && delivered.metadata.labels == meta.labels));
        let mut replay = resolver.watch(WatchStart::FromVersion("0".into())).await.expect("replay watch");
        // SQLite decodes replay during watch construction on its connection
        // thread; this consumer-thread measurement only covers replay delivery.
        let (replay_event, replay_cost) = measured(&format!("{name}/replay-delivery"), replay.next()).await;
        let replay_event = replay_event.expect("replay").expect("decode");
        assert!(replay_cost.0 < local_cost.0 && replay_cost.1 < local_cost.1, "owned replay reuses its JSON strings");
        assert!(matches!(&replay_event, WatchEvent::Added(delivered) if delivered.spec == spec));
        let writer = backend.replica_writer::<Convoy>(NodeId::new("remote"), "replica-allocation");
        let mut replica = backend.including_replicas::<Convoy>("replica-allocation").watch().await.expect("replica watch");
        writer.apply(WatchEvent::Added(object), Utc::now()).await.expect("replicate");
        let (replica_event, replica_cost) = measured(&format!("{name}/replica"), replica.next()).await;
        let replica_event = replica_event.expect("replica").expect("decode");
        assert!(replica_cost.0 < legacy_cost.0 && replica_cost.1 < legacy_cost.1, "replica delivery avoids the JSON-tree clone");
        drop(local);
        drop(replay);
        drop(replica);
        drop(resolver);
        drop(writer);
        drop(backend);
        assert!(matches!(replica_event, flotilla_resources::ReadWatchEvent::Added(delivered) if delivered.object.spec == spec));
    }
}
