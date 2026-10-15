//! Load benchmark for Message events (WORKLOAD.md `change_events`): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-schema-sqlite --release --locked --test integration bench -- --ignored --nocapture
//! ```
//!
//! A temp-file database in WAL mode with `synchronous = NORMAL` holds pre-created messages. Each
//! event appends an edge history row with one gate input and moves the message's state, through
//! `Write::update`: the root row, the edge row, the gate-input row and the `change_log` row (plus
//! the `home_sequence` counter), WORKLOAD.md's ~4 row writes per event. Events are committed in
//! batches of 1, 10, 100 and 1000 per transaction.

use std::{env, fs, path::PathBuf, process, time::Instant};

use flotilla_schema::{Identity, Kind, Ref, State, Timestamp};
use flotilla_schema_sqlite::{Loaded, Store};
use rusqlite::Connection;

use crate::fixtures::{
    message::{AgentName, GateInput, MessageEdge, MessageEdgeEvent, MessageState, Transport},
    registry, AgentSession, Message, LOCAL,
};

const MESSAGES: usize = 10_000;
const INBOXES: usize = 100;
const ROWS_PER_EVENT: f64 = 4.0;

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

fn seed(store: &mut Store) -> Vec<Loaded<Message>> {
    let mut loaded = Vec::with_capacity(MESSAGES);
    for chunk in 0..MESSAGES / 1_000 {
        let mut write = store.write().expect("write");
        for n in chunk * 1_000..(chunk + 1) * 1_000 {
            let value = Message {
                inbox: Ref::new(format!("inbox-{}", n % INBOXES), LOCAL),
                body: format!("message body {n}: please review the change and reply with findings"),
                accepted_seq: n as u64,
                ..Message::sample()
            };
            let stamp = write.insert(&format!("message-{n}"), &value).expect("insert");
            loaded.push(Loaded { id: stamp.id, name: format!("message-{n}"), home: LOCAL.to_owned(), version: stamp.version, value });
        }
        write.commit().expect("commit");
    }
    loaded
}

/// The next state of a message after one delivery event.
fn event(current: &Message, n: usize) -> Message {
    let mut next = current.clone();
    // The store stamps the appended row with the write's sequence.
    next.edges.append(
        0,
        MessageEdgeEvent {
            edge: MessageEdge::Deliver {
                receiver: AgentName("crew".into()),
                receipt: Ref::new(format!("session-{}", n % 50), LOCAL),
                transport: Transport::Turn,
            },
            at: Timestamp(n as i64),
            writer: LOCAL.into(),
            inputs: vec![GateInput {
                observed: Identity { kind: AgentSession::CODE, name: format!("session-{}", n % 50), home: LOCAL.into() },
                version: n as u64,
            }],
        },
    );
    let state = if current.state.value == MessageState::Pending { MessageState::Awaiting } else { MessageState::Pending };
    next.state = State::new(state, Timestamp(n as i64));
    next
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn run(batch: usize, events: usize) -> String {
    let (_database, mut store) = open(batch);
    let mut messages = seed(&mut store);
    let mut transactions = Vec::new();
    let mut commits = Vec::new();
    let start = Instant::now();
    let mut n = 0;
    while n < events {
        let began = Instant::now();
        let mut write = store.write().expect("write");
        for _ in 0..batch {
            let current = &mut messages[n % MESSAGES];
            let next = event(&current.value, n);
            let stamp = write.update(current, &next).expect("update");
            current.version = stamp.version;
            current.value = next;
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
        format!("{MESSAGES} messages over {INBOXES} inboxes; each event: edge record + gate input + state move + change_log"),
        "| batch | events | events/s |    rows/s | txn p50 ms | txn p99 ms | commit p50 ms | commit p99 ms |".to_owned(),
        "|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for (batch, events) in [(1, 5_000), (10, 20_000), (100, 20_000), (1_000, 20_000)] {
        lines.push(run(batch, events));
    }
    eprintln!("{}", lines.join("\n"));
}
