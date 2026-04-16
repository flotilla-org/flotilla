mod common;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use common::{resource_meta, TestLoopHarness};
use flotilla_resources::{
    controller::{Actuation, ControllerLoop, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler},
    ApiPaths, InMemoryBackend, InputMeta, NoStatusPatch, Presentation, PresentationSpec, Resource, ResourceBackend, ResourceError,
    ResourceObject, TaskWorkspace, TaskWorkspaceSpec,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::timeout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrimaryResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PrimarySpec {
    value: String,
}

impl Resource for PrimaryResource {
    type Spec = PrimarySpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "test-primaries", kind: "TestPrimary" };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SecondaryResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SecondarySpec {
    value: String,
}

impl Resource for SecondaryResource {
    type Spec = SecondarySpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "test-secondaries", kind: "TestSecondary" };
}

#[derive(Clone)]
struct RecordingReconciler {
    reconciled: Arc<Mutex<Vec<String>>>,
}

impl Reconciler for RecordingReconciler {
    type Resource = PrimaryResource;
    type Dependencies = ();

    async fn fetch_dependencies(&self, _obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        Ok(())
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        _deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        self.reconciled.lock().expect("reconciled lock").push(obj.metadata.name.clone());
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

#[derive(Clone)]
struct FinalizingReconciler {
    finalized: Arc<Mutex<Vec<String>>>,
}

impl Reconciler for FinalizingReconciler {
    type Resource = PrimaryResource;
    type Dependencies = ();

    async fn fetch_dependencies(&self, _obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        Ok(())
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        ReconcileOutcome::new(None)
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        self.finalized.lock().expect("finalized lock").push(obj.metadata.name.clone());
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/test-finalizer")
    }
}

#[derive(Clone)]
struct RestartingSecondaryWatch {
    spawns: Arc<AtomicUsize>,
}

impl flotilla_resources::controller::SecondaryWatch for RestartingSecondaryWatch {
    type Primary = PrimaryResource;

    fn clone_box(&self) -> Box<dyn flotilla_resources::controller::SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        _sender: mpsc::Sender<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct ActuatingReconciler {
    actuation: Actuation,
}

impl Reconciler for ActuatingReconciler {
    type Resource = PrimaryResource;
    type Dependencies = ();

    async fn fetch_dependencies(&self, _obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        Ok(())
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        ReconcileOutcome::with_actuations(None, vec![self.actuation.clone()])
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

fn primary_meta(name: &str) -> InputMeta {
    resource_meta().name(name).call()
}

fn secondary_meta(name: &str, primary: &str) -> InputMeta {
    resource_meta().name(name).labels([("flotilla.work/primary".to_string(), primary.to_string())].into_iter().collect()).call()
}

fn grouped_primary_meta(name: &str, group: &str) -> InputMeta {
    resource_meta().name(name).labels([("flotilla.work/group".to_string(), group.to_string())].into_iter().collect()).call()
}

fn grouped_secondary_meta(name: &str, group: &str) -> InputMeta {
    resource_meta().name(name).labels([("flotilla.work/group".to_string(), group.to_string())].into_iter().collect()).call()
}

#[tokio::test]
async fn controller_loop_reconciles_existing_primary_objects_from_initial_list() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: Vec::new(),
            reconciler: RecordingReconciler { reconciled: Arc::clone(&reconciled) },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if reconciled.lock().expect("reconciled lock").iter().any(|name| name == "alpha") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial list should reconcile alpha");

    harness.shutdown().await;
}

#[tokio::test]
async fn label_mapped_watch_enqueues_primary_named_in_secondary_label() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let secondaries = backend.clone().using::<SecondaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(LabelMappedWatch::<SecondaryResource, PrimaryResource> {
                label_key: "flotilla.work/primary",
                _marker: std::marker::PhantomData,
            })],
            reconciler: RecordingReconciler { reconciled: Arc::clone(&reconciled) },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if reconciled.lock().expect("reconciled lock").iter().filter(|name| *name == "alpha").count() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial list should reconcile alpha once");

    {
        let mut reconciled = reconciled.lock().expect("reconciled lock");
        reconciled.clear();
    }

    secondaries
        .create(&secondary_meta("secondary-a", "alpha"), &SecondarySpec { value: "wake".to_string() })
        .await
        .expect("secondary create should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let hits = reconciled.lock().expect("reconciled lock").iter().filter(|name| *name == "alpha").count();
            if hits >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("secondary watch should enqueue alpha");

    harness.shutdown().await;
}

#[tokio::test]
async fn label_join_watch_enqueues_each_primary_sharing_the_label_value() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let secondaries = backend.clone().using::<SecondaryResource>("flotilla");
    primaries
        .create(&grouped_primary_meta("alpha", "convoy-a"), &PrimarySpec { value: "one".to_string() })
        .await
        .expect("alpha create should succeed");
    primaries
        .create(&grouped_primary_meta("beta", "convoy-a"), &PrimarySpec { value: "two".to_string() })
        .await
        .expect("beta create should succeed");
    primaries
        .create(&grouped_primary_meta("gamma", "convoy-b"), &PrimarySpec { value: "three".to_string() })
        .await
        .expect("gamma create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(LabelJoinWatch::<SecondaryResource, PrimaryResource> {
                label_key: "flotilla.work/group",
                _marker: std::marker::PhantomData,
            })],
            reconciler: RecordingReconciler { reconciled: Arc::clone(&reconciled) },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let reconciled = reconciled.lock().expect("reconciled lock").clone();
            let alpha_hits = reconciled.iter().filter(|name| *name == "alpha").count();
            let beta_hits = reconciled.iter().filter(|name| *name == "beta").count();
            let gamma_hits = reconciled.iter().filter(|name| *name == "gamma").count();
            if alpha_hits >= 1 && beta_hits >= 1 && gamma_hits >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial list should reconcile each primary once");

    {
        let mut reconciled = reconciled.lock().expect("reconciled lock");
        reconciled.clear();
    }

    secondaries
        .create(&grouped_secondary_meta("secondary-a", "convoy-a"), &SecondarySpec { value: "wake".to_string() })
        .await
        .expect("secondary create should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let reconciled = reconciled.lock().expect("reconciled lock").clone();
            let alpha_hits = reconciled.iter().filter(|name| *name == "alpha").count();
            let beta_hits = reconciled.iter().filter(|name| *name == "beta").count();
            let gamma_hits = reconciled.iter().filter(|name| *name == "gamma").count();
            if alpha_hits >= 1 && beta_hits >= 1 && gamma_hits == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("join watch should wake both matching primaries and no non-matches");

    harness.shutdown().await;
}

#[tokio::test]
async fn duplicate_secondary_events_for_the_same_primary_are_deduped_per_burst() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let secondaries = backend.clone().using::<SecondaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(LabelMappedWatch::<SecondaryResource, PrimaryResource> {
                label_key: "flotilla.work/primary",
                _marker: std::marker::PhantomData,
            })],
            reconciler: RecordingReconciler { reconciled: Arc::clone(&reconciled) },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if reconciled.lock().expect("reconciled lock").iter().filter(|name| *name == "alpha").count() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial list should reconcile alpha once");

    {
        let mut reconciled = reconciled.lock().expect("reconciled lock");
        reconciled.clear();
    }

    let secondary_a_meta = secondary_meta("secondary-a", "alpha");
    let secondary_b_meta = secondary_meta("secondary-b", "alpha");
    let secondary_a_spec = SecondarySpec { value: "wake-a".to_string() };
    let secondary_b_spec = SecondarySpec { value: "wake-b".to_string() };
    let create_a = secondaries.create(&secondary_a_meta, &secondary_a_spec);
    let create_b = secondaries.create(&secondary_b_meta, &secondary_b_spec);
    let (_a, _b) = tokio::join!(create_a, create_b);

    timeout(Duration::from_secs(1), async {
        loop {
            if !reconciled.lock().expect("reconciled lock").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("secondary burst should wake alpha");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(reconciled.lock().expect("reconciled lock").as_slice(), &["alpha".to_string()]);

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_runs_finalizer_and_deletes_resource_after_finalizer_completion() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let meta = resource_meta()
        .name("alpha")
        .finalizers(vec!["flotilla.work/test-finalizer".to_string()])
        .deletion_timestamp(chrono::Utc::now())
        .call();
    primaries.create(&meta, &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let finalized = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: FinalizingReconciler { finalized: Arc::clone(&finalized) },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let finalized_hits = finalized.lock().expect("finalized lock").iter().filter(|name| *name == "alpha").count();
            if finalized_hits >= 1 && matches!(primaries.get("alpha").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("finalizer should run and then the deleting resource should disappear");

    assert!(matches!(primaries.get("alpha").await, Err(ResourceError::NotFound { .. })));

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_adds_finalizer_to_managed_resources() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: FinalizingReconciler { finalized: Arc::new(Mutex::new(Vec::new())) },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let object = primaries.get("alpha").await.expect("primary get should succeed");
            if object.metadata.finalizers == vec!["flotilla.work/test-finalizer".to_string()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller should attach its finalizer");

    harness.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn secondary_watch_restart_is_backed_off() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");

    let spawns = Arc::new(AtomicUsize::new(0));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(RestartingSecondaryWatch { spawns: Arc::clone(&spawns) })],
            reconciler: RecordingReconciler { reconciled: Arc::new(Mutex::new(Vec::new())) },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    tokio::task::yield_now().await;
    assert_eq!(spawns.load(Ordering::SeqCst), 1, "watch should start immediately");

    tokio::time::advance(Duration::from_millis(99)).await;
    tokio::task::yield_now().await;
    assert_eq!(spawns.load(Ordering::SeqCst), 1, "watch should not restart before the backoff elapses");

    tokio::time::advance(Duration::from_millis(1)).await;
    timeout(Duration::from_secs(1), async {
        loop {
            if spawns.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("secondary watch should restart once the backoff elapses");

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_applies_create_presentation_actuation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let presentations = backend.clone().using::<Presentation>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler {
                actuation: Actuation::CreatePresentation {
                    meta: resource_meta().name("alpha-presentation").call(),
                    spec: PresentationSpec {
                        convoy_ref: "alpha".to_string(),
                        presentation_policy_ref: "default".to_string(),
                        name: "alpha".to_string(),
                        process_selector: [("flotilla.work/convoy".to_string(), "alpha".to_string())].into_iter().collect(),
                    },
                },
            },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if presentations.get("alpha-presentation").await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("create presentation actuation should create the resource");

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_applies_delete_actuations_idempotently() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let presentations = backend.clone().using::<Presentation>("flotilla");
    let task_workspaces = backend.clone().using::<TaskWorkspace>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");
    presentations
        .create(&resource_meta().name("alpha-presentation").call(), &PresentationSpec {
            convoy_ref: "alpha".to_string(),
            presentation_policy_ref: "default".to_string(),
            name: "alpha".to_string(),
            process_selector: [("flotilla.work/convoy".to_string(), "alpha".to_string())].into_iter().collect(),
        })
        .await
        .expect("presentation create should succeed");
    task_workspaces
        .create(&resource_meta().name("alpha-task").call(), &TaskWorkspaceSpec {
            convoy_ref: "alpha".to_string(),
            task: "implement".to_string(),
            placement_policy_ref: "local".to_string(),
        })
        .await
        .expect("task workspace create should succeed");

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler { actuation: Actuation::DeletePresentation { name: "alpha-presentation".to_string() } },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(presentations.get("alpha-presentation").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete presentation actuation should remove the resource");

    harness.shutdown().await;

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler { actuation: Actuation::DeleteTaskWorkspace { name: "alpha-task".to_string() } },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(task_workspaces.get("alpha-task").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete task workspace actuation should remove the resource");

    task_workspaces.delete("alpha-task").await.expect_err("resource should already be gone");

    harness.shutdown().await;
}
