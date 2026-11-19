use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::Duration,
};

use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::{
    stream::{Frame, Service},
    StoreError, WalPolicy,
};

use crate::fixtures::{addr, message, origin, registry};

const LIMIT: u64 = 128 * 1024;

fn policy() -> WalPolicy {
    WalPolicy { checkpoint_pages: 64, size_limit: LIMIT, warn_after_starved: 4 }
}

fn open(dir: &Path) -> (Arc<Service>, PathBuf) {
    let path = dir.join("store.sqlite");
    let service = Service::open_with_wal(&path, "home", registry(), policy()).expect("service");
    (Arc::new(service), dir.join("store.sqlite-wal"))
}

fn log_bytes(wal: &Path) -> u64 {
    std::fs::metadata(wal).expect("write-ahead log").len()
}

/// Inserts `count` messages in one transaction.
fn burst(service: &Service, prefix: &str, count: usize) {
    service
        .with_store(|store| {
            let mut write = store.write()?;
            for index in 0..count {
                write.insert(&format!("{prefix}-{index}"), &message::sample_in(addr("inbox")), &origin())?;
            }
            write.commit()
        })
        .expect("burst");
}

// A burst leaves the log far past its limit; the writer checkpoints it whole, and the next
// commit restarts the log and gives the file back down to the limit.
#[test]
fn the_log_shrinks_to_its_limit_once_a_burst_is_checkpointed() {
    let dir = tempfile::tempdir().expect("dir");
    let (service, wal) = open(dir.path());
    burst(&service, "burst", 3000);
    assert!(log_bytes(&wal) > 4 * LIMIT, "the burst grew the log to {} bytes", log_bytes(&wal));
    let reading = service.wal();
    assert!(reading.complete >= 1, "{reading:?}");
    assert_eq!(reading.uncheckpointed, 0, "{reading:?}");
    burst(&service, "after", 1);
    assert!(log_bytes(&wal) <= LIMIT, "the log file is {} bytes, past its {LIMIT}-byte limit", log_bytes(&wal));
}

// A reader holding a read transaction (here a snapshot round whose consumer has not taken its
// first frame) keeps every checkpoint from completing: the starved checkpoints are counted and
// warned of, naming the readers. Once it ends its round, the next checkpoint completes and the
// log shrinks back.
#[test]
fn a_held_reader_starves_checkpoints_until_it_ends() {
    let dir = tempfile::tempdir().expect("dir");
    let (service, wal) = open(dir.path());
    burst(&service, "seed", 1);
    let (entered, holding) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let reader = Arc::clone(&service);
    let round = std::thread::spawn(move || {
        reader.serve(None, Timestamp(2), |bytes| {
            if matches!(Frame::decode(bytes)?, Frame::Begin { .. }) {
                entered.send(()).map_err(|_| StoreError::Stream("test gone".into()))?;
                released.recv().map_err(|_| StoreError::Stream("test gone".into()))?;
            }
            Ok(())
        })
    });
    holding.recv_timeout(Duration::from_secs(10)).expect("the round holds its read");
    let before = service.wal();

    let (logging, logs) = flotilla_log_testkit::capture(tracing::Level::INFO);
    let _logging = tracing::dispatcher::set_default(&logging);
    for round in 0..4 {
        burst(&service, &format!("held-{round}"), 1000);
    }
    let starved = service.wal();
    assert_eq!(starved.starved - before.starved, 4, "{starved:?}");
    assert_eq!(starved.complete, before.complete, "{starved:?}");
    assert!(starved.uncheckpointed > 0, "{starved:?}");
    assert!(log_bytes(&wal) > LIMIT, "the held log is {} bytes", log_bytes(&wal));
    assert!(logs.text().contains("readers are holding the log"), "{}", logs.text());
    assert!(logs.text().contains("serving round of `home` from a snapshot"), "the warning names the round: {}", logs.text());

    release.send(()).expect("release");
    round.join().expect("round").expect("served");
    burst(&service, "released", 1);
    let released = service.wal();
    assert_eq!(released.uncheckpointed, 0, "{released:?}");
    assert_eq!(released.complete, before.complete + 1, "{released:?}");
    assert!(logs.text().contains("completed once readers released the log"), "{}", logs.text());
    burst(&service, "restarted", 1);
    assert!(log_bytes(&wal) <= LIMIT, "the log file is {} bytes, past its {LIMIT}-byte limit", log_bytes(&wal));
}
