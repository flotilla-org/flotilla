//! AwaitingReply under load (#3587 layer 7): ignored by default.
//!
//! ```text
//! FLOTILLA_AWAITING_BENCH_N=1000 cargo test -p flotilla-core --release --locked --test awaiting_bench -- --ignored --nocapture
//! ```
//!
//! `FLOTILLA_AWAITING_BENCH_N` records (default 10,000) at one home over a temp-file decided
//! store: one in ten a stall about one of 1,000 seats, the rest replies awaited by those seats,
//! half with a deadline. Per-event cost: the record written by a send, and the answer when its
//! reply lands (one keyed read and one edge). Per-query cost: "is this seat stalled", one probe of
//! the `open_by_subject` index, and the deadline sweep, one ordered read of the `deadline` index
//! that stops at the first record not yet due.

use std::{sync::Arc, time::Instant};

use flotilla_core::primitives::awaiting::{AwaitingReplies, STALL};
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::{
    awaiting_reply::{Awaited, AwaitingReplySpec},
    message::MessageRef,
};

const SEATS: usize = 1_000;
const SUPERVISOR: &str = "project:fleet/role:governor";

fn seat(n: usize) -> Address {
    format!("project:fleet/convoy:c{n}/role:coder").parse().expect("seat")
}

fn spec(n: usize) -> AwaitingReplySpec {
    let stall = n.is_multiple_of(10);
    AwaitingReplySpec {
        message: MessageRef::new(format!("{SUPERVISOR}/message:m{n}"), "home-b"),
        sender: seat(n % SEATS),
        recipient: SUPERVISOR.parse().expect("supervisor"),
        subject: stall.then(|| seat(n % SEATS)),
        kind: if stall { STALL.into() } else { "reply".into() },
        awaited: Awaited::Reply,
        deadline: n.is_multiple_of(2).then_some(Timestamp(1_000_000 + n as i64)),
    }
}

fn peak_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| line.strip_prefix("VmHWM:").map(|rest| rest.trim().trim_end_matches(" kB").trim().to_owned()))
        })
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
#[ignore = "load benchmark; run explicitly with --ignored --nocapture"]
fn awaiting_replies_at_the_ceiling() {
    let total: usize = std::env::var("FLOTILLA_AWAITING_BENCH_N").ok().and_then(|value| value.parse().ok()).unwrap_or(10_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), "home-a", flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    let records = AwaitingReplies::open("home-a", &service).expect("open");

    let started = Instant::now();
    for n in 0..total {
        assert!(records.record(spec(n), Timestamp(1)).expect("record"));
    }
    let per = started.elapsed().as_secs_f64() * 1e6 / total as f64;
    println!("{:>34}: {per:>8.1} µs/event over {total} records", "record (write + drop lookup)");

    let started = Instant::now();
    let mut stalled = 0;
    for n in 0..SEATS {
        stalled += usize::from(records.stalled(&seat(n)).expect("stalled"));
    }
    let per = started.elapsed().as_secs_f64() * 1e6 / SEATS as f64;
    println!("{:>34}: {per:>8.1} µs/query, {stalled} of {SEATS} seats stalled", "stalled(seat), open_by_subject");

    let started = Instant::now();
    let expired = records.expire_due(Timestamp(1_000_000 + total as i64 / 2)).expect("sweep");
    println!(
        "{:>34}: {:>8.1} ms for {} expired of {} with deadlines",
        "deadline sweep",
        started.elapsed().as_secs_f64() * 1e3,
        expired.len(),
        total / 2
    );

    let answers: Vec<usize> = (1..total).step_by(2).collect();
    let started = Instant::now();
    for &n in &answers {
        let spec = spec(n);
        let reply = MessageRef::new(format!("{}/message:r{n}", spec.sender), "home-a");
        assert!(records.landed(&reply, &spec.sender, Some(&spec.message), Timestamp(2)).expect("answer").is_some());
    }
    let per = started.elapsed().as_secs_f64() * 1e6 / answers.len() as f64;
    println!("{:>34}: {per:>8.1} µs/event over {} replies", "answer (keyed read + one edge)", answers.len());
    println!("{:>34}: {} KiB", "peak resident (VmHWM)", peak_kib());
}
