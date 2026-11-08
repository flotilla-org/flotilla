//! Real decided-store fixture shared by lifecycle reader contract tests.
pub mod agent_sessions;
pub mod checkouts;
pub mod clones;
pub mod environments;
pub mod image_build;
mod memory_store;
pub mod other_home;
pub use agent_sessions::AgentSessionsFixture;
pub use checkouts::CheckoutFixture;
pub use clones::CloneFixture;
pub use environments::EnvironmentFixture;
pub use memory_store::memory_store;
pub use other_home::{other_home_for, put_host_at_its_home, put_host_at_its_home_heard, replicate_home, replicate_home_at, OtherHome};
use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::engine::{
    reads::{LifecycleReadPort, StoreReads},
    Binding, LifecycleHome, Lifecycles, Step,
};
use flotilla_resources::{environment_records::EnvironmentRecords, lifecycle_reads::LifecycleReads, ResourceObject};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{Attempted, Lifecycle, LifecycleKind, MemoryLifecycleWorld},
    Cause, CauseCategory, Home, Log, Outcome,
};

/// The engine's environments, clones and checkouts homed at one home on one store, each fixture's
/// `reads` serving all three, as a daemon's lifecycle reads do.
pub struct EngineFixture {
    pub environments: EnvironmentFixture,
    pub clones: CloneFixture,
    pub checkouts: CheckoutFixture,
}

impl Default for EngineFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl EngineFixture {
    /// An engine whose records are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, CheckoutFixture::at(home).store)
    }

    /// An engine over `store`, whose home is `home`: a daemon's decided store, say.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let mut environments = EnvironmentFixture::over(home, Arc::clone(&store));
        let mut clones = CloneFixture::over(home, Arc::clone(&store));
        let checkouts =
            CheckoutFixture::over(home, store).serving(Arc::clone(&environments.environments) as _, Arc::clone(&clones.clones) as _);
        environments.reads = checkouts.reads.clone();
        clones.reads = checkouts.reads.clone();
        Self { environments, clones, checkouts }
    }

    /// The readings serving all three.
    pub fn reads(&self) -> Arc<dyn LifecycleReads> {
        self.checkouts.reads.clone()
    }
}

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
    /// The environments its readers find, homed at `home` on the same store.
    pub environments: EnvironmentFixture,
    /// The clones its readers find, homed at `home` on the same store.
    pub clones: CloneFixture,
    /// The checkouts its readers find, and the engine Convoys they are held for, homed at `home`
    /// on the same store.
    pub checkouts: CheckoutFixture,
}

impl<B: Binding> LifecycleFixture<B>
where
    <B::Kind as LifecycleKind>::Request: Send + Sync,
    StoreLog: Log<Lifecycle<B::Kind>>,
{
    pub fn new(replica: bool) -> Self {
        let store = Arc::new(Mutex::new(memory_store("home")));
        let home = Arc::new(LifecycleHome::new("home", Home::with_log("home", StoreLog::new(Arc::clone(&store))).expect("home")));
        let environments = EnvironmentFixture::over("home", Arc::clone(&store));
        let clones = CloneFixture::over("home", Arc::clone(&store));
        let checkouts = CheckoutFixture::over("home", Arc::clone(&store));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), if replica { "reader" } else { "home" }.into())
                .with_environments(Arc::clone(&environments.environments) as Arc<dyn EnvironmentRecords>)
                .with_clones(Arc::clone(&clones.clones) as Arc<dyn flotilla_resources::clone_records::CloneRecords>)
                .with_checkouts(Arc::clone(&checkouts.checkouts) as Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>),
        );
        Self { home, reads, store, environments, clones, checkouts }
    }

    pub fn name(&self, obj: &ResourceObject<B::Old>) -> String {
        flotilla_provisioning::engine::current::<B>(self.home.as_ref(), obj).0
    }

    pub fn request(&self, obj: &ResourceObject<B::Old>) {
        self.home.request(&self.name(obj), B::request(obj, "home").expect("request"), Timestamp(1_000_000)).expect("requested");
    }

    pub fn retry(&self, obj: &ResourceObject<B::Old>, cause: &str) {
        self.home
            .step(
                &self.name(obj),
                &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, cause), retryable: true }),
                Timestamp(2_000_000),
            )
            .expect("retry");
    }

    /// Persist only Attempted, modelling a restart before GiveUp; later assertions use the store read port.
    pub fn exhaust_before_give_up(&self, obj: &ResourceObject<B::Old>, cause: &str) {
        let mut home = Home::<Lifecycle<B::Kind>, _>::with_log("home", StoreLog::new(Arc::clone(&self.store))).expect("home");
        home.restore(self.home.get(&self.name(obj)).expect("requested resource")).expect("restore");
        home.apply(
            &self.name(obj),
            Attempted { outcome: Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false } },
            &MemoryLifecycleWorld::default(),
            Timestamp(2_000_000),
        )
        .expect("durable failed attempt");
    }

    pub fn finish(&self, obj: &ResourceObject<B::Old>, ending: Ending<'_>) {
        let name = self.name(obj);
        let name = name.as_str();
        let step = match ending {
            Ending::Ready(identity) => Step::Realised { identity: identity.into() },
            Ending::Lost(identity) => {
                if !self.home.get(name).is_some_and(|resource| {
                    matches!(resource.state(), flotilla_state_machines::machines::lifecycle::LifecycleState::Realised { .. })
                }) {
                    self.home.step(name, &Step::Realised { identity: identity.into() }, Timestamp(3_000_000)).expect("ready");
                }
                Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::lost() }
            }
            Ending::Released => Step::Released,
            Ending::Failed(reason) => {
                if !self.home.get(name).is_some_and(|resource| {
                    matches!(resource.state(), flotilla_state_machines::machines::lifecycle::LifecycleState::Realised { .. })
                }) {
                    self.home.step(name, &Step::Realised { identity: "backing".into() }, Timestamp(3_000_000)).expect("ready");
                }
                Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Failed { reason: reason.into() } }
            }
            Ending::GaveUp(cause) => {
                Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false })
            }
        };
        self.home.step(name, &step, Timestamp(4_000_000)).expect("ending");
    }

    /// Restore the controller's home from SQLite after an interrupted write sequence.
    pub fn reopen(&self) -> Arc<dyn Lifecycles<B::Kind>>
    where
        flotilla_state_machines::Resource<Lifecycle<B::Kind>>: flotilla_schema::Kind,
    {
        let home: Home<Lifecycle<B::Kind>, StoreLog> =
            flotilla_engine_store::open_home(StoreLog::new(Arc::clone(&self.store))).expect("restore home");
        Arc::new(LifecycleHome::new("home", home))
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

impl VesselFixture {
    /// Holds the lease of `vessel`, of `convoy`, pinned to `host`, as its placement does: the
    /// environment requested for it waits on this.
    pub fn lease(&self, vessel: &str, convoy: &str, host: &str) {
        use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
        use flotilla_resources::{Vessel, VesselSpec, VesselStatus};
        let placed = object::<Vessel>(
            vessel,
            VesselSpec {
                convoy_ref: convoy.into(),
                vessel_name: vessel.into(),
                fulfilment_kind_ref: String::new(),
                adopted_checkout_refs: Default::default(),
            },
            VesselStatus {
                placement_decision: Some(
                    PlacementDecision::builder()
                        .kind("pinned".into())
                        .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved(host), display_name: host.into() })
                        .build(),
                ),
                ..Default::default()
            },
        );
        self.request(&placed);
    }
}

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

/// Contradictory terminal observations; launch identity is selected separately from the engine.
pub fn terminal_session(phase: flotilla_resources::TerminalSessionPhase) -> ResourceObject<flotilla_resources::TerminalSession> {
    use flotilla_resources::{TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus};
    object(
        "terminal-a",
        TerminalSessionSpec {
            env_ref: "env".into(),
            role: "coder".into(),
            source: TerminalSessionSource::Tool { command: "bash".into() },
            cwd: "/work".into(),
            pool: "cleat".into(),
            env: Default::default(),
        },
        TerminalSessionStatus {
            phase,
            session_id: Some("old".into()),
            message: Some("stale reason".into()),
            stopped_at: Some(chrono::DateTime::UNIX_EPOCH),
            ..Default::default()
        },
    )
}

pub type TerminalFixture = LifecycleFixture<flotilla_provisioning::engine::terminal_session::TerminalSessionBinding>;

impl TerminalFixture {
    pub fn replace(&self, obj: &ResourceObject<flotilla_resources::TerminalSession>, predecessor: &str) -> Result<String, String> {
        use flotilla_resources::terminal_launches::TerminalLaunches;
        flotilla_provisioning::engine::terminal_session::EngineTerminalLaunches { lifecycles: self.home.clone() }.replace(
            obj,
            predecessor,
            chrono::DateTime::UNIX_EPOCH + chrono::Duration::seconds(10),
        )
    }
}

impl TerminalFixture {
    /// Inject a storage-boundary failure, forwarding every successful write to the actual SQLite log.
    pub fn replacement_write_failures(
        &self,
        obj: &ResourceObject<flotilla_resources::TerminalSession>,
        create: bool,
        count: usize,
    ) -> Arc<dyn Lifecycles<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>> {
        use flotilla_state_machines::machines::lifecycle::LifecycleEdgeKind;
        use flotilla_state_machines::{EdgeEventOf, InputRead, LogError, Resource, StoredState, Version};
        type Machine = Lifecycle<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>;
        struct FailedWrites {
            inner: StoreLog,
            create: bool,
            remaining: usize,
        }
        impl Log<Machine> for FailedWrites {
            fn create(
                &mut self,
                writer: &str,
                at: Timestamp,
                resource: &Resource<Machine>,
                reads: &[InputRead],
            ) -> Result<Version, LogError> {
                if self.create && self.remaining > 0 {
                    self.remaining -= 1;
                    return Err(LogError::new("injected successor create failure"));
                }
                self.inner.create(writer, at, resource, reads)
            }
            fn edge(
                &mut self,
                writer: &str,
                previous: &StoredState<Machine>,
                next: &Resource<Machine>,
                event: &EdgeEventOf<Machine>,
            ) -> Result<Version, LogError> {
                if !self.create && self.remaining > 0 && matches!(&event.edge, LifecycleEdgeKind::Vanish) {
                    self.remaining -= 1;
                    return Err(LogError::new("injected predecessor ending failure"));
                }
                self.inner.edge(writer, previous, next, event)
            }
        }
        let mut home =
            Home::<Machine, _>::with_log("home", FailedWrites { inner: StoreLog::new(self.store.clone()), create, remaining: count })
                .expect("home");
        home.restore(self.home.get(&self.name(obj)).expect("predecessor")).expect("restore");
        Arc::new(LifecycleHome::new("home", home))
    }
}
