mod support;
use support::{CountedInbox as SqliteInbox, CountingLog};
// Process-wide Rust live-allocation measurement: kept in its own test target.
// SQLite's authoritative data/pages use its native allocator and are excluded.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        atomic::{AtomicIsize, Ordering},
        Arc, Mutex,
    },
};

use flotilla_inbox::{sqlite::StoreLog, InboxBackend, Rules};
use flotilla_inbox_testkit::{consumer, inbox, spec, Evidence, SessionPort, Supervisors};
use flotilla_schema_sqlite::{Store, StoreOptions};
use flotilla_state_machines::machines::message::MessageBody;
use futures::TryStreamExt;
use rusqlite::Connection;

struct LiveAllocator;
static LIVE: AtomicIsize = AtomicIsize::new(0);
// Every operation delegates to System. Track requested live Rust bytes across
// the async caller and its sole blocking writer; no pointer is inspected.
unsafe impl GlobalAlloc for LiveAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }
}
#[global_allocator]
static ALLOCATOR: LiveAllocator = LiveAllocator;

// Behaviour: once the replica and fixed-size store caches are warm, another
// equal block of open messages grows only by its budgeted Pending keys; values plateau.
// This fixed workload measures allocation/rate behavior, not gate logic.
#[tokio::test(flavor = "current_thread")]
async fn bounded_resident_allocation_workload() {
    calibrate();
    const BUDGET: usize = 32 * 1024;
    // Small, explicitly bounded source caches isolate the replica plateau.
    // Production Store defaults have the same two-generation policy at 65,536.
    let store = Arc::new(Mutex::new(
        Store::open_with(
            Connection::open_in_memory().expect("db"),
            "test",
            flotilla_state_machines::kinds().expect("registry"),
            StoreOptions { string_cache: 8, identity_cache: 8, ..StoreOptions::default() },
        )
        .expect("store"),
    ));
    let evidence = Arc::new(Evidence::default());
    let log = CountingLog::new(store.clone());
    let backend = SqliteInbox::open_with_budget("test", log.clone(), evidence, Arc::new(Supervisors), BUDGET).expect("open");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store)));
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut retained = Vec::new();
    for block in 0..3 {
        for n in 0..32 {
            let mut value = spec();
            value.body = MessageBody::text(format!("{block}/{n}:{}", "x".repeat(2048))).expect("body");
            backend.enqueue(value).await.expect("enqueue");
        }
        let residency = backend.residency_blocking();
        assert!(residency.accounted_bytes <= BUDGET, "{residency:?}");
        assert_eq!(residency.pending_messages, (block + 1) * 32);
        retained.push(LIVE.load(Ordering::SeqCst));
        eprintln!("messages={} retained_rust_bytes={} residency={residency:?}", (block + 1) * 32, retained[block]);
    }
    assert!(retained[2] - retained[1] < BUDGET as isize, "resident allocations plateau: {retained:?}");
    log.reset_read_counts();
    let mut pending = backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending");
    let mut count = 0;
    while pending.try_next().await.expect("stream").is_some() {
        count += 1;
    }
    assert_eq!(count, 96);
    assert_eq!(log.read_counts().resource_loads, 96);
    assert_eq!(log.read_counts().terminal_loads + log.read_counts().scan_pages, 0);
}

#[allow(dead_code)]
#[path = "../src/resident.rs"]
mod charges;

fn calibrate() {
    use flotilla_inbox_testkit::address;
    use flotilla_state_machines::{machines::message::*, Resource, Version};
    use std::{collections::BTreeMap, hint::black_box, mem::size_of};
    const N: usize = 1000;
    let artifact = address("project:test/convoy:run/role:coder")
        .artifact("explainer", &format!("project:{}/role:{}", "p".repeat(512), "r".repeat(512)))
        .unwrap()
        .to_string();
    // Parsed and cloned cases span short/long values, extra hierarchy depth,
    // artifact subjects and escaped delimiters in the canonical representation.
    for text in [
        "project:test/role:inbox".to_owned(),
        format!("project:{}/role:{}", "p".repeat(64), "r".repeat(64)),
        format!("project:{}/role:{}", "p".repeat(1024), "r".repeat(1024)),
        format!("project:{}/convoy:{}/role:{}", "p".repeat(256), "c".repeat(256), "r".repeat(256)),
        artifact,
    ] {
        let template = address(&text);
        let base = LIVE.load(Ordering::SeqCst);
        let rows: Vec<_> = (0..N).map(|_| address(&text)).collect();
        let actual = (LIVE.load(Ordering::SeqCst) - base) as usize / N;
        let charge = charges::address(&template);
        eprintln!("calibrate parsed address len={} allocated={actual} charge={charge}", text.len());
        assert!(charge >= actual, "address charge covers parsed allocation");
        black_box(&rows);
        drop(rows);
        let base = LIVE.load(Ordering::SeqCst);
        let rows: Vec<_> = (0..N).map(|_| template.clone()).collect();
        let actual = (LIVE.load(Ordering::SeqCst) - base) as usize / N;
        eprintln!("calibrate cloned address len={} allocated={actual} charge={charge}", text.len());
        assert!(charge >= actual);
        black_box(&rows);
        drop(rows);
    }
    for (body_len, received) in [(0, false), (2048, false), (8192, false), (usize::MAX, false), (2048, true)] {
        let base = LIVE.load(Ordering::SeqCst);
        let mut values = BTreeMap::new();
        let mut sizes = BTreeMap::new();
        let mut lru = BTreeMap::new();
        let mut charged = 0;
        let mut receipts = BTreeMap::new();
        for n in 0..N {
            let name = format!("project:inbox/role:message-{n}");
            let mut value = spec();
            value.body = if body_len == usize::MAX {
                use flotilla_state_machines::machines::convoy::*;
                flotilla_schema::Frozen::new(MessageBody::Request(RequestBody::Convoy(ConvoyRequest {
                    project: address("project:test"),
                    promises: [(PromiseId(0), Promise { kind: "opaque".into(), target: inbox() })].into(),
                    quota: [(VesselClass(0), 2)].into(),
                    priority: Priority(1),
                })))
                .unwrap()
            } else {
                MessageBody::text("x".repeat(body_len)).unwrap()
            };
            let value = Resource::<Message>::stored(
                &name,
                value,
                flotilla_schema::State::new(MessageState::Pending, flotilla_schema::Timestamp(10)),
                Version(1),
            );
            let rows =
                if received { Some([(consumer("a"), flotilla_state_machines::Transport::Turn)].into_iter().collect()) } else { None };
            let charge = charges::resource(&value, rows.as_ref());
            if let Some(rows) = rows {
                receipts.insert(name.clone(), rows);
            }
            charged += charge;
            sizes.insert(name.clone(), (charge, n as u64));
            lru.insert(n as u64, name.clone());
            values.insert(name, value);
        }
        let actual = (LIVE.load(Ordering::SeqCst) - base) as usize / N;
        eprintln!("calibrate resident body={body_len} received={received} allocated={actual} charge={}", charged / N);
        assert!(charged / N >= actual, "charge covers Home + metadata + LRU and distinct bodies");
        black_box((&values, &sizes, &lru, &receipts));
        drop((values, sizes, lru, receipts));
    }
    for bound in [false, true] {
        let base = LIVE.load(Ordering::SeqCst);
        let mut rows = BTreeMap::new();
        let mut charged = 0;
        for n in 0..N {
            let inbox = address(&format!("project:test/role:inbox-{n}"));
            let value = if bound {
                InboxBinding::Bound { consumers: [consumer("a")].into() }
            } else {
                InboxBinding::Unbound { since: flotilla_schema::Timestamp(10) }
            };
            charged += charges::binding(&inbox, &value);
            rows.insert(inbox, (value, 0usize));
        }
        let actual = (LIVE.load(Ordering::SeqCst) - base) as usize / N;
        eprintln!("calibrate binding bound={bound} allocated={actual} charge={}", charged / N);
        assert!(charged / N >= actual, "binding includes its map allocation");
        black_box(&rows);
        drop(rows);
    }
    // Actual ceiling cardinality, independent of Message bodies and SQLite caches.
    let base = LIVE.load(Ordering::SeqCst);
    let mut pending: BTreeMap<_, BTreeMap<u64, String>> = BTreeMap::new();
    let mut charged = 0usize;
    for n in 0..500_000 {
        let inbox = address(&format!("project:test/role:inbox-{}", n % 46_000));
        let name = format!("project:inbox/role:message-{n:064}");
        charged += charges::pending(&inbox, &name, !pending.contains_key(&inbox));
        pending.entry(inbox).or_default().insert(n as u64 + 1, name);
    }
    let actual = (LIVE.load(Ordering::SeqCst) - base) as usize;
    eprintln!("calibrate ceiling Pending entries=500000 inboxes=46000 allocated={actual} charge={charged} cap={}", charges::PENDING_BUDGET);
    assert!(charged >= actual && charged <= charges::PENDING_BUDGET);
    black_box(&pending);
    drop(pending);
    eprintln!("resource_header_bytes={}", size_of::<Resource<Message>>());
}
