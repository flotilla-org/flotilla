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
use flotilla_schema::{Address, ChangeItem, Frozen, Identity, Kind, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::{
        convoy::{
            Abandon, AbandonCause, Convoy, ConvoyRequest, Deliver, Duty, DutyId, DutyScope, MemoryConvoyWorld, Priority, PromiseKinds,
            Readmit, RetireDuty, Seat, SeatName, Terms, VesselClass,
        },
        lifecycle::{LifecycleState, VanishCause},
    },
    Cause, CauseCategory, Home, Outcome, Ref, Resource,
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
    /// The facts the launch log writes with each Realise: a Checkout controller over these
    /// lifecycles stages into it.
    pub staged: StagedFacts<CheckoutFacts>,
    /// The items each launch is created under, staged for the launch log as the checkout port
    /// stages them: a test may stage its own before requesting through [`Self::lifecycles`].
    pub inputs: StagedFacts<Vec<ChangeItem>>,
    /// The engine Convoys checkouts are held for, at this home.
    convoys: Mutex<Home<Convoy, StoreLog>>,
}

/// The project every fixture convoy is admitted under; the authority that holds.
const PROJECT: &str = "p";
const AUTHORITY: &str = "yes";
const DUTY: DutyId = DutyId(1);

fn convoy_world() -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default().with_project(&Address::project(PROJECT).expect("project"));
    world.authority.put(&Ref::new(AUTHORITY), true);
    world
}

fn lead() -> SeatName {
    SeatName::new("lead", "")
}

impl Default for CheckoutFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl CheckoutFixture {
    /// A fixture whose checkouts are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, Arc::new(Mutex::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let staged = StagedFacts::default();
        let inputs = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<CheckoutKind>> = Arc::new(LifecycleHome::new(
            home,
            open_home_with(&log, LaunchLog::new(log.clone(), staged.clone()).with_inputs(inputs.clone())).expect("checkouts"),
        ));
        let checkouts = Arc::new(EngineCheckouts::new(Arc::clone(&lifecycles), Arc::clone(&store), inputs.clone()));
        let convoys = Mutex::new(open_home_with::<Convoy, _>(&log, log.clone()).expect("convoys"));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_checkouts(Arc::clone(&checkouts) as Arc<dyn CheckoutRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, checkouts, reads, staged, inputs, convoys }
    }

    /// The same fixture whose [`Self::reads`] serve `environments` and `clones` too, as a daemon's
    /// readings do.
    pub fn serving(
        mut self,
        environments: Arc<dyn flotilla_resources::environment_records::EnvironmentRecords>,
        clones: Arc<dyn flotilla_resources::clone_records::CloneRecords>,
    ) -> Self {
        self.reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&self.store)), self.home.clone())
                .with_environments(environments)
                .with_clones(clones)
                .with_checkouts(Arc::clone(&self.checkouts) as Arc<dyn CheckoutRecords>),
        );
        self
    }

    /// Readings that serve this fixture's checkouts and `agent_sessions`.
    pub fn reads_with_agent_sessions(
        &self,
        agent_sessions: Arc<dyn flotilla_resources::agent_activity::AgentSessionReads>,
    ) -> Arc<dyn LifecycleReads> {
        Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&self.store)), self.home.clone())
                .with_checkouts(Arc::clone(&self.checkouts) as Arc<dyn CheckoutRecords>)
                .with_agent_sessions(agent_sessions),
        )
    }

    /// Admits engine Convoy `name` here, open, its one obligation the lead's lifetime duty: what a
    /// checkout held for it is requested under.
    pub fn open_convoy(&self, name: &str) {
        let seat = Seat {
            role: "lead".into(),
            seat: String::new(),
            parent: None,
            supervised_by: Address::project(PROJECT).expect("project"),
            failed: None,
            retired: false,
            replacements: 0,
        };
        let request = ConvoyRequest {
            workflow_ref: "w".into(),
            admission: Frozen::new(flotilla_state_machines::machines::convoy::AdmissionBytes::new(Vec::new())).expect("freeze"),
            project: Address::project(PROJECT).expect("project"),
            promises: Default::default(),
            duties: [(DUTY, Duty { owner: lead(), scope: DutyScope::Lifetime, terms: Terms::default() })].into_iter().collect(),
            seats: vec![seat],
            quota: [(VesselClass::new("standard"), 1)].into_iter().collect(),
            priority: Priority(1),
            kinds: Frozen::new(PromiseKinds::default()).expect("freeze"),
        };
        self.convoys.lock().expect("convoys").create(name, request, &convoy_world(), Self::now()).expect("convoy admitted");
    }

    /// Admits engine Convoy `name` here unless it is held already, as admission does once for a
    /// convoy whose vessels follow.
    pub fn ensure_convoy(&self, name: &str) {
        if self.convoys.lock().expect("convoys").get(name).is_none() {
            self.open_convoy(name);
        }
    }

    /// Readmits engine Convoy `name` with one more standard vessel: a write that ends nothing.
    pub fn readmit_convoy(&self, name: &str) {
        let readmit =
            Readmit { quota: [(VesselClass::new("standard"), 2)].into_iter().collect(), by: lead(), authority: Ref::new(AUTHORITY) };
        self.convoys.lock().expect("convoys").apply(name, readmit, &convoy_world(), Self::now()).expect("convoy readmitted");
    }

    /// The engine Convoy `name`'s version here, and its identity.
    pub fn convoy_version(&self, name: &str) -> (Identity, u64) {
        let mut store = self.store.lock().expect("store");
        let mut read = store.read().expect("read");
        let code = <Resource<Convoy> as Kind>::CODE;
        let id = read.resolve(code, name, &self.home).expect("resolve").expect("convoy held");
        let loaded = read.load::<Resource<Convoy>>(id).expect("load").expect("convoy held");
        (Identity { kind: code, name: loaded.name, home: loaded.home }, loaded.version)
    }

    /// Ends engine Convoy `name` as delivered: its lead's duty retired, nothing holds it open.
    pub fn deliver_convoy(&self, name: &str) {
        let mut convoys = self.convoys.lock().expect("convoys");
        let retire = RetireDuty { duty: DUTY, by: lead(), authority: Ref::new(AUTHORITY) };
        convoys.apply(name, retire, &convoy_world(), Self::now()).expect("duty retired");
        convoys.apply(name, Deliver { disposition: "merged".into() }, &convoy_world(), Self::now()).expect("convoy delivered");
    }

    /// Ends engine Convoy `name` as abandoned by the operator, for `reason`.
    pub fn abandon_convoy(&self, name: &str, reason: &str) {
        let abandon = Abandon { by: lead(), cause: AbandonCause::Operator, reason: reason.into(), authority: Ref::new(AUTHORITY) };
        self.convoys.lock().expect("convoys").apply(name, abandon, &convoy_world(), Self::now()).expect("convoy abandoned");
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&chrono::Utc::now())
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

    /// One retryable failed try of `launch`.
    pub fn retry(&self, launch: &str, cause: &str) {
        self.step(launch, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, cause), retryable: true }));
    }

    /// Poisons the store's lock, so every read through it fails.
    pub fn poison(&self) {
        flotilla_store_testkit::poison_mutex(Arc::clone(&self.store));
    }

    fn step(&self, launch: &str, step: Step) {
        self.lifecycles.step(launch, &step, Timestamp::from_datetime(&chrono::Utc::now())).expect("checkout step");
    }
}
