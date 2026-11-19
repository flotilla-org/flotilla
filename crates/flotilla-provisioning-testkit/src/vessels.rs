//! Vessels on a real decided store, for the tests of their readers: each launch is requested
//! through the vessel port and stepped as its home's vessel planner would step it.

use std::sync::Arc;

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::engine::{
    reads::{LifecycleReadPort, StoreReads},
    vessel::{EngineVessels, PlacementLifecycles},
    Lifecycles,
};
use flotilla_resources::{
    lifecycle_reads::LifecycleReads,
    vessel_records::{VesselLaunchRequest, VesselRecords, VesselStep},
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::{stream::Service, StoreHandle};
use flotilla_state_machines::{
    machines::{
        lifecycle::{Attempted, Lifecycle, MemoryLifecycleWorld},
        vessel::VesselKind,
    },
    Cause, CauseCategory, Home, Outcome,
};

/// One home's vessels on a decided store, served through the engine's readings.
pub struct VesselsFixture {
    pub home: String,
    pub store: Arc<StoreHandle>,
    pub lifecycles: Arc<dyn Lifecycles<VesselKind>>,
    pub vessels: Arc<EngineVessels>,
    /// The engine's readings, serving [`Self::vessels`].
    pub reads: Arc<dyn LifecycleReads>,
}

impl Default for VesselsFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl VesselsFixture {
    /// A fixture whose vessels are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, Arc::new(StoreHandle::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<StoreHandle>) -> Self {
        let owned = home.to_owned();
        Self::over_homes(home, store, move || vec![owned.clone()])
    }

    /// A fixture over `service`'s store and home, reading every home it holds.
    pub fn over_service(service: &Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        let homes = Arc::clone(service);
        Self::over_homes(service.home(), service.store(), move || homes.homes())
    }

    fn over_homes(home: &str, store: Arc<StoreHandle>, homes: impl Fn() -> Vec<String> + Send + Sync + 'static) -> Self {
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<VesselKind>> =
            Arc::new(PlacementLifecycles::new(home, Home::with_log(home, log).expect("vessel home")).expect("vessel placements"));
        let vessels = Arc::new(EngineVessels::new(Arc::clone(&lifecycles), Arc::clone(&store), homes));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_vessels(Arc::clone(&vessels) as Arc<dyn VesselRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, vessels, reads }
    }

    /// The launch that serves `work` of `convoy` (homed here) on `host`.
    pub fn request(&self, convoy: &str, work: &str, host: &str) -> String {
        self.request_for(&VesselLaunchRequest {
            convoy: convoy.to_owned(),
            convoy_home: self.home.clone(),
            work: work.to_owned(),
            host: host.to_owned(),
        })
    }

    /// The launch that serves `request`.
    pub fn request_for(&self, request: &VesselLaunchRequest) -> String {
        self.vessels.request(request).expect("vessel requested")
    }

    /// Realises launch `launch` in environment `environment`.
    pub fn ready(&self, launch: &str, environment: &str) {
        self.step(launch, VesselStep::Realised { environment: environment.to_owned() });
    }

    /// Writes `step` on launch `launch`.
    pub fn step(&self, launch: &str, step: VesselStep) {
        self.vessels.step(launch, &step).expect("vessel stepped");
    }

    /// Writes launch `launch`'s last try spent and no give-up, as a restart between the two leaves
    /// it: it reads Exhausted until its home writes the give-up.
    pub fn exhaust_before_give_up(&self, launch: &str, cause: &str) {
        let mut home = Home::<Lifecycle<VesselKind>, _>::with_log(&self.home, StoreLog::new(Arc::clone(&self.store))).expect("vessel home");
        home.restore(self.lifecycles.get(launch).expect("requested launch")).expect("restore");
        home.apply(
            launch,
            Attempted { outcome: Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false } },
            &MemoryLifecycleWorld::default(),
            Timestamp::from_datetime(&chrono::Utc::now()),
        )
        .expect("durable spent try");
    }

    /// Poisons the store's lock, so every read through it fails.
    pub fn poison(&self) {
        crate::poison_store(Arc::clone(&self.store));
    }
}

/// One home's vessels and checkouts on a decided store of their own, their changes followed from
/// its log, so a reader can wait on [`VesselRecords::changes`] and the checkouts' changes. The
/// vessels' readings serve both. Following stops when this is dropped.
pub struct FollowedVessels {
    pub fixture: VesselsFixture,
    pub checkouts: crate::CheckoutFixture,
    followers: [tokio::task::JoinHandle<()>; 2],
    _directory: tempfile::TempDir,
}

impl FollowedVessels {
    /// Vessels homed at `home`, followed.
    pub fn at(home: &str) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let registry = flotilla_provisioning::stored::decided_registry().expect("registry");
        let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), home, registry).expect("store"));
        let mut fixture = VesselsFixture::over_service(&service);
        let checkouts = crate::CheckoutFixture::over(home, service.store());
        fixture.reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(service.store()), home.to_owned())
                .with_vessels(Arc::clone(&fixture.vessels) as Arc<dyn VesselRecords>)
                .with_checkouts(Arc::clone(&checkouts.checkouts) as _),
        );
        let followers = [
            fixture.vessels.follow(Arc::clone(&service)).expect("follow the vessels"),
            checkouts.checkouts.follow(service).expect("follow the checkouts"),
        ];
        Self { fixture, checkouts, followers, _directory: directory }
    }
}

impl Drop for FollowedVessels {
    fn drop(&mut self) {
        self.followers.iter().for_each(tokio::task::JoinHandle::abort);
    }
}
