//! Terminal sessions on a real decided store, for the tests of their readers: each launch is
//! requested through the terminal port and stepped as the TerminalSession controller would step
//! it, its facts written with its Realise.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_provisioning::{
    engine::{
        launch_log::{LaunchLog, StagedFacts},
        reads::{LifecycleReadPort, StoreReads},
        terminals::{stored_facts, EngineTerminals},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{terminal_facts::TerminalLaunchFacts, terminal_session::TerminalSessionKind},
};
use flotilla_resources::{
    lifecycle_reads::LifecycleReads,
    terminal_records::{
        TerminalLaunchFacts as FactsView, TerminalLaunchRequest, TerminalLaunchSource, TerminalRecord, TerminalRecords, PROCESS_EXITED,
    },
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::machines::lifecycle::VanishCause;
use flotilla_state_machines::{Cause, CauseCategory, Outcome};

/// One home's terminal sessions on a decided store, served through the engine's readings.
pub struct TerminalsFixture {
    pub home: String,
    pub store: Arc<Mutex<Store>>,
    pub lifecycles: Arc<dyn Lifecycles<TerminalSessionKind>>,
    pub terminals: Arc<EngineTerminals>,
    /// The engine's readings, serving [`Self::terminals`].
    pub reads: Arc<dyn LifecycleReads>,
    /// The facts the launch log writes with each Realise.
    pub staged: StagedFacts<TerminalLaunchFacts>,
}

impl Default for TerminalsFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl TerminalsFixture {
    /// A fixture whose sessions are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, Arc::new(Mutex::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let owned = home.to_owned();
        Self::over_homes(home, store, move || vec![owned.clone()])
    }

    /// A fixture over `service`'s store and home, reading every home it holds: other homes'
    /// sessions arrive as their logs are replicated into it.
    pub fn over_service(service: &Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        let homes = Arc::clone(service);
        Self::over_homes(service.home(), service.store(), move || homes.homes())
    }

    fn over_homes(home: &str, store: Arc<Mutex<Store>>, homes: impl Fn() -> Vec<String> + Send + Sync + 'static) -> Self {
        let staged = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<TerminalSessionKind>> = Arc::new(LifecycleHome::new(
            home,
            open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("terminal sessions"),
        ));
        let terminals = Arc::new(EngineTerminals::new(Arc::clone(&lifecycles), Arc::clone(&store), homes));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_terminals(Arc::clone(&terminals) as Arc<dyn TerminalRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, terminals, reads, staged }
    }

    /// A crew's agent session in `environment`, held by `vessel` for `role`.
    pub fn agent(vessel: &str, role: &str, environment: &str) -> TerminalLaunchRequest {
        TerminalLaunchRequest {
            session: format!("terminal-{vessel}-{role}"),
            environment: environment.to_owned(),
            vessel: Some(vessel.to_owned()),
            role: role.to_owned(),
            pool: "cleat".into(),
            cwd: format!("/work/{vessel}"),
            env: BTreeMap::from([("FLOTILLA_ROLE".to_owned(), role.to_owned())]),
            source: TerminalLaunchSource::Agent {
                capability: "coding".into(),
                adapter: Some("claude".into()),
                model: None,
                brief: flotilla_resources::TerminalBrief {
                    path: "BRIEF.md".into(),
                    content: "do the work".into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
            },
        }
    }

    /// Requests `request`, returning the launch that serves it.
    pub fn request(&self, request: &TerminalLaunchRequest) -> String {
        self.terminals.request(request.clone()).expect("terminal session requested")
    }

    /// Realises launch `launch` as pool session `session_id`, with `facts`.
    pub fn ready(&self, launch: &str, session_id: &str, facts: &FactsView) {
        self.staged.stage(launch, stored_facts(facts));
        self.step(launch, Step::Realised { identity: session_id.to_owned() });
    }

    /// Realises launch `launch` as pool session `session_id`, with `facts`, at `at`: a launch that
    /// has been running since then.
    pub fn ready_at(&self, launch: &str, session_id: &str, facts: &FactsView, at: Timestamp) {
        self.staged.stage(launch, stored_facts(facts));
        self.step_at(launch, Step::Realised { identity: session_id.to_owned() }, at);
    }

    /// Ends realised launch `launch`: its process exited, and it waits for a resume.
    pub fn exited(&self, launch: &str, reason: &str) {
        self.step(launch, Step::Vanished { cause: VanishCause::Lost { reason: Some(format!("{PROCESS_EXITED}: {reason}")) } });
    }

    /// Ends realised launch `launch` lost: its pool no longer runs it, for `reason`.
    pub fn lost(&self, launch: &str, reason: &str) {
        self.step(launch, Step::Vanished { cause: VanishCause::Lost { reason: Some(reason.into()) } });
    }

    /// Ends realised launch `launch` failed, for `reason`.
    pub fn failed(&self, launch: &str, reason: &str) {
        self.step(launch, Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } });
    }

    /// Gives launch `launch` up: its pool refused it, for `cause`.
    pub fn give_up(&self, launch: &str, cause: &str) {
        self.step(launch, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false }));
    }

    /// Releases launch `launch`.
    pub fn release(&self, launch: &str) {
        self.step(launch, Step::Released);
    }

    /// Launch `launch` as its readers see it.
    pub fn record(&self, launch: &str) -> TerminalRecord {
        self.terminals.get(launch, None).expect("read").expect("held")
    }

    fn step(&self, launch: &str, step: Step) {
        self.step_at(launch, step, Timestamp::from_datetime(&chrono::Utc::now()));
    }

    fn step_at(&self, launch: &str, step: Step, at: Timestamp) {
        self.lifecycles.step(launch, &step, at).expect("terminal session step");
    }
}
