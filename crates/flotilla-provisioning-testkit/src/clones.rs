//! Clones on a real decided store, for the tests of their readers: each is requested through the
//! clone port and stepped as the Clone controller would step it.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_provisioning::{
    engine::{
        clones::EngineClones,
        launch_log::{LaunchLog, StagedFacts},
        reads::{LifecycleReadPort, StoreReads},
        LifecycleHome, Lifecycles, Step,
    },
    stored::clone::{CloneFacts, CloneKind},
};
use flotilla_resources::{
    clone_records::{CloneRecord, CloneRecords},
    lifecycle_reads::LifecycleReads,
    CloneSpec, RepositorySpec,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};

use crate::Ending;

/// One home's clones on a decided store, served through the engine's readings.
pub struct CloneFixture {
    pub home: String,
    pub store: Arc<Mutex<Store>>,
    pub lifecycles: Arc<dyn Lifecycles<CloneKind>>,
    pub clones: Arc<EngineClones>,
    /// The engine's readings, serving [`Self::clones`].
    pub reads: Arc<dyn LifecycleReads>,
    staged: StagedFacts<CloneFacts>,
}

impl Default for CloneFixture {
    fn default() -> Self {
        Self::at("home")
    }
}

impl CloneFixture {
    /// A fixture whose clones are homed at `home`.
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
        let lifecycles: Arc<dyn Lifecycles<CloneKind>> =
            Arc::new(LifecycleHome::new(home, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("clones")));
        let clones = Arc::new(EngineClones::new(Arc::clone(&lifecycles), Arc::clone(&store)));
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_clones(Arc::clone(&clones) as Arc<dyn CloneRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, clones, reads, staged }
    }

    /// Requests a clone of `repository` with `spec`, returning the launch that serves it.
    pub fn request(&self, repository: &RepositorySpec, spec: &CloneSpec, discovered: bool) -> String {
        self.clones.request(repository, spec, discovered).expect("clone requested")
    }

    /// Realises launch `launch` under its path, having found `default_branch`.
    pub fn ready(&self, launch: &str, default_branch: Option<&str>) {
        let path = self.lifecycles.get(launch).expect("held").spec().path.clone();
        self.staged.stage(launch, CloneFacts { default_branch: default_branch.map(str::to_owned) });
        self.step(launch, Step::Realised { identity: path });
    }

    /// Requests a clone of `repository` with `spec` and realises it, having found `default_branch`.
    pub fn put_ready(&self, repository: &RepositorySpec, spec: &CloneSpec, default_branch: Option<&str>) -> String {
        let launch = self.request(repository, spec, false);
        self.ready(&launch, default_branch);
        launch
    }

    /// Writes launch 1 of a clone named `name` (not named for its repository, as an imported
    /// clone may be), requested with `spec`, and returns the launch.
    pub fn put_named(&self, name: &str, spec: &CloneSpec) -> String {
        let launch = flotilla_resources::clone_records::launch(name, 1);
        let request = flotilla_provisioning::engine::clones::request_of(spec, false, &self.home);
        self.lifecycles.request(&launch, request, Timestamp::from_datetime(&chrono::Utc::now())).expect("clone requested");
        launch
    }

    /// [`Self::put_named`], realised having found `default_branch`.
    pub fn put_named_ready(&self, name: &str, spec: &CloneSpec, default_branch: Option<&str>) -> String {
        let launch = self.put_named(name, spec);
        self.ready(&launch, default_branch);
        launch
    }

    /// Launch `launch` as its readers see it.
    pub fn record(&self, launch: &str) -> CloneRecord {
        self.clones.get(launch, None).expect("read").expect("held")
    }

    /// Ends `launch` as `ending` says, realising it first where the ending needs a realised launch.
    /// A clone is realised under its path, whatever identity the ending names.
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
        self.lifecycles.step(launch, &step, Timestamp::from_datetime(&chrono::Utc::now())).expect("clone step");
    }
}

/// Records no remote: the tests that run it see no transport failure.
struct NoRemotes;

impl flotilla_provisioning::engine::clone_actuator::RemoteReachability for NoRemotes {
    fn unreachable(&self, _: &str, _: &str, _: &str) {}
}

/// A decided store at `home` whose Clone controller runs over a clone runtime, for tests that run
/// the controllers together: its environments and clones are homed at `home`, and
/// [`Self::reads`] serves both. Their changes are followed in the store's log, as the runtime
/// follows them.
pub struct RunningClones {
    pub service: Arc<flotilla_schema_sqlite::stream::Service>,
    /// The environments the clones are made in, on the same store.
    pub environments: crate::EnvironmentFixture,
    pub clones: Arc<EngineClones>,
    /// The engine's readings, serving [`Self::environments`] and [`Self::clones`].
    pub reads: Arc<dyn LifecycleReads>,
    controller: tokio::task::JoinHandle<()>,
    follower: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl RunningClones {
    /// Starts the Clone controller over `runtime`. Must be called within a Tokio runtime.
    pub fn start<R>(home: &str, runtime: Arc<R>) -> Self
    where
        R: flotilla_provisioning::reconcilers::CloneRuntime + 'static,
    {
        use flotilla_provisioning::engine::{clone_actuator::CloneActuator, controller::Controller};
        let directory = tempfile::tempdir().expect("directory");
        let service = Arc::new(
            flotilla_schema_sqlite::stream::Service::open(
                &directory.path().join("decided.sqlite"),
                home,
                flotilla_provisioning::stored::decided_registry().expect("registry"),
            )
            .expect("store"),
        );
        let log = StoreLog::new(service.store());
        let staged = StagedFacts::default();
        let lifecycles: Arc<dyn Lifecycles<CloneKind>> =
            Arc::new(LifecycleHome::new(home, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("clones")));
        let environments = crate::EnvironmentFixture::over(home, service.store());
        let clones = Arc::new(EngineClones::new(Arc::clone(&lifecycles), service.store()));
        let follower = clones.follow(Arc::clone(&service)).expect("follow the clones");
        let reads = Arc::new(
            LifecycleReadPort::new(StoreReads::new(service.store()), home.to_owned())
                .with_environments(Arc::clone(&environments.environments) as _)
                .with_clones(Arc::clone(&clones) as Arc<dyn CloneRecords>),
        );
        let actuator = Arc::new(CloneActuator::new(runtime, StoreReads::new(service.store()), home, staged, Arc::new(NoRemotes)));
        let controller =
            Controller::start(lifecycles, actuator, Arc::new(flotilla_resources::SystemClock), Arc::clone(&service)).expect("controller");
        let (backing, backings) = tokio::sync::mpsc::channel(1);
        // The sender is kept by the controller's task: a clone's backing is never pushed.
        let controller = tokio::spawn(async move {
            let _backing = backing;
            controller.run(backings).await;
        });
        Self { service, environments, clones, reads, controller, follower, _directory: directory }
    }
}

impl Drop for RunningClones {
    fn drop(&mut self) {
        self.controller.abort();
        self.follower.abort();
    }
}
