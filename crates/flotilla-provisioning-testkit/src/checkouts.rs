//! Checkouts on a real decided store, for the tests of their readers: each is requested through
//! the checkout port and stepped as the Checkout controller would step it.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_provisioning::{
    engine::{
        checkouts::EngineCheckouts,
        launch_log::{LaunchLog, StagedFacts},
        reads::{LifecycleReadPort, StoreReads},
        LifecycleHome, Lifecycles, Step,
    },
    stored::checkout::{CheckoutFacts, CheckoutKind},
};
use flotilla_resources::{
    checkout_records::{CheckoutHolder, CheckoutRecord, CheckoutRecords},
    lifecycle_reads::LifecycleReads,
    CheckoutSpec,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};

use crate::Ending;

/// One home's checkouts on a decided store, served through the engine's readings.
pub struct CheckoutFixture {
    pub home: String,
    pub store: Arc<Mutex<Store>>,
    pub lifecycles: Arc<dyn Lifecycles<CheckoutKind>>,
    pub checkouts: Arc<EngineCheckouts>,
    /// The engine's readings, serving [`Self::checkouts`].
    pub reads: Arc<dyn LifecycleReads>,
    staged: StagedFacts<CheckoutFacts>,
}

impl Default for CheckoutFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl CheckoutFixture {
    /// A fixture whose checkouts are homed at `home`.
    pub fn at(home: &str) -> Self {
        let registry = flotilla_provisioning::stored::production_kinds().expect("registry");
        let store =
            Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().expect("memory"), home, registry).expect("store")));
        Self::over(home, store)
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let staged = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<CheckoutKind>> =
            Arc::new(LifecycleHome::new(home, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("checkouts")));
        let checkouts = Arc::new(EngineCheckouts::new(Arc::clone(&lifecycles), Arc::clone(&store)));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_checkouts(Arc::clone(&checkouts) as Arc<dyn CheckoutRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, checkouts, reads, staged }
    }

    /// Requests checkout `name` with `spec`, held for `holder`, returning the launch that serves it.
    pub fn request(&self, name: &str, spec: &CheckoutSpec, holder: Option<&CheckoutHolder>) -> String {
        self.checkouts.request(name, spec, holder).expect("checkout requested")
    }

    /// Realises launch `launch` under its path, made short of what was asked if `degraded` says so.
    pub fn ready(&self, launch: &str, degraded: Option<&str>) {
        let path = self.lifecycles.get(launch).expect("held").spec().path.clone();
        self.staged.stage(launch, CheckoutFacts { degraded: degraded.map(str::to_owned) });
        self.step(launch, Step::Realised { identity: path });
    }

    /// Requests checkout `name` and realises it.
    pub fn put_ready(&self, name: &str, spec: &CheckoutSpec, holder: Option<&CheckoutHolder>) -> String {
        let launch = self.request(name, spec, holder);
        self.ready(&launch, None);
        launch
    }

    /// Launch `launch` as its readers see it.
    pub fn record(&self, launch: &str) -> CheckoutRecord {
        self.checkouts.get(launch, None).expect("read").expect("held")
    }

    /// Ends `launch` as `ending` says, realising it first where the ending needs a realised launch.
    /// A checkout is realised under its path, whatever identity the ending names.
    pub fn finish(&self, launch: &str, ending: Ending<'_>) {
        let realised = self.lifecycles.get(launch).is_some_and(|resource| matches!(resource.state(), LifecycleState::Realised { .. }));
        match ending {
            Ending::Ready(_) => self.ready(launch, None),
            Ending::Lost(_) => {
                if !realised {
                    self.ready(launch, None);
                }
                self.step(launch, Step::Vanished { cause: VanishCause::lost() });
            }
            Ending::Failed(reason) => {
                if !realised {
                    self.ready(launch, None);
                }
                self.step(launch, Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } });
            }
            Ending::Released => self.step(launch, Step::Released),
            Ending::GaveUp(cause) => {
                self.step(launch, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false }))
            }
        }
    }

    fn step(&self, launch: &str, step: Step) {
        self.lifecycles.step(launch, &step, Timestamp::from_datetime(&chrono::Utc::now())).expect("checkout step");
    }
}
