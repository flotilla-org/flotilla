//! Read decode cost on a Convoy-shaped store (#3154). Not a gate: it prints
//! CPU per MB of stored text and the peak heap a read adds, so revisions can be
//! compared. It installs a process-wide counting allocator, so it lives in its
//! own target. Run it in release, one test at a time:
//!
//! ```text
//! cargo test --release -p flotilla-store-testkit --test decode_cost -- --ignored --nocapture --test-threads=1
//! ```
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{Convoy, ConvoySpec, ConvoyStatus, InputMeta, ObjectMeta, ResourceList, ResourceObject, WatchStart};
use flotilla_store::ResourceBackend;
use flotilla_store_sqlite::SqliteBackend;
use futures::StreamExt;
use serde_json::Value;

struct PeakAllocator;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// Delegates every operation to System and tracks live bytes across all threads,
// including the SQLite reader and writer threads that decode stored bodies.
unsafe impl GlobalAlloc for PeakAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if size >= layout.size() {
            grew(size - layout.size());
        } else {
            CURRENT.fetch_sub(layout.size() - size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: PeakAllocator = PeakAllocator;

const NAMESPACE: &str = "cost";
const LOCAL: usize = 250;
const ORIGINS: usize = 3;
const PER_ORIGIN: usize = 250;
const RUNS: usize = 7;

/// Real deployed Convoy specs and statuses (the golden stored corpus), cycled
/// to fill the store: frozen workflow snapshots dominate, as in production.
fn corpus() -> Vec<(ConvoySpec, Option<ConvoyStatus>)> {
    let documents: Vec<Value> = serde_json::from_str(include_str!("fixtures/stored-records/r355/Convoy.json")).expect("read Convoy corpus");
    documents
        .into_iter()
        .map(|document| {
            let spec = serde_json::from_value(document["spec"].clone()).expect("corpus spec");
            let status = document
                .get("status")
                .filter(|status| !status.is_null())
                .map(|status| serde_json::from_value(status.clone()).expect("corpus status"));
            (spec, status)
        })
        .collect()
}

struct Measured {
    median: Duration,
    peak_bytes: usize,
    retained_bytes: usize,
}

/// Median wall time over `RUNS` runs (the decode runs on one reader thread, so
/// wall time is its CPU time), the largest live-heap growth of any run while
/// it ran, and the heap still held by the returned objects.
async fn measure<F, Fut, R>(mut read: F) -> Measured
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    drop(read().await);
    let mut times = Vec::new();
    let mut peak_bytes = 0;
    let mut retained_bytes = 0;
    for _ in 0..RUNS {
        let baseline = CURRENT.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        let started = Instant::now();
        let output = read().await;
        times.push(started.elapsed());
        peak_bytes = peak_bytes.max(PEAK.load(Ordering::SeqCst).saturating_sub(baseline));
        retained_bytes = retained_bytes.max(CURRENT.load(Ordering::SeqCst).saturating_sub(baseline));
        drop(output);
    }
    times.sort();
    Measured { median: times[RUNS / 2], peak_bytes, retained_bytes }
}

fn report(label: &str, rows: usize, text_bytes: usize, measured: Measured) {
    let megabytes = text_bytes as f64 / 1_000_000.0;
    let millis = measured.median.as_secs_f64() * 1000.0;
    eprintln!(
        "{label}: {rows} rows, {megabytes:.2} MB text, {millis:.1} ms ({:.2} ms/MB), peak heap +{:.1} MB ({:.1}x text), result {:.1} MB",
        millis / megabytes,
        measured.peak_bytes as f64 / 1_000_000.0,
        measured.peak_bytes as f64 / text_bytes as f64,
        measured.retained_bytes as f64 / 1_000_000.0,
    );
}

fn meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(BTreeMap::from([("cost".to_string(), "convoy".to_string())])).build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement; run in release with --ignored --nocapture"]
async fn convoy_read_decode_cost() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("resources.sqlite");
    let backend = ResourceBackend::new(SqliteBackend::open(&path).expect("open sqlite")).with_local_root(NodeId::new("cost-root"));
    let corpus = corpus();
    let convoys = backend.using::<Convoy>(NAMESPACE);
    for index in 0..LOCAL {
        let (spec, status) = &corpus[index % corpus.len()];
        let name = format!("local-{index:04}");
        let created = convoys.create(&meta(&name), spec).await.expect("create");
        if let Some(status) = status {
            convoys.update_status(&name, &created.metadata.resource_version, status).await.expect("status");
        }
    }
    for origin in 0..ORIGINS {
        let items = (0..PER_ORIGIN)
            .map(|index| {
                let (spec, status) = &corpus[(index + origin * 7) % corpus.len()];
                ResourceObject::<Convoy> {
                    metadata: ObjectMeta {
                        name: format!("replica-{origin}-{index:04}"),
                        namespace: NAMESPACE.to_string(),
                        resource_version: (index + 1).to_string(),
                        labels: BTreeMap::from([("cost".to_string(), "convoy".to_string())]),
                        annotations: BTreeMap::new(),
                        owner_references: Vec::new(),
                        finalizers: Vec::new(),
                        deletion_timestamp: None,
                        creation_timestamp: Utc::now(),
                        merge: None,
                    },
                    spec: spec.clone(),
                    status: status.clone(),
                }
            })
            .collect();
        let listed = ResourceList { items, resource_version: PER_ORIGIN.to_string(), generation: None };
        backend
            .replica_writer::<Convoy>(NodeId::new(format!("origin-{origin}")), NAMESPACE)
            .replace(&listed, Utc::now())
            .await
            .expect("replicas");
    }
    drop(corpus);

    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    let text = |sql: &str| raw.query_row(sql, [], |row| row.get::<_, i64>(0)).expect("text size") as usize;
    let local_text = text("SELECT SUM(LENGTH(body_json)) FROM resource_objects WHERE kind = 'Convoy'");
    let replica_text = text("SELECT SUM(LENGTH(body_json)) FROM replica_objects WHERE kind = 'Convoy'");
    let event_text = text("SELECT SUM(LENGTH(body_json)) FROM resource_events WHERE kind = 'Convoy'");
    let events = text("SELECT COUNT(*) FROM resource_events WHERE kind = 'Convoy'");
    drop(raw);

    report("local list", LOCAL, local_text, measure(|| async { convoys.list().await.expect("list") }).await);
    let everything = backend.including_replicas::<Convoy>(NAMESPACE);
    report(
        "list including replicas",
        LOCAL + ORIGINS * PER_ORIGIN,
        local_text + replica_text,
        measure(|| async { everything.list().await.expect("list including replicas") }).await,
    );
    report(
        "get each local",
        LOCAL,
        local_text,
        measure(|| async {
            let mut objects = Vec::with_capacity(LOCAL);
            for index in 0..LOCAL {
                objects.push(convoys.get(&format!("local-{index:04}")).await.expect("get"));
            }
            objects
        })
        .await,
    );
    report(
        "watch replay from 0",
        events,
        event_text,
        measure(|| async {
            let mut stream = convoys.watch(WatchStart::FromVersion("0".to_string())).await.expect("watch");
            let mut replayed = Vec::with_capacity(events);
            for _ in 0..events {
                replayed.push(stream.next().await.expect("replayed").expect("decoded"));
            }
            replayed
        })
        .await,
    );
}

// #3430: count the bodies the old callers decoded versus indexed live/ensure
// queries. Half are replicas; real corpus snapshots retain production body sizes.
#[tokio::test]
#[ignore = "measurement; run explicitly with --ignored --nocapture"]
async fn convoy_filter_pushdown_decode_cost() {
    use flotilla_resources::ConvoyPhase;
    use flotilla_store::{live_convoys_query, StoreQuery};
    use flotilla_store_testkit::{measure_reads, ReadCountsBackendExt};
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("filters.sqlite");
    let backend = ResourceBackend::new(SqliteBackend::open(&path).expect("sqlite")).with_read_counts();
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let corpus = corpus();
    for index in 0..310 {
        let (spec, status) = &corpus[index % corpus.len()];
        let name = format!("c-{index:03}");
        let meta = InputMeta::builder()
            .name(name.clone())
            .annotations(BTreeMap::from([("flotilla.work/ensured-from".into(), if index % 10 < 5 { "wanted" } else { "other" }.into())]))
            .build();
        let created = convoys.create(&meta, spec).await.expect("convoy");
        let mut status = status.clone().unwrap_or_default();
        status.phase = if index < 300 { ConvoyPhase::Landed } else { ConvoyPhase::Active };
        convoys.update_status(&name, &created.metadata.resource_version, &status).await.expect("status");
    }
    let mut replicas = convoys.list().await.expect("fixture");
    replicas.items.retain(|object| object.metadata.name[2..].parse::<usize>().expect("index") % 2 == 0);
    backend.replica_writer::<Convoy>(NodeId::new("remote"), NAMESPACE).replace(&replicas, Utc::now()).await.expect("replicas");
    for object in &replicas.items {
        convoys.delete(&object.metadata.name).await.expect("move to replica");
    }
    let raw = rusqlite::Connection::open(&path).expect("raw measurement connection");
    let bytes = |predicate: &str| -> usize {
        ["resource_objects", "replica_objects"]
            .into_iter()
            .map(|table| {
                raw.query_row(
                    &format!("SELECT COALESCE(SUM(LENGTH(CAST(body_json AS BLOB))), 0) FROM {table} WHERE kind = 'Convoy' AND {predicate}"),
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .expect("stored bytes")
            })
            .sum()
    };
    let (before, reads) = measure_reads(backend.including_replicas::<Convoy>(NAMESPACE).list()).await;
    assert_eq!(before.expect("before").items.len(), 310);
    eprintln!("before list: {} rows, {} stored bytes decoded/call", reads.of(&backend).objects_by_kind()["Convoy"], bytes("1"));
    let (after, reads) = measure_reads(backend.query_declared::<Convoy>(NAMESPACE, &live_convoys_query())).await;
    assert_eq!(after.expect("live").items.len(), 10);
    eprintln!(
        "after live: {} rows, {} stored bytes decoded/call",
        reads.of(&backend).objects_by_kind()["Convoy"],
        bytes("json_extract(body_json, '$.status.phase') = 'Active'")
    );
    let query = StoreQuery {
        fields: BTreeMap::from([("metadata.annotations.\"flotilla.work/ensured-from\"".into(), "wanted".into())]),
        ..live_convoys_query()
    };
    let (after, reads) = measure_reads(backend.query_declared::<Convoy>(NAMESPACE, &query)).await;
    assert_eq!(after.expect("ensured").items.len(), 5);
    eprintln!("after ensured live: {} rows, {} stored bytes decoded/call", reads.of(&backend).objects_by_kind()["Convoy"], bytes("json_extract(body_json, '$.status.phase') = 'Active' AND json_extract(body_json, '$.metadata.annotations.\"flotilla.work/ensured-from\"') = 'wanted'"));
}
