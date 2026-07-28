use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    },
};

use chrono::Utc;
use flotilla_resources::{apply_status_patch_checked, Host, HostSpec, HostStatusPatch, InMemoryBackend, InputMeta, ResourceBackend};
use tokio::sync::Notify;

fn host_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: Default::default(),
        annotations: Default::default(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

fn heartbeat_patch() -> HostStatusPatch {
    HostStatusPatch::Heartbeat {
        capabilities: BTreeMap::new(),
        heartbeat_at: Utc::now(),
        ready: true,
        resource_store: None,
        daemon_generation: None,
        daemon_version: None,
        daemon_started_at: None,
        disk_free_bytes: None,
        conditions: Vec::new(),
    }
}

/// Regression test for a heartbeat write losing an optimistic-concurrency race against a
/// concurrent status writer and skipping its tick (flotilla-org/flotilla#1219). A plain
/// read-then-`update_status` write returns a `Conflict` and drops the tick when another
/// writer lands in between; `apply_status_patch` must retry against the freshly observed
/// resourceVersion instead, so the heartbeat still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_patch_retries_past_a_concurrent_status_writer_instead_of_skipping_the_tick() {
    let hosts = ResourceBackend::InMemory(InMemoryBackend::default()).using::<Host>("flotilla");
    hosts.create(&host_meta("dinghy"), &HostSpec {}).await.expect("create host should succeed");

    let concurrent_writer_started = Arc::new(Notify::new());
    let concurrent_writer_finished = Arc::new(Barrier::new(2));
    let concurrent_writer_triggered = Arc::new(AtomicBool::new(false));

    let concurrent_writer = {
        let hosts = hosts.clone();
        let concurrent_writer_started = Arc::clone(&concurrent_writer_started);
        let concurrent_writer_finished = Arc::clone(&concurrent_writer_finished);
        tokio::spawn(async move {
            concurrent_writer_started.notified().await;
            let current = hosts.get("dinghy").await.expect("concurrent get should succeed");
            let status = current.status.clone().unwrap_or_default();
            hosts
                .update_status("dinghy", &current.metadata.resource_version, &status)
                .await
                .expect("concurrent status write should succeed");
            concurrent_writer_finished.wait();
        })
    };

    let patch = heartbeat_patch();
    let HostStatusPatch::Heartbeat { heartbeat_at, .. } = &patch;
    let heartbeat_at = *heartbeat_at;

    // `check` runs at the start of every attempt (initial and post-conflict retries), the same
    // point a real concurrent writer would race against. The first attempt blocks until the
    // concurrent writer has landed its update against the resourceVersion this attempt already
    // fetched, guaranteeing the first write attempt observes a stale version and conflicts.
    let updated = apply_status_patch_checked(&hosts, "dinghy", &patch, {
        let concurrent_writer_started = Arc::clone(&concurrent_writer_started);
        let concurrent_writer_finished = Arc::clone(&concurrent_writer_finished);
        let concurrent_writer_triggered = Arc::clone(&concurrent_writer_triggered);
        move |_current| {
            if !concurrent_writer_triggered.swap(true, Ordering::SeqCst) {
                concurrent_writer_started.notify_one();
                concurrent_writer_finished.wait();
            }
            Ok(())
        }
    })
    .await;

    concurrent_writer.await.expect("concurrent writer task should finish");

    let updated = updated.expect("heartbeat patch should retry past the conflict instead of failing the tick");
    assert_eq!(updated.status.expect("heartbeat status").heartbeat_at, Some(heartbeat_at));
}
