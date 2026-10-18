//! Process-wide Rust live-allocation measurement: kept in its own test target.
//! SQLite's authoritative data/pages use its native allocator and are excluded.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        atomic::{AtomicIsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use flotilla_inbox::{
    sqlite::{SqliteInbox, StoreLog},
    InboxBackend, Rules,
};
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
// equal block of open messages cannot grow retained Rust memory with history.
// This fixed workload measures allocation/rate behavior, not gate logic.
#[tokio::test(flavor = "current_thread")]
async fn bounded_resident_allocation_workload() {
    const BUDGET: usize = 512 * 1024;
    // Small, explicitly bounded source caches isolate the replica plateau.
    // Production Store defaults have the same two-generation policy at 65,536.
    let store = Arc::new(Mutex::new(
        Store::open_with(
            Connection::open_in_memory().expect("db"),
            "test",
            flotilla_state_machines::kinds().expect("registry"),
            StoreOptions { string_cache: 128, identity_cache: 128, ..StoreOptions::default() },
        )
        .expect("store"),
    ));
    let evidence = Arc::new(Evidence::default());
    let log = StoreLog::new(store.clone());
    let backend = SqliteInbox::open_with_budget("test", log.clone(), evidence, Arc::new(Supervisors), BUDGET).expect("open");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store)));
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut retained = Vec::new();
    let start = Instant::now();
    for block in 0..3 {
        for n in 0..2000 {
            let mut value = spec();
            value.body = MessageBody::text(format!("{block}/{n}:{}", "x".repeat(2048))).expect("body");
            backend.enqueue(value).await.expect("enqueue");
        }
        let residency = backend.residency_blocking();
        assert!(residency.accounted_bytes <= BUDGET, "{residency:?}");
        retained.push(LIVE.load(Ordering::SeqCst));
        eprintln!("messages={} retained_rust_bytes={} residency={residency:?}", (block + 1) * 2000, retained[block]);
    }
    let enqueue_time = start.elapsed();
    assert!(retained[2] - retained[1] < BUDGET as isize, "resident allocations plateau: {retained:?}");
    let start = Instant::now();
    log.reset_read_counts();
    let mut pending = backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending");
    let mut count = 0;
    while pending.try_next().await.expect("stream").is_some() {
        count += 1;
    }
    assert_eq!(count, 6000);
    eprintln!(
        "enqueue_6000_ms={} cold_pending_6000_ms={} reads={:?}",
        enqueue_time.as_millis(),
        start.elapsed().as_millis(),
        log.read_counts()
    );
}
