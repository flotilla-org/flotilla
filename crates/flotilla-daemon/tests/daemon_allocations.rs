//! In-process daemon memory scenario for #3101; isolated global allocator target.
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::{HostName, NodeId, QueryCursor, QueryId};
use flotilla_resources::{Convoy, ConvoySpec, ConvoyStatus, InputMeta};
use flotilla_store::{ResourceBackend, ViewSnapshot, WatchView};
use flotilla_store_memory::InMemoryBackend;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct CountingAllocator;
static LIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
            LIVE_COUNT.fetch_add(1, Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        LIVE_COUNT.fetch_sub(1, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            LIVE.fetch_add(size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
fn counters() -> (usize, usize, usize) {
    (LIVE.load(Ordering::Relaxed), CALLS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed))
}

// Real daemon, store, leaf/stall engine and query subscription registry. Vary N,
// M and K independently; status carries a 16 KiB diagnostic plus workflow data.
// Steady passes must not retain history and each subscriber must have a small,
// inventory-independent cost. One test owns the allocator to avoid interference.
#[test]
fn daemon_memory_scaling() {
    let mut churn_violations = Vec::new();
    for n in [0, 32, 224] {
        // Runtime teardown cancels every daemon background task before the next
        // inventory size, so earlier daemons cannot skew later live-byte counts.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        churn_violations.extend(runtime.block_on(memory_scenario(n)));
    }
    assert!(churn_violations.is_empty(), "steady inventory churn: {churn_violations:?}");
}

async fn memory_scenario(n: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut churn_violations = Vec::new();
    let scenario_start = counters();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"memory-scenario\"\n").unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("memory-scenario"));
    let resolver = backend.using::<Convoy>("flotilla");
    for i in 0..n {
        let object = resolver
            .create(
                &InputMeta::builder().name(format!("convoy-{i:04}")).build(),
                &ConvoySpec::builder().workflow_ref("single-agent".into()).build(),
            )
            .await
            .unwrap();
        let status: ConvoyStatus = serde_json::from_value(serde_json::json!({"phase": if i % 8 == 0 { "Active" } else { "Landed" }, "message":"diagnostic ".repeat(1500), "workflow_snapshot": {"vessels": flotilla_resources::single_agent_workflow_spec().vessels}})).unwrap();
        resolver.update_status(&object.metadata.name, &object.metadata.resource_version, &status).await.unwrap();
    }
    eprintln!("N={n} owner=store live_delta={}", counters().0.saturating_sub(scenario_start.0));
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("memory-scenario"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    eprintln!("N={n} owner=daemon live={}", counters().0);
    let mut bootstrap_events = daemon.subscribe();
    let state = daemon.aggregator_projection_state().await;
    let aggregate_daemon = daemon.clone();
    let aggregate_state = state.clone();
    let aggregator = tokio::spawn(async move {
        flotilla_projections::run(
            flotilla_projections::AggregatorContext {
                durable: aggregate_daemon.resource_backend(),
                observed: aggregate_daemon.observed_resource_backend(),
                host_name: aggregate_daemon.host_name().clone(),
                event_sink: aggregate_daemon.event_sink(),
                events: aggregate_daemon.subscribe(),
                attach: aggregate_daemon.clone(),
                change_requests: aggregate_daemon.clone(),
                issues: aggregate_daemon.clone(),
            },
            "flotilla",
            aggregate_state,
            Default::default(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        // Its bootstrap ResultSet is emitted after all watched kinds are
        // installed, including an empty convoy inventory. No timing warmup.
        loop {
            let event = bootstrap_events.recv().await.expect("daemon bootstrap events");
            if let flotilla_protocol::DaemonEvent::ResultSet(set) = event {
                if set.query() == (QueryId::Convoys { scope: None }) {
                    assert_eq!(set.rows.len(), n);
                    break;
                }
            }
        }
    })
    .await
    .expect("aggregator bootstrap");
    tokio::task::yield_now().await;
    drop(bootstrap_events);
    assert!(!aggregator.is_finished(), "aggregator must be running");
    eprintln!("N={n} owner=aggregator live={}", counters().0);
    daemon.reconcile_crew_stalls_once("flotilla").await.unwrap();
    eprintln!("N={n} owner=leaf_views live={}", counters().0);
    tokio::task::yield_now().await;
    let mut tokens = Vec::new();
    for m in [1, 8, 32] {
        let before = counters();
        while tokens.len() < m {
            let id = uuid::Uuid::new_v4();
            tokens.push(daemon.query_subscription(id));
            drop(daemon.subscribe_queries(id, &[QueryCursor { query: QueryId::Convoys { scope: None }, since: None }]).await.unwrap());
        }
        tokio::task::yield_now().await;
        let subscribed = counters();
        eprintln!("N={n} M={m} subscriber_live_delta={}", subscribed.0.saturating_sub(before.0));
        // These count requested Rust bytes, independent of allocator rounding
        // and RSS. Budgets leave ample room above measured bookkeeping while
        // rejecting an additional inventory at the populated fixture sizes.
        assert!(subscribed.0.saturating_sub(before.0) < 256 * 1024, "subscriber must not retain another inventory");
        for k in [1, 8, 32] {
            let start = counters();
            for _ in 0..k {
                daemon.reconcile_crew_stalls_once("flotilla").await.unwrap();
            }
            tokio::task::yield_now().await;
            let end = counters();
            eprintln!("N={n} M={m} K={k} live_allocations={}", LIVE_COUNT.load(Ordering::Relaxed));
            eprintln!(
                "N={n} M={m} K={k} live={} live_delta={} calls={} requested={}",
                end.0,
                end.0 as isize - start.0 as isize,
                end.1 - start.1,
                end.2 - start.2
            );
            assert!(end.0 <= subscribed.0 + 64 * 1024, "all steady pass batches must share one fixed retention budget");
            // No full inventory clone/JSON rebuild in a steady pass. The
            // absolute budget is independent of N and leaves runtime slack.
            if end.2 - start.2 > k * 256 * 1024 {
                churn_violations.push((n, m, k, end.2 - start.2));
            }
        }
    }
    allocator_stats(n);
    // Measure the retaining owner directly: many readers of a watched
    // generation keep one inventory and one maintained serialized partition.
    let start = counters();
    let view = WatchView::<Convoy>::start(backend, "flotilla").await.unwrap();
    eprintln!("N={n} owner=watch_indexed_state live_delta={}", counters().0.saturating_sub(start.0));
    let mut warm = ViewSnapshot::new(Some(NodeId::new("memory-scenario")));
    let start = counters();
    let inventory = view.shared_list().await.unwrap();
    eprintln!("N={n} owner=shared_typed_generation live_delta={}", counters().0.saturating_sub(start.0));
    let start = counters();
    view.insert_snapshot("flotilla", &mut warm).await.unwrap();
    eprintln!("N={n} owner=maintained_serialized_partition live_delta={}", counters().0.saturating_sub(start.0));
    let start = counters();
    let mut readers = Vec::new();
    for _ in 0..32 {
        let mut snapshot = ViewSnapshot::new(Some(NodeId::new("memory-scenario")));
        let listed = view.snapshot("flotilla", &mut snapshot).await.unwrap();
        assert!(Arc::ptr_eq(&inventory, &listed));
        readers.push((snapshot, listed));
    }
    let retained = counters().0.saturating_sub(start.0);
    eprintln!("N={n} owner=32_snapshot_readers live_delta={retained}");
    assert!(retained < 64 * 1024, "snapshot readers must share inventory bodies and indexes");
    drop(readers);
    drop(tokens);
    aggregator.abort();
    drop(aggregator);
    drop(daemon);
    tokio::task::yield_now().await;
    churn_violations
}

// glibc exposes allocator-held free bytes separately from Rust's requested
// live bytes. This host-side measurement uses no daemon sockets/live hosts.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_stats(n: usize) {
    #[repr(C)]
    struct MallInfo {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    unsafe extern "C" {
        fn mallinfo2() -> MallInfo;
    }
    // mallinfo2 returns process-wide counters and requires no ownership input.
    let info = unsafe { mallinfo2() };
    eprintln!(
        "N={n} allocator_arena={} allocator_used={} allocator_free={} mmap={}",
        info.arena, info.uordblks, info.fordblks, info.hblkhd
    );
}
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_stats(_n: usize) {}
