use std::time::Duration;

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    delete_resource_kind, EventRetention, Host, HostSpec, InMemoryBackend, InputMeta, LifecycleAuthority, OwnerGarbageCollector,
    OwnerReference, ResourceBackend, ResourceError, SqliteBackend, WatchEvent, WatchStart,
};
use futures::StreamExt;

const NS: &str = "gc-test";

fn meta(name: &str, owner: Option<&str>) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .owner_references(
            owner
                .into_iter()
                .map(|name| OwnerReference {
                    api_version: "flotilla.work/v1".to_string(),
                    kind: "Host".to_string(),
                    name: name.to_string(),
                    controller: true,
                })
                .collect(),
        )
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Managed)
}

async fn create(backend: &ResourceBackend, meta: InputMeta) {
    backend.using::<Host>(NS).create(&meta, &HostSpec::default()).await.expect("create host");
}

async fn deleted(watch: &mut flotilla_resources::WatchStream<Host>, name: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Deleted(object) if object.metadata.name == name) {
                break;
            }
        }
    }).await.expect("reactive deletion without an hourly sweep");
}

async fn start(backend: &ResourceBackend) -> tokio::task::JoinHandle<Result<(), ResourceError>> {
    let mut watch = backend.using::<Host>(NS).watch(WatchStart::Now).await.expect("watch startup");
    create(backend, meta("startup-orphan", Some("missing"))).await;
    let collector = OwnerGarbageCollector::new(backend.clone(), NS);
    let mut task = tokio::spawn(async move { collector.run(Duration::from_secs(3600)).await });
    // Startup recovery deletes this marker only after all watches are established.
    tokio::select! {
        _ = deleted(&mut watch, "startup-orphan") => {},
        result = &mut task => panic!("collector stopped: {result:?}"),
    }
    task
}

/// A deletion after the preceding writes on this kind confirms that the
/// collector has consumed those watch events before assertions inspect it.
async fn flush(backend: &ResourceBackend, marker: &str) {
    let mut watch = backend.using::<Host>(NS).watch(WatchStart::Now).await.expect("watch marker");
    create(backend, meta(marker, Some("missing"))).await;
    deleted(&mut watch, marker).await;
}

async fn index_maintenance_contract(backend: ResourceBackend) {
    create(&backend, meta("first-owner", None)).await;
    create(&backend, meta("second-owner", None)).await;
    create(&backend, meta("moving-child", Some("first-owner"))).await;
    create(&backend, meta("removed-child", Some("first-owner"))).await;
    let task = start(&backend).await;
    let hosts = backend.using::<Host>(NS);

    let moving = hosts.get("moving-child").await.expect("moving child");
    hosts
        .update(&meta("moving-child", Some("second-owner")), &moving.metadata.resource_version, &moving.spec)
        .await
        .expect("reparent child");
    hosts.delete("removed-child").await.expect("delete indexed child");
    flush(&backend, "after-reparent").await;
    hosts.delete("first-owner").await.expect("delete old owner");
    flush(&backend, "after-old-owner").await;
    assert!(hosts.get("moving-child").await.is_ok(), "old owner must not collect reparented child");

    let mut watch = hosts.watch(WatchStart::Now).await.expect("watch moving child");
    hosts.delete("second-owner").await.expect("delete current owner");
    deleted(&mut watch, "moving-child").await;

    let mut watch = hosts.watch(WatchStart::Now).await.expect("watch late child");
    create(&backend, meta("late-child", Some("second-owner"))).await;
    deleted(&mut watch, "late-child").await;

    task.abort();
    let _ = task.await;
    create(&backend, meta("restart-orphan", Some("second-owner"))).await;
    let task = start(&backend).await;
    assert!(matches!(hosts.get("restart-orphan").await, Err(ResourceError::NotFound { .. })), "restart sweep catches missed writes");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn memory_index_maintenance() {
    index_maintenance_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_index_maintenance() {
    index_maintenance_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}

async fn expired_cursor_recovery_contract(backend: ResourceBackend) {
    let hosts = backend.using::<Host>(NS);
    let first = hosts.create(&meta("old-event", None), &HostSpec::default()).await.expect("first event");
    create(&backend, meta("new-event", None)).await;
    create(&backend, meta("orphan-after-gap", Some("missing-owner"))).await;
    assert!(matches!(hosts.watch(WatchStart::FromVersion(first.metadata.resource_version)).await, Err(ResourceError::WatchExpired { .. })));
    let task = start(&backend).await;
    assert!(matches!(hosts.get("orphan-after-gap").await, Err(ResourceError::NotFound { .. })));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn memory_expired_cursor_recovery() {
    let retention = EventRetention::new(1).expect("retention");
    expired_cursor_recovery_contract(ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention))).await;
}

#[tokio::test]
async fn sqlite_expired_cursor_recovery() {
    let retention = EventRetention::new(1).expect("retention");
    expired_cursor_recovery_contract(ResourceBackend::Sqlite(
        SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite"),
    ))
    .await;
}

/// Manual cost check at a fleet-sized namespace. Startup still relists, while
/// the measured steady-state phase deletes leaves with no controlled children.
#[tokio::test]
#[ignore = "manual garbage collection cost measurement"]
async fn leaf_deletion_cost_at_fleet_size() {
    for (label, backend) in [
        ("memory", ResourceBackend::InMemory(InMemoryBackend::default())),
        ("sqlite", ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))),
    ] {
        for index in 0..1_000 {
            create(&backend, meta(&format!("unrelated-{index}"), None)).await;
        }
        let task = start(&backend).await;
        let begin = std::time::Instant::now();
        for index in 0..100 {
            backend.using::<Host>(NS).delete(&format!("unrelated-{index}")).await.expect("delete leaf");
        }
        flush(&backend, "benchmark-flush").await;
        println!("{label}: 100 leaf deletions among 1000 resources: {:?}", begin.elapsed());
        task.abort();
        let _ = task.await;
    }
}

async fn cascade_contract(backend: ResourceBackend) {
    create(&backend, meta("owner", None).with_added_finalizer("test/owner")).await;
    create(&backend, meta("child", Some("owner")).with_added_finalizer("test/child")).await;
    create(&backend, meta("grandchild", Some("child"))).await;
    create(&backend, meta("adopted", Some("owner")).with_lifecycle_authority(LifecycleAuthority::Adopted)).await;
    let mut noncontroller = meta("noncontroller", Some("owner"));
    noncontroller.owner_references[0].controller = false;
    create(&backend, noncontroller).await;
    let mut foreign = meta("foreign-api", Some("owner"));
    foreign.owner_references[0].api_version = "foreign/v1".to_string();
    create(&backend, foreign).await;
    let other_namespace = backend.using::<Host>("elsewhere");
    other_namespace.create(&meta("other-namespace", Some("owner")), &HostSpec::default()).await.expect("other namespace");
    let task = start(&backend).await;
    let hosts = backend.using::<Host>(NS);
    let mut watch = hosts.watch(WatchStart::Now).await.expect("watch cascade");
    delete_resource_kind(&backend, NS, "Host", "owner").await.expect("request raw deletion");
    let owner = hosts.get("owner").await.expect("owner finalizer retained");
    assert!(owner.metadata.is_pending_finalization());
    assert!(hosts.get("child").await.expect("child still present").metadata.deletion_timestamp.is_none());
    hosts
        .update(&InputMeta::from(&owner.metadata).without_finalizer("test/owner"), &owner.metadata.resource_version, &owner.spec)
        .await
        .expect("owner finalizer finishes");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(watch.next().await.expect("watch open").expect("watch event"), WatchEvent::Modified(object) if object.metadata.name == "child" && object.metadata.is_pending_finalization()) {
                break;
            }
        }
    }).await.expect("child reactively enters finalization");
    let child = hosts.get("child").await.expect("child finalizer retained");
    assert!(hosts.get("grandchild").await.is_ok());
    hosts
        .update(&InputMeta::from(&child.metadata).without_finalizer("test/child"), &child.metadata.resource_version, &child.spec)
        .await
        .expect("child finalizer finishes");
    deleted(&mut watch, "grandchild").await;
    for name in ["adopted", "noncontroller", "foreign-api"] {
        assert!(hosts.get(name).await.is_ok(), "preserve {name}");
    }
    assert!(other_namespace.get("other-namespace").await.is_ok());
    let settled_version = hosts.list().await.expect("settled resources").resource_version;
    OwnerGarbageCollector::new(backend.clone(), NS).sweep().await.expect("backstop after reactive cleanup");
    assert_eq!(hosts.list().await.expect("resources after backstop").resource_version, settled_version, "backstop normally has no work");
    task.abort();
    let _ = task.await;
}

async fn replica_contract(backend: ResourceBackend) {
    let origin = ResourceBackend::InMemory(InMemoryBackend::default());
    create(&origin, meta("remote-owner", None)).await;
    let owner_root = NodeId::new("owner-root");
    let writer = backend.replica_writer::<Host>(owner_root, NS);
    writer.replace(&origin.using::<Host>(NS).list().await.expect("owner snapshot"), Utc::now()).await.expect("replicate owner");
    create(&backend, meta("local-child", Some("remote-owner"))).await;
    create(&origin, meta("remote-child", Some("remote-owner"))).await;
    writer.replace(&origin.using::<Host>(NS).list().await.expect("snapshot"), Utc::now()).await.expect("replicate child");
    let task = start(&backend).await;
    let mut watch = backend.using::<Host>(NS).watch(WatchStart::Now).await.expect("watch local child");
    let mut origin_watch = origin.using::<Host>(NS).watch(WatchStart::Now).await.expect("watch owner");
    origin.using::<Host>(NS).delete("remote-owner").await.expect("delete remote owner");
    writer.apply(origin_watch.next().await.expect("delete event").expect("event"), Utc::now()).await.expect("replicate deletion");
    deleted(&mut watch, "local-child").await;
    assert!(backend.including_replicas::<Host>(NS).get("remote-child").await.is_ok(), "replica is never mutated locally");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn memory_cascade() {
    cascade_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}
#[tokio::test]
async fn sqlite_cascade() {
    cascade_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}
#[tokio::test]
async fn memory_replica() {
    replica_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}
#[tokio::test]
async fn sqlite_replica() {
    replica_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}

async fn vessel_finalizer_contract(backend: ResourceBackend) {
    use flotilla_controllers::reconcilers::VesselReconciler;
    use flotilla_resources::{
        controller::ControllerLoop, TerminalSession, TerminalSessionSource, TerminalSessionSpec, Vessel, VesselSpec, VESSEL_REF_LABEL,
    };

    create(&backend, meta("convoy-surrogate", None)).await;
    let vessels = backend.using::<Vessel>(NS);
    vessels
        .create(
            &meta("vessel", Some("convoy-surrogate")).with_added_finalizer("flotilla.work/vessel-workspace-teardown"),
            &VesselSpec {
                convoy_ref: "convoy-surrogate".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "unused".to_string(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("create vessel");
    let terminals = backend.using::<TerminalSession>(NS);
    // A legacy label-only child proves the Vessel's finalizer actually ran;
    // the generic collector cannot delete this terminal itself.
    let mut terminal_meta = meta("running-terminal", None);
    terminal_meta.labels.insert(VESSEL_REF_LABEL.to_string(), "vessel".to_string());
    terminals
        .create(
            &terminal_meta,
            &TerminalSessionSpec {
                env_ref: "unused".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "true".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "test".to_string(),
            },
        )
        .await
        .expect("create terminal");
    let gc = start(&backend).await;
    let mut vessel_watch = vessels.watch(WatchStart::Now).await.expect("watch vessel");
    backend.using::<Host>(NS).delete("convoy-surrogate").await.expect("delete owner");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(vessel_watch.next().await.expect("watch open").expect("event"), WatchEvent::Modified(vessel) if vessel.metadata.is_pending_finalization()) {
                break;
            }
        }
    }).await.expect("cascade requests finalization");
    assert!(terminals.get("running-terminal").await.is_ok());
    let controller = tokio::spawn(
        ControllerLoop {
            primary: vessels.clone(),
            secondaries: Vec::new(),
            reconciler: VesselReconciler::new(backend.clone(), NS),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(vessel_watch.next().await.expect("watch open").expect("event"), WatchEvent::Deleted(_)) {
                break;
            }
        }
    })
    .await
    .expect("vessel finalizes reactively");
    assert!(matches!(terminals.get("running-terminal").await, Err(ResourceError::NotFound { .. })));
    gc.abort();
    controller.abort();
    let _ = gc.await;
    let _ = controller.await;
}

#[tokio::test]
async fn memory_vessel_finalizer() {
    vessel_finalizer_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}
#[tokio::test]
async fn sqlite_vessel_finalizer() {
    vessel_finalizer_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}
