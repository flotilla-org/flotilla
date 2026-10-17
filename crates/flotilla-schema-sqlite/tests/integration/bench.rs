//! Load benchmark for Message events (WORKLOAD.md `change_events`): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-schema-sqlite --release --locked --test integration bench -- --ignored --nocapture
//! ```
//!
//! A temp-file database in WAL mode with `synchronous = NORMAL` holds pre-created messages, each
//! Pending and expecting a reply. Each event is one engine edge through `Write::apply_edge`: the
//! engine's `deliver` (Pending to Awaiting, with its receiver) and then its `settle` (Awaiting to
//! Settled, on the reply), each computed by the engine's home before the timed loop. An edge moves
//! the message's state (the label, its `since` and the label's data, with the version, in the state
//! field's one statement) and appends an edge history row, whose document holds the edge's
//! arguments and the gate's one read. That is the edge row, the root row and the `change_log` row
//! (plus the `home_sequence` counter and the identity's sequence): one row fewer than WORKLOAD.md's
//! ~4 row writes per event, which counted the gate read as a row. Events are committed in batches of
//! 1, 10, 100 and 1000 per transaction.

use std::{env, fs, path::PathBuf, process, time::Instant};

use flotilla_schema::{Edge, Kind, Timestamp};
use flotilla_schema_sqlite::{Stamp, Store};
use flotilla_state_machines::{
    machines::message::{Deliver, MemoryMessageWorld, Message, MessageWorld, Receiver, Settle, Settlement},
    Home, Ref, Resource, Transport,
};
use rusqlite::Connection;

use crate::fixtures::{engine::NAME, message, registry, LOCAL};

const MESSAGES: usize = 10_000;
const INBOXES: usize = 100;
const ROWS_PER_EVENT: f64 = 3.0;

struct Database(PathBuf);

impl Drop for Database {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn open(batch: usize) -> (Database, Store) {
    let path = env::temp_dir().join(format!("flotilla-schema-bench-{}-{batch}.db", process::id()));
    let database = Database(path);
    // Clear leftovers of an interrupted run.
    drop(Database(database.0.clone()));
    let conn = Connection::open(&database.0).expect("open");
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).expect("wal");
    assert_eq!(mode, "wal");
    conn.pragma_update(None, "synchronous", "NORMAL").expect("synchronous");
    (database, Store::open(conn, LOCAL, registry()).expect("store"))
}

fn value(n: usize) -> Resource<Message> {
    message::sample_in(format!("inbox-{}", n % INBOXES))
}

/// The messages' stamps.
fn seed(store: &mut Store) -> Vec<Stamp> {
    let mut stamps = Vec::with_capacity(MESSAGES);
    for chunk in 0..MESSAGES / 1_000 {
        let mut write = store.write().expect("write");
        for n in chunk * 1_000..(chunk + 1) * 1_000 {
            stamps.push(write.insert(&format!("message-{n}"), &value(n)).expect("insert"));
        }
        write.commit().expect("commit");
    }
    stamps
}

/// Message `n`'s two events, as the engine applies them: its delivery to a session (the gate
/// reads the session's receipt) and its settlement on the reply (the gate reads the reply).
fn events(n: usize) -> [Edge<Resource<Message>>; 2] {
    let session = format!("session-{}", n % 50);
    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&Ref::new(NAME), [(session.clone(), Transport::Turn)].into());
    world.reply.put(&Ref::new(NAME), ());
    let world: Box<dyn MessageWorld> = Box::new(world);
    let mut home = Home::<Message>::new(LOCAL).expect("valid");
    home.restore(value(n).with_identity(NAME, 1)).unwrap_or_else(|_| panic!("an empty home"));
    let at = Timestamp(n as i64);
    let pending = home.get(NAME).expect("present").stored_state().clone();
    let deliver = Deliver { receiver: Receiver { session, host: "consumer".into() }, transport: Transport::Turn };
    let event = home.apply(NAME, deliver, &*world, at).unwrap_or_else(|_| panic!("delivered")).clone();
    let first = home.get(NAME).expect("present").stored_edge(&pending, &event).expect("edge");
    let delivered = home.get(NAME).expect("present").stored_state().clone();
    let event =
        home.apply(NAME, Settle { by: Settlement::Answered }, &*world, at.saturating_add(1)).unwrap_or_else(|_| panic!("settled")).clone();
    let second = home.get(NAME).expect("present").stored_edge(&delivered, &event).expect("edge");
    [first, second]
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn run(batch: usize, events: usize) -> String {
    let (_database, mut store) = open(batch);
    let mut messages = seed(&mut store);
    assert!(events <= 2 * MESSAGES, "each message takes two events");
    let mut pending: Vec<std::vec::IntoIter<Edge<Resource<Message>>>> =
        (0..MESSAGES).map(|n| Vec::from(self::events(n)).into_iter()).collect();
    let mut transactions = Vec::new();
    let mut commits = Vec::new();
    let start = Instant::now();
    let mut n = 0;
    while n < events {
        let began = Instant::now();
        let mut write = store.write().expect("write");
        for _ in 0..batch {
            let edge = pending[n % MESSAGES].next().expect("an event left");
            let stamp = &mut messages[n % MESSAGES];
            *stamp = write.apply_edge(*stamp, edge).expect("edge");
            n += 1;
        }
        let committing = Instant::now();
        write.commit().expect("commit");
        commits.push(committing.elapsed().as_secs_f64() * 1e3);
        transactions.push(began.elapsed().as_secs_f64() * 1e3);
    }
    let seconds = start.elapsed().as_secs_f64();
    transactions.sort_by(f64::total_cmp);
    commits.sort_by(f64::total_cmp);
    let rate = n as f64 / seconds;
    format!(
        "| {batch:>5} | {n:>6} | {rate:>9.0} | {:>9.0} | {:>8.3} | {:>8.3} | {:>8.3} | {:>8.3} |",
        rate * ROWS_PER_EVENT,
        percentile(&transactions, 0.5),
        percentile(&transactions, 0.99),
        percentile(&commits, 0.5),
        percentile(&commits, 0.99),
    )
}

// Measurement, not a pass/fail behaviour: prints sustained events/s and rows/s and per-transaction
// and commit latency per batch size, to compare with WORKLOAD.md's 1,000 events/s sustained
// (~4,000 rows/s) and 10,000 events/s bursts.
#[test]
#[ignore = "load benchmark: run in release with --ignored --nocapture"]
fn bench_message_events() {
    let mut lines = vec![
        format!("{MESSAGES} messages over {INBOXES} inboxes; each event, one engine edge through apply_edge: state move + edge history row (one gate read in its document) + change_log"),
        "| batch | events | events/s |    rows/s | txn p50 ms | txn p99 ms | commit p50 ms | commit p99 ms |".to_owned(),
        "|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for (batch, events) in [(1, 5_000), (10, 20_000), (100, 20_000), (1_000, 20_000)] {
        lines.push(run(batch, events));
    }
    eprintln!("{}", lines.join("\n"));
}
