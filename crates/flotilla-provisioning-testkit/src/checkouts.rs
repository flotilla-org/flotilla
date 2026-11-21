//! Checkouts on a real decided store, for the tests of their readers: each is requested through
//! the checkout port and stepped as the Checkout controller would step it.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_protocol::PrincipalRef;
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
    AdmissionDocument, CheckoutSpec, WorkflowSnapshot,
};
use flotilla_schema::{Address, ChangeItem, Frozen, Identity, Kind, Timestamp};
use flotilla_schema_sqlite::StoreHandle;
use flotilla_state_machines::{
    machines::{
        convoy::{
            Abandon, AbandonCause, Convoy, ConvoyRequest, Deliver, Duty, DutyId, DutyScope, MemoryConvoyWorld, Priority, PromiseKinds,
            Resupervise, RetireDuty, Seat, SeatName, Terms,
        },
        lifecycle::{LifecycleState, VanishCause},
    },
    Cause, CauseCategory, Home, Outcome, Ref, Resource,
};

use crate::Ending;

/// One home's checkouts on a decided store, served through the engine's readings.
pub struct CheckoutFixture {
    pub home: String,
    pub store: Arc<StoreHandle>,
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

/// An admission document with no crew or vessels: what a fixture convoy froze, so the daemon's
/// convoy views decode it like any other launch.
fn empty_admission() -> Vec<u8> {
    AdmissionDocument {
        workflow_snapshot: WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: Vec::new(),
        },
        placement_decision: None,
        placement_policy: None,
        role: String::new(),
        ensure_admission: None,
        naming: None,
        dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "operator".into() },
        continuation: None,
        inputs: Default::default(),
        repositories: Vec::new(),
        r#ref: None,
        adopted_checkout_refs: Default::default(),
        subjects: Vec::new(),
        instruction: None,
        workflow_template: None,
        placement_snapshot: None,
        vessel_placements: Default::default(),
        brief_artifacts: false,
        adopted_checkout: None,
        provenance: Default::default(),
    }
    .encode()
    .expect("encode admission")
}

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
        Self::over(home, Arc::new(StoreHandle::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<StoreHandle>) -> Self {
        let staged = StagedFacts::default();
        let inputs = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<CheckoutKind>> = Arc::new(LifecycleHome::new(
            home,
            open_home_with(&log, LaunchLog::new(log.clone(), staged.clone()).with_inputs(inputs.clone())).expect("checkouts"),
        ));
        let only = home.to_owned();
        let checkouts =
            Arc::new(EngineCheckouts::new(Arc::clone(&lifecycles), Arc::clone(&store), inputs.clone(), move || vec![only.clone()]));
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

    /// Admits a launch of engine Convoy `name` here, open, its one obligation the lead's lifetime
    /// duty: what a checkout held for it is requested under. Returns the launch's name.
    pub fn open_convoy(&self, name: &str) -> String {
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
            role: String::new(),
            launch_of: name.into(),
            workflow_ref: "w".into(),
            admission: Frozen::new(flotilla_state_machines::machines::convoy::AdmissionBytes::new(empty_admission())).expect("freeze"),
            project: Address::project(PROJECT).expect("project"),
            promises: Default::default(),
            duties: [(DUTY, Duty { owner: lead(), scope: DutyScope::Lifetime, terms: Terms::default() })].into_iter().collect(),
            seats: vec![seat],
            priority: Priority(1),
            kinds: Frozen::new(PromiseKinds::default()).expect("freeze"),
        };
        let mut convoys = self.convoys.lock().expect("convoys");
        convoys.create(name, request, &convoy_world(), Self::now()).expect("convoy admitted");
        convoys.newest_launch(name, None).expect("convoy admitted").name().to_owned()
    }

    /// Admits a launch of engine Convoy `of` here unless one is held, and names it on old convoy
    /// `record` in `backend` as engine admission does: what that convoy's checkouts are held for.
    /// Returns the launch.
    pub async fn admit_for(&self, backend: &flotilla_store::ResourceBackend, namespace: &str, of: &str, record: &str) -> String {
        let launch = self.ensure_convoy(of);
        let convoys = backend.clone().using::<flotilla_resources::Convoy>(namespace);
        let current = convoys.get(record).await.expect("convoy");
        let status = flotilla_resources::ConvoyStatus { engine_launch: Some(launch.clone()), ..current.status.clone().unwrap_or_default() };
        convoys.update_status(record, &current.metadata.resource_version, &status).await.expect("launch named");
        launch
    }

    /// The newest launch of engine Convoy `name` here; `name` itself, which names no launch, when
    /// none is held.
    pub fn launch(&self, name: &str) -> String {
        self.convoys.lock().expect("convoys").newest_launch(name, None).map_or_else(|| name.to_owned(), |launch| launch.name().to_owned())
    }

    /// The holder of checkouts held for engine Convoy `name`'s newest launch here.
    pub fn holder(&self, name: &str) -> CheckoutHolder {
        CheckoutHolder::Convoy(self.launch(name))
    }

    /// Admits engine Convoy `name` here unless a launch is held already, as admission does once
    /// for a convoy whose vessels follow. Returns the launch's name.
    pub fn ensure_convoy(&self, name: &str) -> String {
        let held = self.convoys.lock().expect("convoys").newest_launch(name, None).map(|launch| launch.name().to_owned());
        held.unwrap_or_else(|| self.open_convoy(name))
    }

    /// Re-points the lead seat's supervisor on the newest launch of engine Convoy `name`: a write that ends nothing.
    pub fn resupervise_convoy(&self, name: &str) {
        let supervised_by = Address::project("elsewhere").expect("project");
        let resupervise = Resupervise { seat: lead(), supervised_by, by: lead(), authority: Ref::new(AUTHORITY) };
        let launch = self.launch(name);
        self.convoys.lock().expect("convoys").apply(&launch, resupervise, &convoy_world(), Self::now()).expect("convoy resupervised");
    }

    /// The newest launch of engine Convoy `name`: its version here, and its identity.
    pub fn convoy_version(&self, name: &str) -> (Identity, u64) {
        let name = &self.launch(name);
        let mut store = self.store.lock().expect("store");
        let mut read = store.read().expect("read");
        let code = <Resource<Convoy> as Kind>::CODE;
        let id = read.resolve(code, name, &self.home).expect("resolve").expect("convoy held");
        let loaded = read.load::<Resource<Convoy>>(id).expect("load").expect("convoy held");
        (Identity { kind: code, name: loaded.name, home: loaded.home }, loaded.version)
    }

    /// Ends the newest launch of engine Convoy `name` as delivered: its lead's duty retired, nothing holds it open.
    pub fn deliver_convoy(&self, name: &str) {
        let name = &self.launch(name);
        let mut convoys = self.convoys.lock().expect("convoys");
        let retire = RetireDuty { duty: DUTY, by: lead(), authority: Ref::new(AUTHORITY) };
        convoys.apply(name, retire, &convoy_world(), Self::now()).expect("duty retired");
        convoys.apply(name, Deliver { disposition: "merged".into() }, &convoy_world(), Self::now()).expect("convoy delivered");
    }

    /// Ends the newest launch of engine Convoy `name` as abandoned by the operator, for `reason`.
    pub fn abandon_convoy(&self, name: &str, reason: &str) {
        let abandon = Abandon { by: lead(), cause: AbandonCause::Operator, reason: reason.into(), authority: Ref::new(AUTHORITY) };
        let launch = self.launch(name);
        self.convoys.lock().expect("convoys").apply(&launch, abandon, &convoy_world(), Self::now()).expect("convoy abandoned");
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&flotilla_resources::clock::now())
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
        crate::poison_store(Arc::clone(&self.store));
    }

    fn step(&self, launch: &str, step: Step) {
        self.lifecycles.step(launch, &step, Timestamp::from_datetime(&flotilla_resources::clock::now())).expect("checkout step");
    }
}
