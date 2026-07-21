mod common;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use common::{resource_meta, TestLoopHarness};
use flotilla_resources::{
    controller::{Actuation, ControllerLoop, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler, ResolverLabelMappedWatch},
    ApiPaths, InMemoryBackend, InputMeta, LifecycleAuthority, NoStatusPatch, Presentation, PresentationSpec, Resource, ResourceBackend,
    ResourceError, ResourceObject, StatusPatch, TypedResolver, Vessel, VesselSpec,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, Notify},
    time::timeout,
};

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
    notify: Arc<Notify>,
}

impl RecordingReconciler {
    fn new(reconciled: Arc<Mutex<Vec<String>>>) -> Self {
        Self { reconciled, notify: Arc::new(Notify::new()) }
    }

    fn with_notify(reconciled: Arc<Mutex<Vec<String>>>, notify: Arc<Notify>) -> Self {
        Self { reconciled, notify }
    }
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
        self.notify.notify_one();
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
struct RacingFinalizerRemovalReconciler {
    finalized: Arc<Mutex<Vec<String>>>,
    primaries: TypedResolver<PrimaryResource>,
}

impl Reconciler for RacingFinalizerRemovalReconciler {
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
        let meta = InputMeta::from(&obj.metadata).without_finalizer("flotilla.work/test-finalizer");
        match self.primaries.update(&meta, &obj.metadata.resource_version, &obj.spec).await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/test-finalizer")
    }
}

fn delete_synchronously<T: Resource>(resolver: TypedResolver<T>, name: String) {
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime for synchronous delete");
        runtime.block_on(async move {
            resolver.delete(&name).await.expect("synchronous delete should succeed");
        });
    });
    assert!(handle.join().is_ok(), "synchronous delete thread should not panic");
}

#[derive(Clone)]
struct RacingAttachReconciler {
    primaries: TypedResolver<PrimaryResource>,
    target: String,
    raced: Arc<AtomicBool>,
}

impl Reconciler for RacingAttachReconciler {
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

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        if !self.raced.swap(true, Ordering::SeqCst) {
            delete_synchronously(self.primaries.clone(), self.target.clone());
        }
        Some("flotilla.work/test-finalizer")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusfulResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StatusfulSpec {
    value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StatusfulStatus {
    touched: bool,
}

enum TouchPatch {
    Touch,
}

impl StatusPatch<StatusfulStatus> for TouchPatch {
    fn apply(&self, status: &mut StatusfulStatus) {
        match self {
            Self::Touch => status.touched = true,
        }
    }
}

impl Resource for StatusfulResource {
    type Spec = StatusfulSpec;
    type Status = StatusfulStatus;
    type StatusPatch = TouchPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "test-statusful", kind: "TestStatusful" };
}

#[derive(Clone)]
struct RacingStatusPatchReconciler {
    primaries: TypedResolver<StatusfulResource>,
    target: String,
    reconciled: Arc<Mutex<Vec<String>>>,
}

impl Reconciler for RacingStatusPatchReconciler {
    type Resource = StatusfulResource;
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
        if obj.metadata.name == self.target {
            delete_synchronously(self.primaries.clone(), obj.metadata.name.clone());
        }
        ReconcileOutcome::new(Some(TouchPatch::Touch))
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
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
struct ExpiringSecondaryWatch {
    spawns: Arc<AtomicUsize>,
    expire: Arc<Notify>,
    spawned: Arc<Notify>,
}

impl flotilla_resources::controller::SecondaryWatch for ExpiringSecondaryWatch {
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
            let spawn = self.spawns.fetch_add(1, Ordering::SeqCst);
            self.spawned.notify_one();
            if spawn == 0 {
                self.expire.notified().await;
                return Err(ResourceError::WatchExpired { requested_version: "1".to_string(), compacted_through: Some("2".to_string()) });
            }
            std::future::pending().await
        })
    }
}

#[derive(Clone)]
struct FailingSecondaryWatch;

impl flotilla_resources::controller::SecondaryWatch for FailingSecondaryWatch {
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
        Box::pin(async { Err(ResourceError::other("secondary watch failed")) })
    }
}

#[derive(Clone)]
struct ActuatingReconciler {
    actuation: Actuation,
    reconciled: Option<Arc<Mutex<Vec<String>>>>,
}

impl Reconciler for ActuatingReconciler {
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
        if let Some(reconciled) = &self.reconciled {
            reconciled.lock().expect("reconciled lock").push(obj.metadata.name.clone());
        }
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

fn primary_meta_with_authority(name: &str, authority: LifecycleAuthority) -> InputMeta {
    primary_meta(name).with_lifecycle_authority(authority)
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
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
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
async fn controller_loop_watches_survive_resume_on_a_generational_backend() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
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
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
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
    .expect("primary watch should resume within the store generation");

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
            if reconciled.lock().expect("reconciled lock").iter().any(|name| name == "alpha") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("secondary watch should resume within the store generation");

    harness.shutdown().await;
}

#[tokio::test]
async fn resolver_label_mapped_watch_resumes_on_a_generational_observed_backend() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let observed_secondaries = observed.clone().using::<SecondaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(ResolverLabelMappedWatch::<SecondaryResource, PrimaryResource> {
                label_key: "flotilla.work/primary",
                resolver: observed_secondaries.clone(),
                _marker: std::marker::PhantomData,
            })],
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
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

    {
        let mut reconciled = reconciled.lock().expect("reconciled lock");
        reconciled.clear();
    }

    observed_secondaries
        .create(&secondary_meta("secondary-a", "alpha"), &SecondarySpec { value: "wake".to_string() })
        .await
        .expect("observed secondary create should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            if reconciled.lock().expect("reconciled lock").iter().any(|name| name == "alpha") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observed secondary watch should resume within the store generation");

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
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
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
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
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
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
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
async fn controller_loop_survives_notfound_when_removing_finalizer() {
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
            reconciler: RacingFinalizerRemovalReconciler { finalized: Arc::clone(&finalized), primaries: primaries.clone() },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let finalized_alpha = finalized.lock().expect("finalized lock").iter().any(|name| name == "alpha");
            if finalized_alpha && matches!(primaries.get("alpha").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("alpha should be finalized and removed by the racing delete");

    primaries.create(&primary_meta("beta"), &PrimarySpec { value: "two".to_string() }).await.expect("beta create should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let object = primaries.get("beta").await.expect("beta should still exist");
            if object.metadata.finalizers == vec!["flotilla.work/test-finalizer".to_string()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should continue after finalizer removal NotFound");

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_survives_notfound_when_attaching_finalizer() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: RacingAttachReconciler {
                primaries: primaries.clone(),
                target: "alpha".to_string(),
                raced: Arc::new(AtomicBool::new(false)),
            },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(1), || {
            let primaries = primaries.clone();
            async move { matches!(primaries.get("alpha").await, Err(ResourceError::NotFound { .. })) }
        })
        .await;

    primaries.create(&primary_meta("beta"), &PrimarySpec { value: "two".to_string() }).await.expect("second primary create should succeed");
    harness
        .wait_until(Duration::from_secs(1), || {
            let primaries = primaries.clone();
            async move {
                primaries
                    .get("beta")
                    .await
                    .is_ok_and(|object| object.metadata.finalizers == vec!["flotilla.work/test-finalizer".to_string()])
            }
        })
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_survives_notfound_when_patching_status() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<StatusfulResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &StatusfulSpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: RacingStatusPatchReconciler {
                primaries: primaries.clone(),
                target: "alpha".to_string(),
                reconciled: Arc::clone(&reconciled),
            },
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(1), || {
            let primaries = primaries.clone();
            let reconciled = Arc::clone(&reconciled);
            async move {
                let alpha_reconciled = reconciled.lock().expect("reconciled lock").iter().any(|name| name == "alpha");
                alpha_reconciled && matches!(primaries.get("alpha").await, Err(ResourceError::NotFound { .. }))
            }
        })
        .await;

    primaries
        .create(&primary_meta("beta"), &StatusfulSpec { value: "two".to_string() })
        .await
        .expect("second primary create should succeed");
    harness
        .wait_until(Duration::from_secs(1), || {
            let reconciled = Arc::clone(&reconciled);
            async move { reconciled.lock().expect("reconciled lock").iter().any(|name| name == "beta") }
        })
        .await;

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

#[tokio::test]
async fn controller_loop_skips_reconcile_for_observed_and_adopted_resources() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries
        .create(&primary_meta_with_authority("a-adopted", LifecycleAuthority::Adopted), &PrimarySpec { value: "one".to_string() })
        .await
        .expect("adopted primary create should succeed");
    primaries
        .create(&primary_meta_with_authority("b-observed", LifecycleAuthority::Observed), &PrimarySpec { value: "two".to_string() })
        .await
        .expect("observed primary create should succeed");
    primaries.create(&primary_meta("z-managed"), &PrimarySpec { value: "three".to_string() }).await.expect("managed create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: Vec::new(),
            reconciler: RecordingReconciler::new(Arc::clone(&reconciled)),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if reconciled.lock().expect("reconciled lock").iter().any(|name| name == "z-managed") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("managed primary should reconcile");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(reconciled.lock().expect("reconciled lock").as_slice(), &["z-managed".to_string()]);

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_does_not_add_finalizers_to_observed_or_adopted_resources() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries
        .create(&primary_meta_with_authority("a-adopted", LifecycleAuthority::Adopted), &PrimarySpec { value: "one".to_string() })
        .await
        .expect("adopted primary create should succeed");
    primaries
        .create(&primary_meta_with_authority("b-observed", LifecycleAuthority::Observed), &PrimarySpec { value: "two".to_string() })
        .await
        .expect("observed primary create should succeed");
    primaries.create(&primary_meta("z-managed"), &PrimarySpec { value: "three".to_string() }).await.expect("managed create should succeed");

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
            let object = primaries.get("z-managed").await.expect("managed primary get should succeed");
            if object.metadata.finalizers == vec!["flotilla.work/test-finalizer".to_string()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller should attach its finalizer to managed primary");

    assert!(primaries.get("a-adopted").await.expect("adopted primary get should succeed").metadata.finalizers.is_empty());
    assert!(primaries.get("b-observed").await.expect("observed primary get should succeed").metadata.finalizers.is_empty());

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_removes_existing_adopted_finalizer_without_running_teardown() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let meta = resource_meta()
        .name("a-adopted")
        .finalizers(vec!["flotilla.work/test-finalizer".to_string()])
        .deletion_timestamp(chrono::Utc::now())
        .call()
        .with_lifecycle_authority(LifecycleAuthority::Adopted);
    primaries.create(&meta, &PrimarySpec { value: "one".to_string() }).await.expect("adopted primary create should succeed");

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
            if matches!(primaries.get("a-adopted").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("adopted primary should be unblocked without teardown");

    assert!(finalized.lock().expect("finalized lock").is_empty());

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
            reconciler: RecordingReconciler::new(Arc::new(Mutex::new(Vec::new()))),
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

#[tokio::test(start_paused = true)]
async fn expired_secondary_watch_resyncs_primaries_without_restarting_controller_loop() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");

    let spawns = Arc::new(AtomicUsize::new(0));
    let expire = Arc::new(Notify::new());
    let spawned = Arc::new(Notify::new());
    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let reconciled_notify = Arc::new(Notify::new());
    let first_spawned = spawned.notified();
    let initially_reconciled = reconciled_notify.notified();
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(ExpiringSecondaryWatch {
                spawns: Arc::clone(&spawns),
                expire: Arc::clone(&expire),
                spawned: Arc::clone(&spawned),
            })],
            reconciler: RecordingReconciler::with_notify(Arc::clone(&reconciled), Arc::clone(&reconciled_notify)),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    timeout(Duration::from_secs(1), first_spawned).await.expect("secondary watch should start");
    timeout(Duration::from_secs(1), initially_reconciled).await.expect("initial primary list should reconcile alpha");
    assert_eq!(spawns.load(Ordering::SeqCst), 1, "watch should start immediately");
    reconciled.lock().expect("reconciled lock").clear();

    let resynced = reconciled_notify.notified();
    expire.notify_one();
    timeout(Duration::from_secs(1), resynced).await.expect("expiry should immediately resync all primaries");
    assert!(reconciled.lock().expect("reconciled lock").contains(&"alpha".to_string()), "expiry should immediately resync all primaries");

    let restarted = spawned.notified();
    tokio::time::advance(Duration::from_millis(100)).await;
    timeout(Duration::from_secs(1), restarted).await.expect("expired secondary watch should restart in place");
    assert_eq!(spawns.load(Ordering::SeqCst), 2, "expired secondary watch should restart in place");

    harness.shutdown().await;
}

#[tokio::test]
async fn non_expiry_secondary_watch_error_still_exits_controller_loop() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let result = timeout(
        Duration::from_secs(1),
        ControllerLoop {
            primary: primaries,
            secondaries: vec![Box::new(FailingSecondaryWatch)],
            reconciler: RecordingReconciler::new(Arc::new(Mutex::new(Vec::new()))),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    )
    .await
    .expect("controller loop should return the watch error")
    .expect_err("non-expiry watch error should reach supervision");

    assert_eq!(result, ResourceError::other("secondary watch failed"));
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
                reconciled: None,
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

    let created = presentations.get("alpha-presentation").await.expect("created presentation should be readable");
    assert_eq!(created.metadata.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Managed));

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_applies_delete_actuations_idempotently() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let presentations = backend.clone().using::<Presentation>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
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
    vessels
        .create(&resource_meta().name("alpha-task").call(), &VesselSpec {
            convoy_ref: "alpha".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "local".to_string(),
            adopted_checkout_refs: Default::default(),
        })
        .await
        .expect("task workspace create should succeed");

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler {
                actuation: Actuation::DeletePresentation { name: "alpha-presentation".to_string() },
                reconciled: None,
            },
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
            reconciler: ActuatingReconciler { actuation: Actuation::DeleteVessel { name: "alpha-task".to_string() }, reconciled: None },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(vessels.get("alpha-task").await, Err(ResourceError::NotFound { .. })) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete task workspace actuation should remove the resource");

    vessels.delete("alpha-task").await.expect_err("resource should already be gone");

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_loop_delete_actuations_preserve_observed_and_adopted_resources() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let primaries = backend.clone().using::<PrimaryResource>("flotilla");
    let presentations = backend.clone().using::<Presentation>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    primaries.create(&primary_meta("alpha"), &PrimarySpec { value: "one".to_string() }).await.expect("primary create should succeed");
    presentations
        .create(
            &resource_meta().name("adopted-presentation").call().with_lifecycle_authority(LifecycleAuthority::Adopted),
            &PresentationSpec {
                convoy_ref: "alpha".to_string(),
                presentation_policy_ref: "default".to_string(),
                name: "alpha".to_string(),
                process_selector: [("flotilla.work/convoy".to_string(), "alpha".to_string())].into_iter().collect(),
            },
        )
        .await
        .expect("presentation create should succeed");
    vessels
        .create(&resource_meta().name("observed-task").call().with_lifecycle_authority(LifecycleAuthority::Observed), &VesselSpec {
            convoy_ref: "alpha".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "local".to_string(),
            adopted_checkout_refs: Default::default(),
        })
        .await
        .expect("task workspace create should succeed");

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries.clone(),
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler {
                actuation: Actuation::DeletePresentation { name: "adopted-presentation".to_string() },
                reconciled: Some(Arc::clone(&reconciled)),
            },
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
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
    .expect("delete presentation actuation should run");
    let presentation = presentations.get("adopted-presentation").await.expect("adopted presentation should remain");
    assert_eq!(presentation.metadata.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Adopted));

    harness.shutdown().await;

    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: primaries,
            secondaries: Vec::new(),
            reconciler: ActuatingReconciler {
                actuation: Actuation::DeleteVessel { name: "observed-task".to_string() },
                reconciled: Some(Arc::clone(&reconciled)),
            },
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
    .expect("delete task workspace actuation should run");
    let vessel = vessels.get("observed-task").await.expect("observed task workspace should remain");
    assert_eq!(vessel.metadata.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Observed));

    harness.shutdown().await;
}
