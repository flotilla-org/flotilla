//! Watch replay runs on a reader (#3154 item 4): registration on the writer,
//! replay of `(from, registered version]` on a reader, live delivery after.

use std::{
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use futures::{FutureExt, StreamExt};
use hegel::generators as gs;

use super::SqliteBackend;
use crate::{
    retention::EventRetention, InputMeta, Resource, ResourceBackend, ResourceError, ResourceKind, TypedResolver, WatchEvent, WatchStart,
};

const NAMESPACE: &str = "flotilla";
const NAMES: [&str; 2] = ["feta", "kiwi"];

/// One delivered or committed event: version, action and object name.
type Seen = (u64, &'static str, String);

/// A write drawn for the scenario: which name, and whether a live name is
/// deleted rather than updated. A missing name is always created.
#[derive(Debug, Clone, Copy)]
struct Write {
    name: usize,
    delete: bool,
}

/// The committed event log as the writes produced it.
#[derive(Default)]
struct Model {
    events: Vec<Seen>,
    live: std::collections::BTreeMap<String, String>,
    counter: u64,
}

/// A plain stream kind with configured event retention, so the scenario's
/// compaction model does not depend on any registered kind's retention.
struct ProbeResource;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProbeSpec {
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProbeStatus {}

enum ProbePatch {}
impl flotilla_resources::StatusPatch<ProbeStatus> for ProbePatch {
    fn apply(&self, _: &mut ProbeStatus) {
        match *self {}
    }
}

impl Resource for ProbeResource {
    type Spec = ProbeSpec;
    type Status = ProbeStatus;
    type StatusPatch = ProbePatch;

    const API_PATHS: flotilla_resources::ApiPaths =
        flotilla_resources::ApiPaths { group: "flotilla.work", version: "v1", plural: "proberesources", kind: "ProbeResource" };
    const REPLICATION_CLASS: flotilla_resources::ReplicationClass = flotilla_resources::ReplicationClass::None;
}

fn spec(label: String) -> ProbeSpec {
    ProbeSpec { label }
}

/// Apply one write and append its event to the model. Every write changes the
/// object, so each commits exactly one event; the stream position confirms it.
async fn apply(backend: &SqliteBackend, hosts: &TypedResolver<ProbeResource>, model: &Mutex<Model>, write: Write) {
    let name = NAMES[write.name].to_string();
    let (current, label) = {
        let mut model = model.lock().expect("model");
        model.counter += 1;
        (model.live.get(&name).cloned(), format!("{name}-{}", model.counter))
    };
    let meta = InputMeta::builder().name(name.clone()).build();
    let action = match current {
        None => {
            hosts.create(&meta, &spec(label)).await.expect("create probe");
            "ADDED"
        }
        Some(version) if !write.delete => {
            hosts.update(&meta, &version, &spec(label)).await.expect("update probe");
            "MODIFIED"
        }
        Some(_) => {
            hosts.delete(&name).await.expect("delete host");
            "DELETED"
        }
    };
    let position = backend.current_position_raw(ResourceKind::of::<ProbeResource>(), NAMESPACE).await.expect("stream position");
    let version: u64 = position.resource_version.parse().expect("numeric position");
    let mut model = model.lock().expect("model");
    let expected = model.events.last().map_or(1, |(last, _, _)| last + 1);
    assert_eq!(version, expected, "each scenario write commits exactly one event");
    if action == "DELETED" {
        model.live.remove(&name);
    } else {
        model.live.insert(name.clone(), version.to_string());
    }
    model.events.push((version, action, name));
}

fn seen(event: WatchEvent<ProbeResource>) -> Seen {
    let (action, metadata) = match event {
        WatchEvent::Added(object) => ("ADDED", object.metadata),
        WatchEvent::Modified(object) => ("MODIFIED", object.metadata),
        WatchEvent::Deleted(object) => ("DELETED", object.metadata),
        WatchEvent::DeletedByName(tombstone) => {
            return (tombstone.resource_version.parse().expect("numeric version"), "DELETED", tombstone.name)
        }
    };
    (metadata.resource_version.parse().expect("numeric version"), action, metadata.name)
}

fn open(file: bool, retention: usize, directory: &tempfile::TempDir) -> SqliteBackend {
    let retention = EventRetention::new(retention).expect("retention");
    if file {
        SqliteBackend::open_with_event_retention(directory.path().join("resources.sqlite"), retention).expect("open file store")
    } else {
        SqliteBackend::open_in_memory_with_event_retention(retention).expect("open in-memory store")
    }
}

fn draw_writes(tc: &hegel::TestCase, max: usize) -> Vec<Write> {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(max));
    (0..count)
        .map(|_| Write { name: tc.draw(gs::integers::<usize>().min_value(0).max_value(NAMES.len() - 1)), delete: tc.draw(gs::booleans()) })
        .collect()
}

// Behaviour (#3154 item 4): a watch from version `from` delivers every event
// committed after `from` exactly once and in commit order, whether the event
// committed before the watch, between the subscriber's registration and the
// reader's replay, inside the reader's replay snapshot, or after the watch
// returned. When compaction has removed an event after `from` (before
// registration or before the reader's snapshot), the watch fails with
// `WatchExpired` naming the floor instead of skipping it; a start ahead of the
// stream is invalid.
//
// Generators: both backends (the in-memory one replays on its only
// connection); retention 1-5 events so compaction races the replay in many
// cases; 0-8 writes before, 0-5 between registration and replay, 0-4 inside
// the snapshot (file only: the in-memory reader is the writer) and 0-4 after;
// `from` spans 0 through one past the current version, so expired, empty,
// full and ahead starts are all drawn. Writes create, update and delete two
// names, so every event action is replayed and delivered live.
#[hegel::test(suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn watch_delivers_each_event_after_its_start_exactly_once(tc: hegel::TestCase) {
    let file = tc.draw(gs::booleans());
    let retention = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let before = draw_writes(&tc, 8);
    let from = tc.draw(gs::integers::<u64>().min_value(0).max_value(before.len() as u64 + 1));
    let registered = draw_writes(&tc, 5);
    let in_snapshot = if file { draw_writes(&tc, 4) } else { Vec::new() };
    let after = draw_writes(&tc, 4);

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let handle = runtime.handle().clone();
    runtime.block_on(async move {
        let directory = tempfile::tempdir().expect("store directory");
        let mut backend = open(file, retention, &directory);
        let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
        let model = Arc::new(Mutex::new(Model::default()));
        for write in before {
            apply(&backend, &hosts, &model, write).await;
        }
        let current = model.lock().expect("model").events.len() as u64;
        let floor_at = |events: u64| events.saturating_sub(retention as u64);
        let floor_at_registration = floor_at(current);
        let floor_at_snapshot = floor_at(current + registered.len() as u64);

        let (pause_backend, pause_hosts, pause_model) = (backend.clone(), hosts.clone(), Arc::clone(&model));
        let registered = Arc::new(Mutex::new(Some(registered)));
        backend.replay_probe.registered = Some(Arc::new(move || {
            let (backend, hosts, model) = (pause_backend.clone(), pause_hosts.clone(), Arc::clone(&pause_model));
            let writes = registered.lock().expect("writes").take().unwrap_or_default();
            async move {
                for write in writes {
                    apply(&backend, &hosts, &model, write).await;
                }
            }
            .boxed()
        }));
        let (pause_backend, pause_hosts, pause_model) = (backend.clone(), hosts.clone(), Arc::clone(&model));
        let in_snapshot = Mutex::new(Some(in_snapshot));
        backend.replay_probe.in_snapshot = Some(Arc::new(move || {
            // The reader is a plain worker thread, so it can drive the writes
            // to completion on the writer while its own snapshot stays fixed.
            for write in in_snapshot.lock().expect("writes").take().unwrap_or_default() {
                handle.block_on(apply(&pause_backend, &pause_hosts, &pause_model, write));
            }
        }));

        let watched = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
        let watch = tokio::time::timeout(Duration::from_secs(30), watched.watch(WatchStart::FromVersion(from.to_string())))
            .await
            .expect("the watch starts without waiting on its own writes");
        if from > current {
            let error = watch.err().unwrap_or_else(|| panic!("a start ahead of the stream is rejected"));
            assert!(matches!(error, ResourceError::Invalid { .. }), "unexpected error: {error}");
            return;
        }
        let expired_floor = if from < floor_at_registration {
            Some(floor_at_registration)
        } else if from < current && from < floor_at_snapshot {
            // Only a non-empty replay reads the log; an empty one goes live.
            Some(floor_at_snapshot)
        } else {
            None
        };
        if let Some(floor) = expired_floor {
            let error = watch.err().unwrap_or_else(|| panic!("a start below the compaction floor expires"));
            assert_eq!(
                error,
                ResourceError::WatchExpired { requested_version: from.to_string(), compacted_through: Some(floor.to_string()) }
            );
            return;
        }
        let mut watch = watch.expect("watch from a retained version");
        for write in after {
            apply(&backend, &hosts, &model, write).await;
        }
        let expected: Vec<Seen> = model.lock().expect("model").events.iter().filter(|(version, _, _)| *version > from).cloned().collect();
        let mut delivered = Vec::new();
        for _ in 0..expected.len() {
            let event = tokio::time::timeout(Duration::from_secs(10), watch.next())
                .await
                .expect("event arrives")
                .expect("stream stays open")
                .expect("event decodes");
            delivered.push(seen(event));
        }
        assert_eq!(delivered, expected, "every event after the start, once, in order");
        assert!(watch.next().now_or_never().is_none(), "no event is delivered twice");
    });
}

/// Block every resource reader except the one the next read will lease; the
/// returned permits keep the others out of the pool.
async fn single_resource_reader(backend: &SqliteBackend) -> tokio::sync::SemaphorePermit<'_> {
    backend.reader_slots.acquire_many((super::READER_COUNT - 2) as u32).await.expect("hold other readers")
}

// Behaviour (#3154 item 4): watch replay runs on a reader, not the writer.
// While a replay is in progress (its reader holds the snapshot, paused), a
// write commits; the writer executes only the registration and that write,
// and one reader executes the replay. The concurrent write is delivered live
// after the replayed events, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_replay_runs_on_a_reader_while_a_write_commits() {
    const SEEDED: u64 = 300;
    let directory = tempfile::tempdir().expect("store directory");
    let mut backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
    let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let meta = InputMeta::builder().name("feta".to_string()).build();
    let mut current = hosts.create(&meta, &spec("x".repeat(4096))).await.expect("create probe");
    for index in 1..SEEDED {
        current =
            hosts.update(&meta, &current.metadata.resource_version, &spec(format!("{index}{}", "x".repeat(4096)))).await.expect("update");
    }

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    backend.replay_probe.in_snapshot = Some(Arc::new(move || {
        entered_tx.send(()).expect("announce replay");
        release_rx.lock().expect("release").recv().expect("release replay");
    }));
    let watched = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let before = backend.prepare_counters();
    let watch = tokio::spawn(async move { watched.watch(WatchStart::FromVersion("0".to_string())).await });
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("replay entered its snapshot")).await.expect("wait for replay");

    // The replay holds its reader; a write on the writer must still commit.
    let concurrent = tokio::time::timeout(
        Duration::from_secs(10),
        hosts.update(&meta, &current.metadata.resource_version, &spec("concurrent".to_string())),
    )
    .await
    .expect("a write commits while the replay is in progress")
    .expect("update probe");
    let during = backend.prepare_counters().since(&before);
    release_tx.send(()).expect("release replay");
    let mut watch = watch.await.expect("watch task").expect("watch from the start of the stream");
    let used = backend.prepare_counters().since(&before);

    assert_eq!(during.writer.operations, 2, "the writer ran only the registration and the concurrent write");
    assert_eq!(used.writer.operations, 2, "the replay added no writer operation");
    assert_eq!(used.readers_total().operations, 1, "one reader ran the replay");

    let mut versions = Vec::new();
    for _ in 0..=SEEDED {
        let event = tokio::time::timeout(Duration::from_secs(10), watch.next()).await.expect("event").expect("open").expect("decodes");
        versions.push(seen(event).0);
    }
    assert_eq!(versions, (1..=SEEDED + 1).collect::<Vec<_>>(), "replay then the concurrent write, once each");
    assert_eq!(concurrent.metadata.resource_version, (SEEDED + 1).to_string());
    assert!(watch.next().now_or_never().is_none(), "the concurrent write is not delivered twice");
}

// Behaviour: the reader deadline bounds only a replay's queue wait. A replay
// still running when the deadline passes completes and is delivered; reads
// that are shed keep the total deadline (`expired_reads_do_not_execute`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_replay_outlasting_the_read_deadline_completes() {
    let directory = tempfile::tempdir().expect("store directory");
    let mut backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
    let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let meta = InputMeta::builder().name("feta".to_string()).build();
    hosts.create(&meta, &spec("feta".to_string())).await.expect("create probe");
    backend.replay_probe.in_snapshot = Some(Arc::new(|| std::thread::sleep(super::READ_DEADLINE + Duration::from_millis(300))));
    let watched = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let mut watch = watched.watch(WatchStart::FromVersion("0".to_string())).await.expect("a slow replay is not shed");
    let event = watch.next().await.expect("event").expect("decodes");
    assert_eq!(seen(event), (1, "ADDED", "feta".to_string()));
}

// Behaviour: a replay that cannot start within the reader deadline is shed
// with the store's busy error, as other reads are; the writer stays free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_replay_queued_past_the_read_deadline_reports_busy() {
    let directory = tempfile::tempdir().expect("store directory");
    let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
    let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    hosts.create(&InputMeta::builder().name("feta".to_string()).build(), &spec("feta".to_string())).await.expect("create probe");
    let _others = single_resource_reader(&backend).await;
    let reader = backend.read_connections[super::READER_COUNT - 2].clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let blocked = tokio::spawn(async move {
        reader
            .call(move |_| {
                entered_tx.send(()).expect("announce blocked reader");
                release_rx.recv().expect("release blocked reader");
                Ok::<_, ResourceError>(())
            })
            .await
            .expect("blocked reader completes");
    });
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("reader blocked")).await.expect("wait for reader");
    let started = Instant::now();
    let result = hosts.watch(WatchStart::FromVersion("0".to_string())).await;
    release_tx.send(()).expect("release reader");
    blocked.await.expect("reader task");
    let error = result.err().unwrap_or_else(|| panic!("a replay that cannot start is shed"));
    assert!(error.to_string().contains("store busy"), "unexpected error: {error}");
    assert!(started.elapsed() < super::READ_DEADLINE * 2, "shed at the deadline");
}

// Behaviour: a watch from the current version, or `Now`, needs no replay and
// never occupies a reader.
#[tokio::test]
async fn watch_without_replay_does_not_use_a_reader() {
    let directory = tempfile::tempdir().expect("store directory");
    let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
    let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let created =
        hosts.create(&InputMeta::builder().name("feta".to_string()).build(), &spec("feta".to_string())).await.expect("create probe");
    for start in [WatchStart::Now, WatchStart::FromVersion(created.metadata.resource_version.clone())] {
        let before = backend.prepare_counters();
        let _watch = hosts.watch(start).await.expect("watch");
        let used = backend.prepare_counters().since(&before);
        assert_eq!(used.writer.operations, 1, "registration only");
        assert_eq!(used.readers_total().operations, 0, "no reader for an empty replay");
    }
}

// Measurement for #3154 item 4, not a gate. Seeds a Convoy-sized stream
// (1,024 events of about 25 KB, about 25 MB) and reports (a) how long the
// replay occupies a connection, measured by running it on the writer as the
// previous implementation did, and (b) the longest writer stall a no-op writer
// operation sees while a watch replays from the floor now.
// Run: cargo test -p flotilla-store-sqlite --release --locked measure_watch_replay_writer_stall -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement; prints timings and does not gate"]
async fn measure_watch_replay_writer_stall() {
    const EVENTS: usize = 1024;
    const BODY: usize = 25 * 1024;
    let directory = tempfile::tempdir().expect("store directory");
    let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
    let hosts = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
    let meta = InputMeta::builder().name("convoy-sized".to_string()).build();
    let mut current = hosts.create(&meta, &spec("x".repeat(BODY))).await.expect("create");
    for index in 1..EVENTS {
        current =
            hosts.update(&meta, &current.metadata.resource_version, &spec(format!("{index}{}", "x".repeat(BODY)))).await.expect("update");
    }
    let key = SqliteBackend::store_key(ResourceKind::of::<ProbeResource>(), NAMESPACE);
    let through: u64 = current.metadata.resource_version.parse().expect("version");

    let mut on_writer = Vec::new();
    for _ in 0..5 {
        let key = key.clone();
        let started = Instant::now();
        backend
            .call("measure replay on writer", move |connection| {
                SqliteBackend::replay_events(
                    ResourceKind::of::<ProbeResource>(),
                    connection,
                    &key,
                    0,
                    through,
                    super::RowForm::typed,
                    &super::ReplayProbe::default(),
                )
                .map(|replayed| replayed.events.len())
            })
            .await
            .expect("replay");
        on_writer.push(started.elapsed());
    }

    let mut stalls = Vec::new();
    for _ in 0..5 {
        let watched = ResourceBackend::new(backend.clone()).using::<ProbeResource>(NAMESPACE);
        let watch = tokio::spawn(async move { watched.watch(WatchStart::FromVersion("0".to_string())).await.map(|_| ()) });
        let mut longest = Duration::ZERO;
        while !watch.is_finished() {
            let started = Instant::now();
            backend.call("measure writer probe", |_| Ok(())).await.expect("probe");
            longest = longest.max(started.elapsed());
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        watch.await.expect("watch task").expect("watch");
        stalls.push(longest);
    }
    println!("replay of {EVENTS} x {BODY} B on the writer (previous placement): {on_writer:?}");
    println!("longest writer no-op latency during a reader replay (now): {stalls:?}");
}

#[test]
fn probe_is_a_stream_with_configured_retention() {
    // glue: the scenario's compaction model assumes the probe keeps the
    // configured number of events (not the one-event observation class).
    assert_eq!(ProbeResource::REPLICATION_CLASS.event_retention(3), 3);
}
