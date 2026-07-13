use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Barrier,
};

use flotilla_resources::{ApiPaths, InMemoryBackend, InputMeta, Resource, ResourceBackend, ResourceError, StatusPatch};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy)]
struct CounterResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CounterSpec {
    name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CounterStatus {
    value: u32,
    note: Option<String>,
}

enum CounterPatch {
    Increment,
    IncrementAfterConcurrentUpdate { update_started: Arc<Notify>, update_finished: Arc<Barrier>, update_triggered: Arc<AtomicBool> },
    SetNote(&'static str),
}

impl Resource for CounterResource {
    type Spec = CounterSpec;
    type Status = CounterStatus;
    type StatusPatch = CounterPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "counters", kind: "Counter" };
}

impl StatusPatch<CounterStatus> for CounterPatch {
    fn apply(&self, status: &mut CounterStatus) {
        match self {
            Self::Increment => status.value += 1,
            Self::IncrementAfterConcurrentUpdate { update_started, update_finished, update_triggered } => {
                if !update_triggered.swap(true, Ordering::SeqCst) {
                    update_started.notify_one();
                    update_finished.wait();
                }
                status.value += 1;
            }
            Self::SetNote(note) => status.note = Some((*note).to_string()),
        }
    }
}

fn counter_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: Default::default(),
        annotations: Default::default(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

fn counter_spec(name: &str) -> CounterSpec {
    CounterSpec { name: name.to_string() }
}

#[tokio::test]
async fn apply_status_patch_updates_existing_status() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<CounterResource>("flotilla");
    let created = resolver.create(&counter_meta("alpha"), &counter_spec("alpha")).await.expect("create should succeed");
    let current = resolver
        .update_status("alpha", &created.metadata.resource_version, &CounterStatus { value: 1, note: None })
        .await
        .expect("seed status should succeed");

    let updated =
        flotilla_resources::apply_status_patch(&resolver, "alpha", &CounterPatch::Increment).await.expect("status patch should succeed");

    assert_eq!(updated.status.expect("status"), CounterStatus { value: 2, note: None });
    assert_eq!(updated.metadata.resource_version, "3");
    assert_eq!(current.metadata.resource_version, "2");
}

#[tokio::test]
async fn apply_status_patch_initializes_missing_status_from_default() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<CounterResource>("flotilla");
    let created = resolver.create(&counter_meta("beta"), &counter_spec("beta")).await.expect("create should succeed");

    let updated = flotilla_resources::apply_status_patch(&resolver, "beta", &CounterPatch::SetNote("ready"))
        .await
        .expect("status patch should succeed");

    assert_eq!(created.status, None);
    assert_eq!(updated.status.expect("status"), CounterStatus { value: 0, note: Some("ready".to_string()) });
    assert_eq!(updated.metadata.resource_version, "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_status_patch_revalidates_after_conflict_before_retrying() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<CounterResource>("flotilla");
    let created = resolver.create(&counter_meta("gamma"), &counter_spec("gamma")).await.expect("create should succeed");
    resolver
        .update_status("gamma", &created.metadata.resource_version, &CounterStatus { value: 1, note: None })
        .await
        .expect("seed status should succeed");

    let update_started = Arc::new(Notify::new());
    let update_finished = Arc::new(Barrier::new(2));
    let update_triggered = Arc::new(AtomicBool::new(false));
    let concurrent_update = {
        let resolver = resolver.clone();
        let update_started = Arc::clone(&update_started);
        let update_finished = Arc::clone(&update_finished);
        tokio::spawn(async move {
            update_started.notified().await;
            let current = resolver.get("gamma").await.expect("concurrent get should succeed");
            resolver
                .update_status("gamma", &current.metadata.resource_version, &CounterStatus { value: 1, note: Some("terminal".to_string()) })
                .await
                .expect("concurrent status update should succeed");
            update_finished.wait();
        })
    };

    let result = flotilla_resources::apply_status_patch_checked(
        &resolver,
        "gamma",
        &CounterPatch::IncrementAfterConcurrentUpdate { update_started, update_finished, update_triggered },
        |current| match current.status.as_ref() {
            Some(status) if status.note.as_deref() == Some("terminal") => Err(ResourceError::other("counter is terminal")),
            _ => Ok(()),
        },
    )
    .await;
    concurrent_update.await.expect("concurrent task should finish");

    assert_eq!(result.expect_err("retry should reject the concurrent terminal state"), ResourceError::other("counter is terminal"));
    let current = resolver.get("gamma").await.expect("final get should succeed");
    assert_eq!(current.status.expect("status"), CounterStatus { value: 1, note: Some("terminal".to_string()) });
}
