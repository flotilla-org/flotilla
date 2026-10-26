//! Real decided-store fixture shared by lifecycle reader contract tests.
use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::engine::{
    reads::{LifecycleReadPort, StoreReads},
    Binding, LifecycleHome, Lifecycles, Step,
};
use flotilla_resources::{lifecycle_reads::LifecycleReads, ResourceObject};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{Attempted, Lifecycle, LifecycleKind, MemoryLifecycleWorld},
    Cause, CauseCategory, Home, Log, Outcome,
};

/// Endings shared by every resource reader; the identity belongs to that kind's realisation.
#[derive(Clone, Copy)]
pub enum Ending<'a> {
    Ready(&'a str),
    Lost(&'a str),
    Released,
    Failed(&'a str),
    GaveUp(&'a str),
}

/// Drives the real engine and SQLite adapter without publishing any old status copy.
pub struct LifecycleFixture<B: Binding>
where
    StoreLog: Log<Lifecycle<B::Kind>>,
{
    pub home: Arc<LifecycleHome<B::Kind, StoreLog>>,
    pub reads: Arc<dyn LifecycleReads>,
    pub store: Arc<Mutex<Store>>,
}

impl<B: Binding> LifecycleFixture<B>
where
    <B::Kind as LifecycleKind>::Request: Send + Sync,
    StoreLog: Log<Lifecycle<B::Kind>>,
{
    pub fn new(replica: bool) -> Self {
        let registry = flotilla_provisioning::stored::production_kinds().expect("registry");
        let store =
            Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry).expect("store")));
        let home = Arc::new(LifecycleHome::new("home", Home::with_log("home", StoreLog::new(Arc::clone(&store))).expect("home")));
        let reads = Arc::new(LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), if replica { "reader" } else { "home" }.into()));
        Self { home, reads, store }
    }

    pub fn request(&self, obj: &ResourceObject<B::Old>) {
        self.home.request(&obj.metadata.name, B::request(obj, "home").expect("request"), Timestamp(1_000_000)).expect("requested");
    }

    pub fn retry(&self, obj: &ResourceObject<B::Old>, cause: &str) {
        self.home
            .step(
                &obj.metadata.name,
                &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, cause), retryable: true }),
                Timestamp(2_000_000),
            )
            .expect("retry");
    }

    /// Persist only Attempted, modelling a restart before GiveUp; later assertions use the store read port.
    pub fn exhaust_before_give_up(&self, obj: &ResourceObject<B::Old>, cause: &str) {
        let mut home = Home::<Lifecycle<B::Kind>, _>::with_log("home", StoreLog::new(Arc::clone(&self.store))).expect("home");
        home.restore(self.home.get(&obj.metadata.name).expect("requested resource")).expect("restore");
        home.apply(
            &obj.metadata.name,
            Attempted { outcome: Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false } },
            &MemoryLifecycleWorld::default(),
            Timestamp(2_000_000),
        )
        .expect("durable failed attempt");
    }

    pub fn finish(&self, obj: &ResourceObject<B::Old>, ending: Ending<'_>) {
        let name = &obj.metadata.name;
        let step = match ending {
            Ending::Ready(identity) => Step::Realised { identity: identity.into() },
            Ending::Lost(identity) => {
                if !self.home.get(name).is_some_and(|resource| {
                    matches!(resource.state(), flotilla_state_machines::machines::lifecycle::LifecycleState::Realised { .. })
                }) {
                    self.home.step(name, &Step::Realised { identity: identity.into() }, Timestamp(3_000_000)).expect("ready");
                }
                Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Lost }
            }
            Ending::Released => Step::Released,
            Ending::Failed(reason) => {
                self.home.step(name, &Step::Realised { identity: "backing".into() }, Timestamp(3_000_000)).expect("ready");
                Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Failed { reason: reason.into() } }
            }
            Ending::GaveUp(cause) => {
                Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false })
            }
        };
        self.home.step(name, &step, Timestamp(4_000_000)).expect("ending");
        assert!(B::copy(obj, &self.home.get(name).expect("held")).is_none(), "a migrated binding emits no copy");
    }

    pub fn poison(&self) {
        flotilla_store_testkit::poison_mutex(Arc::clone(&self.store));
    }
}

/// A stale resource snapshot for pure reader seams; no provider or persistence is replaced.
pub fn object<T: flotilla_resources::Resource>(name: &str, spec: T::Spec, status: T::Status) -> ResourceObject<T> {
    ResourceObject {
        metadata: flotilla_resources::ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: chrono::DateTime::UNIX_EPOCH,
            merge: None,
        },
        spec,
        status: Some(status),
    }
}

/// Store a stale snapshot through the real backend's validation and versioning.
pub async fn store_object<T: flotilla_resources::Resource>(
    backend: &flotilla_store::ResourceBackend,
    object: ResourceObject<T>,
) -> ResourceObject<T> {
    let records = backend.using::<T>(&object.metadata.namespace);
    let created = records.create(&flotilla_resources::InputMeta::from(&object.metadata), &object.spec).await.expect("resource");
    match object.status {
        Some(status) => {
            records.update_status(&created.metadata.name, &created.metadata.resource_version, &status).await.expect("stale status")
        }
        None => created,
    }
}

/// The Vessel lease using the same real store and reader contract.
pub type VesselFixture = LifecycleFixture<flotilla_provisioning::engine::vessel::VesselBinding>;

/// Contradictory actuator evidence for image reader contracts; the engine supplies phase/digest/cause.
pub fn image_build(phase: flotilla_resources::ImageBuildPhase) -> ResourceObject<flotilla_resources::ImageBuild> {
    use flotilla_resources::{
        ImageBuildFailure, ImageBuildFailureClass, ImageBuildReservation, ImageBuildResourceSpec, ImageBuildSpec, ImageBuildStatus,
        ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier,
    };
    let layers = vec![ImageLayerSpec::builder()
        .name("base".into())
        .version("1".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from(format!("ubuntu@sha256:{}", "a".repeat(64)))
        .fragment("FROM ${BASE}\n".into())
        .probe("true".into())
        .provides(std::collections::BTreeSet::from(["tool:verified".into()]))
        .build()];
    let identity = flotilla_resources::image_identity(&layers);
    let name = flotilla_resources::image_build_name(&identity, "amd64", &ImageDelivery::Local, "builder");
    object(
        &name,
        ImageBuildResourceSpec::Current(Box::new(
            ImageBuildSpec::builder()
                .identity(identity)
                .architecture("amd64".into())
                .layers(layers)
                .host_ref("builder".into())
                .delivery(ImageDelivery::Local)
                .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
                .build(),
        )),
        ImageBuildStatus {
            phase,
            attempt: 99,
            digest: Some("stale digest".into()),
            failure: Some(ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason: "stale reason".into() }),
            ..Default::default()
        },
    )
}
