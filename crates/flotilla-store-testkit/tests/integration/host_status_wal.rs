//! #3154 measurement, not a gate: WAL bytes per simulated hour of Host status
//! writes against a file-backed SQLite store opened as the daemon opens it.
//!
//! Run with `cargo test -p flotilla-store-testkit --locked --test integration
//! host_status_wal -- --ignored --nocapture`. Each commit's WAL is measured by
//! truncating the WAL with `PRAGMA wal_checkpoint(TRUNCATE)` from a second
//! connection before the write and reading the `-wal` file length after it.
//! The schedules drive the store's `update_status` directly so this file also
//! measures the unmodified store (the "before" numbers).
//!
//! Measured 2026-10-13 (3,587 B corpus Host body, after 1,100 warm-up writes):
//!
//! | Store, schedule | Commits/h | WAL B/commit | WAL B/h |
//! |---|---|---|---|
//! | main (1,024 retained events), every observation commits | 720 | 71,308 | 51,341,760 |
//! | #3154 (one retained event), heartbeat only | 120 | 53,592 | 6,431,040 |
use chrono::{DateTime, Duration, Utc};
use flotilla_resources::{Host, HostSpec, HostStatus, InputMeta};
use flotilla_store::ResourceBackend;
use flotilla_store_sqlite::SqliteBackend;

const HOST: &str = "host-a";
const TICK_SECS: i64 = 5;
const HEARTBEAT_SECS: i64 = 30;
const TICKS_PER_HOUR: i64 = 3600 / TICK_SECS;
/// Enough writes to fill the configured 1,024-event stream, so compaction runs
/// on every measured write as it does on a long-running host.
const WARMUP_WRITES: usize = 1_100;

#[derive(Clone, Copy, Debug)]
enum Schedule {
    /// Before #3154: the heartbeat commits every 30 s, and the 5 s fulfilment
    /// probe commits each pass whose only change is a fresh `observed_at` (an
    /// unresolved provider re-stamps "unknown" every pass). The 5 s blob sync
    /// writer is not modelled: the store already declines identical bodies, so
    /// it costs writer transactions but no WAL.
    EveryObservationCommits,
    /// After #3154: the probe's refreshed `observed_at` folds into the heartbeat.
    HeartbeatOnly,
}

fn kiwi_status() -> HostStatus {
    // The largest Host record in the stored corpus (read-only, never regenerated).
    let documents: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../fixtures/stored-records/r355/Host.json")).expect("stored Host corpus");
    let document = documents.into_iter().max_by_key(|document| document["status"].to_string().len()).expect("a Host record");
    serde_json::from_value(document["status"].clone()).expect("decode stored Host status")
}

fn wal_bytes(path: &std::path::Path) -> u64 {
    std::fs::metadata(path.with_extension("sqlite-wal")).map(|metadata| metadata.len()).unwrap_or(0)
}

fn truncate_wal(checkpointer: &rusqlite::Connection) {
    let busy: i64 = checkpointer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)).expect("checkpoint");
    assert_eq!(busy, 0, "WAL truncation must not be blocked by a reader");
}

async fn measure(schedule: Schedule) -> (usize, u64, usize) {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("resources.sqlite");
    let backend = ResourceBackend::new(SqliteBackend::open_async(&path).await.expect("sqlite backend"));
    let hosts = backend.using::<Host>("flotilla");
    let mut object = hosts.create(&InputMeta::builder().name(HOST.to_string()).build(), &HostSpec::default()).await.expect("create");
    let mut status = kiwi_status();
    let body_bytes = serde_json::to_string(&status).expect("encode").len();
    let origin: DateTime<Utc> = "2026-10-13T00:00:00Z".parse().expect("time");
    for warmup in 0..WARMUP_WRITES {
        status.heartbeat_at = Some(origin - Duration::seconds(HEARTBEAT_SECS * (WARMUP_WRITES - warmup) as i64));
        object = hosts.update_status(HOST, &object.metadata.resource_version, &status).await.expect("warm-up write");
    }

    let checkpointer = rusqlite::Connection::open(&path).expect("checkpoint connection");
    let (mut commits, mut wal) = (0, 0);
    for tick in 0..TICKS_PER_HOUR {
        let now = origin + Duration::seconds(tick * TICK_SECS);
        let heartbeat = tick % (HEARTBEAT_SECS / TICK_SECS) == 0;
        if heartbeat || matches!(schedule, Schedule::EveryObservationCommits) {
            for facts in status.fulfilment_facts.values_mut() {
                facts.observed_at = now;
            }
        }
        if heartbeat {
            status.heartbeat_at = Some(now);
            status.daemon_rss_bytes = Some(200 * 1024 * 1024 + tick as u64);
        }
        let commit = heartbeat || matches!(schedule, Schedule::EveryObservationCommits);
        if commit {
            truncate_wal(&checkpointer);
            object = hosts.update_status(HOST, &object.metadata.resource_version, &status).await.expect("status write");
            wal += wal_bytes(&path);
            commits += 1;
        }
    }
    (commits, wal, body_bytes)
}

#[tokio::test]
#[ignore = "measurement for #3154; run with --ignored --nocapture"]
async fn host_status_wal_bytes_per_hour() {
    for schedule in [Schedule::EveryObservationCommits, Schedule::HeartbeatOnly] {
        let (commits, wal, body) = measure(schedule).await;
        println!(
            "{schedule:?}: body {body} B, {commits} commits/h, {wal} WAL B/h ({} B/commit, {:.1} MB/day)",
            wal / commits as u64,
            wal as f64 * 24.0 / 1_000_000.0
        );
    }
}
