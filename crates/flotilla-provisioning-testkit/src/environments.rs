//! Environments on a real decided store, for the tests of their readers: each is requested through
//! the environment port and stepped as the Environment controller would step it.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_provisioning::engine::{
    environments::{launch_facts, EngineEnvironments},
    launch_log::{LaunchLog, StagedFacts},
    reads::{LifecycleReadPort, StoreReads},
    LifecycleHome, Lifecycles, Step,
};
use flotilla_provisioning::stored::environment::EnvironmentKind;
use flotilla_resources::{
    environment_records::{EnvironmentRecords, LaunchFacts},
    lifecycle_reads::LifecycleReads,
    EnvironmentSpec,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};

use crate::Ending;

/// One home's environments on an in-memory decided store, served through the engine's readings.
pub struct EnvironmentFixture {
    pub home: String,
    pub store: Arc<Mutex<Store>>,
    pub lifecycles: Arc<dyn Lifecycles<EnvironmentKind>>,
    pub environments: Arc<EngineEnvironments>,
    /// The engine's readings, serving [`Self::environments`].
    pub reads: Arc<dyn LifecycleReads>,
    staged: StagedFacts,
}

impl Default for EnvironmentFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl EnvironmentFixture {
    /// A fixture whose environments are homed at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, Arc::new(Mutex::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let staged = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<EnvironmentKind>> =
            Arc::new(LifecycleHome::new(home, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("environments")));
        let only = home.to_owned();
        let environments = Arc::new(EngineEnvironments::new(Arc::clone(&lifecycles), Arc::clone(&store), move || vec![only.clone()]));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_environments(Arc::clone(&environments) as Arc<dyn EnvironmentRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, environments, reads, staged }
    }

    /// The same fixture whose [`Self::reads`] serve `clones` too, as a daemon's readings do.
    pub fn serving_clones(mut self, clones: Arc<dyn flotilla_resources::clone_records::CloneRecords>) -> Self {
        self.reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&self.store)), self.home.clone())
                .with_environments(Arc::clone(&self.environments) as Arc<dyn EnvironmentRecords>)
                .with_clones(clones),
        );
        self
    }

    /// The same fixture whose [`Self::reads`] serve `terminals` too, as a daemon's readings do.
    pub fn serving_terminals(mut self, terminals: Arc<dyn flotilla_resources::terminal_records::TerminalRecords>) -> Self {
        self.reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&self.store)), self.home.clone())
                .with_environments(Arc::clone(&self.environments) as Arc<dyn EnvironmentRecords>)
                .with_terminals(terminals),
        );
        self
    }

    /// Readings that serve this fixture's environments and `agent_sessions`.
    pub fn reads_with_agent_sessions(
        &self,
        agent_sessions: Arc<dyn flotilla_resources::agent_activity::AgentSessionReads>,
    ) -> Arc<dyn LifecycleReads> {
        Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&self.store)), self.home.clone())
                .with_environments(Arc::clone(&self.environments) as Arc<dyn EnvironmentRecords>)
                .with_agent_sessions(agent_sessions),
        )
    }

    /// Requests `name` with `spec`, held for `vessel`.
    pub fn request(&self, name: &str, spec: &EnvironmentSpec, vessel: Option<&str>) {
        self.environments.request(name, spec, vessel).expect("environment requested");
    }

    /// Realises `name` under `identity`, with `facts` as its launch's facts.
    pub fn ready(&self, name: &str, identity: &str, facts: LaunchFacts) {
        self.staged.stage(name, launch_facts(&facts).expect("facts"));
        self.step(name, Step::Realised { identity: identity.into() });
    }

    /// Requests `name` and realises it under `identity`, with `facts`.
    pub fn put_ready(&self, name: &str, spec: &EnvironmentSpec, vessel: Option<&str>, identity: &str, facts: LaunchFacts) {
        self.request(name, spec, vessel);
        self.ready(name, identity, facts);
    }

    /// Ends `name` as `ending` says, realising it first where the ending needs a realised launch.
    pub fn finish(&self, name: &str, ending: Ending<'_>) {
        let realised = self.lifecycles.get(name).is_some_and(|resource| matches!(resource.state(), LifecycleState::Realised { .. }));
        match ending {
            Ending::Ready(identity) => self.ready(name, identity, LaunchFacts::default()),
            Ending::Lost(identity) => {
                if !realised {
                    self.ready(name, identity, LaunchFacts::default());
                }
                self.step(name, Step::Vanished { cause: VanishCause::lost() });
            }
            Ending::Failed(reason) => {
                if !realised {
                    self.ready(name, "backing", LaunchFacts::default());
                }
                self.step(name, Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } });
            }
            Ending::Released => self.step(name, Step::Released),
            Ending::GaveUp(cause) => {
                self.step(name, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false }))
            }
        }
    }

    /// Ends `name` lost, as its home's probe reports a backing gone for `reason` (`cgroup_oom`).
    pub fn lose(&self, name: &str, reason: &str) {
        if !self.lifecycles.get(name).is_some_and(|resource| matches!(resource.state(), LifecycleState::Realised { .. })) {
            self.ready(name, "backing", LaunchFacts::default());
        }
        self.step(name, Step::Vanished { cause: VanishCause::Lost { reason: Some(reason.into()) } });
    }

    /// One retryable failed attempt of `name`.
    pub fn retry(&self, name: &str, cause: &str) {
        self.step(name, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, cause), retryable: true }));
    }

    /// Poisons the store's lock, so every read through it fails.
    pub fn poison(&self) {
        flotilla_store_testkit::poison_mutex(Arc::clone(&self.store));
    }

    fn step(&self, name: &str, step: Step) {
        self.lifecycles.step(name, &step, Timestamp::from_datetime(&chrono::Utc::now())).expect("environment step");
    }
}
