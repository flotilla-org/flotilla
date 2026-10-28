//! A separate binary isolates the counting allocator from other integration tests.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use flotilla_schema::{KindRegistry, NewChange, Stored, Timestamp};
use flotilla_schema_sqlite::Store;
use rusqlite::Connection;

struct CountingAllocator;
thread_local! {
    static BYTES: Cell<Option<usize>> = const { Cell::new(None) };
}
fn record(bytes: usize) {
    let _ = BYTES.try_with(|count| {
        if let Some(total) = count.get() {
            count.set(Some(total + bytes));
        }
    });
}
// Test-only delegation to System; thread-local accounting excludes the test runner's threads.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Stored)]
#[store(kind = "LookupProbe", code = "lp")]
struct Probe {
    value: u8,
}

// Behaviour (review #1): a warm string-id lookup borrows its text. A scan still allocates its
// result page, but must not copy a long home string just to probe the committed cache.
#[test]
fn a_warm_scan_does_not_allocate_its_string_lookup_key() {
    let home = "h".repeat(16 * 1024);
    let mut kinds = KindRegistry::new();
    kinds.register::<Probe>().expect("register");
    let mut store = Store::open(Connection::open_in_memory().expect("connection"), &home, kinds).expect("store");
    let mut write = store.write().expect("write");
    write.insert("p", &Probe { value: 1 }, &NewChange::new("w", Timestamp(0))).expect("insert");
    write.commit().expect("commit");
    let mut read = store.read().expect("read");
    read.scan::<Probe>(&home, None, 1).expect("warm statements and cache");
    BYTES.with(|bytes| bytes.set(Some(0)));
    let page = read.scan::<Probe>(&home, None, 1).expect("scan");
    let bytes = BYTES.with(|bytes| bytes.replace(None)).expect("measurement enabled");
    assert_eq!(page.ids.len(), 1);
    assert!(bytes < home.len(), "scan allocated {bytes} bytes; lookup copied the {}-byte home", home.len());
}
