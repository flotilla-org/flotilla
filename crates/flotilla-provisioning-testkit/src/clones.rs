//! Clones on a real decided store, for the tests of their readers: each is requested through the
//! clone port and stepped as the Clone controller would step it.

use std::sync::Arc;

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
use flotilla_schema_sqlite::StoreHandle;
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};

use crate::Ending;

/// One home's clones on a decided store, served through the engine's readings.
pub struct CloneFixture {
    pub home: String,
    pub store: Arc<StoreHandle>,
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
        Self::over(home, Arc::new(StoreHandle::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<StoreHandle>) -> Self {
        let staged = StagedFacts::default();
        let log = StoreLog::new(Arc::clone(&store));
        let lifecycles: Arc<dyn Lifecycles<CloneKind>> =
            Arc::new(LifecycleHome::new(home, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("clones")));
        let only = home.to_owned();
        let clones = Arc::new(EngineClones::new(Arc::clone(&lifecycles), Arc::clone(&store), move || vec![only.clone()]));
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

    /// Writes the first launch of a clone named `name` (not named for its repository, as an
    /// imported clone may be), requested with `spec`, and returns the launch.
    pub fn put_named(&self, name: &str, spec: &CloneSpec) -> String {
        let request = flotilla_provisioning::engine::clones::request_of(name, spec, false, &self.home);
        let launch = self.lifecycles.request(name, request, Timestamp::from_datetime(&chrono::Utc::now())).expect("clone requested");
        launch.name().to_owned()
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
        crate::poison_store(Arc::clone(&self.store));
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
/// the controllers together: its environments, clones and checkouts are homed at `home`, and
/// [`Self::reads`] serves them all. Their changes are followed in the store's log, as the runtime
/// follows them. Its Checkout controller runs too when it is started with a checkout provider.
pub struct RunningClones {
    pub service: Arc<flotilla_schema_sqlite::stream::Service>,
    /// The environments the clones are made in, on the same store.
    pub environments: crate::EnvironmentFixture,
    pub clones: Arc<EngineClones>,
    /// The checkouts, and the engine Convoys they are held for, on the same store.
    pub checkouts: crate::CheckoutFixture,
    /// The terminal sessions, on the same store, their changes followed from its log.
    pub terminals: crate::TerminalsFixture,
    /// The engine's readings, serving [`Self::environments`], [`Self::clones`],
    /// [`Self::checkouts`] and [`Self::terminals`].
    pub reads: Arc<dyn LifecycleReads>,
    controllers: Vec<tokio::task::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl RunningClones {
    /// Starts the Clone controller over `runtime`. Must be called within a Tokio runtime.
    pub fn start<R>(home: &str, runtime: Arc<R>) -> Self
    where
        R: flotilla_provisioning::reconcilers::CloneRuntime + 'static,
    {
        Self::started(home, runtime)
    }

    /// Starts the Clone controller over `runtime` and the Checkout controller over `provider`,
    /// each checkout's inputs only its holder's name. Must be called within a Tokio runtime.
    pub fn start_with_checkouts<R, P>(home: &str, runtime: Arc<R>, provider: Arc<P>) -> Self
    where
        R: flotilla_provisioning::reconcilers::CloneRuntime + 'static,
        P: flotilla_provisioning::engine::checkout_actuator::CheckoutProvider,
    {
        use flotilla_provisioning::engine::{
            checkout_actuator::{CheckoutActuator, NamedHolderInputs},
            controller::Controller,
        };
        let mut running = Self::started(home, runtime);
        let actuator = Arc::new(CheckoutActuator::new(
            provider,
            Arc::new(NamedHolderInputs),
            Arc::clone(&running.checkouts.checkouts) as Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>,
            StoreReads::new(running.service.store()),
            home,
            running.checkouts.staged.clone(),
        ));
        let controller = Controller::start(
            Arc::clone(&running.checkouts.lifecycles),
            actuator,
            Arc::new(flotilla_resources::SystemClock),
            Arc::clone(&running.service),
        )
        .expect("checkout controller");
        let (backing, backings) = tokio::sync::mpsc::channel(1);
        let (rechecks, recheck_receiver) = tokio::sync::mpsc::channel::<String>(1);
        // The senders are kept by the controller's task: nothing is pushed to it.
        running.controllers.push(tokio::spawn(async move {
            let _senders = (backing, rechecks);
            controller.run_with(backings, recheck_receiver).await;
        }));
        running
    }

    fn started<R>(home: &str, runtime: Arc<R>) -> Self
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
        let clones = Arc::new(EngineClones::over(Arc::clone(&lifecycles), Arc::clone(&service)));
        let follower = clones.follow(Arc::clone(&service)).expect("follow the clones");
        let checkouts = crate::CheckoutFixture::over(home, service.store());
        let mut terminals = crate::TerminalsFixture::over(home, service.store());
        let terminal_follower = terminals.terminals.follow(Arc::clone(&service)).expect("follow the terminal sessions");
        let reads: Arc<dyn LifecycleReads> = Arc::new(
            LifecycleReadPort::new(StoreReads::new(service.store()), home.to_owned())
                .with_environments(Arc::clone(&environments.environments) as _)
                .with_clones(Arc::clone(&clones) as Arc<dyn CloneRecords>)
                .with_checkouts(Arc::clone(&checkouts.checkouts) as _)
                .with_terminals(Arc::clone(&terminals.terminals) as _),
        );
        terminals.reads = reads.clone();
        let actuator = Arc::new(CloneActuator::new(runtime, StoreReads::new(service.store()), home, staged, Arc::new(NoRemotes)));
        let controller =
            Controller::start(lifecycles, actuator, Arc::new(flotilla_resources::SystemClock), Arc::clone(&service)).expect("controller");
        let (backing, backings) = tokio::sync::mpsc::channel(1);
        // The sender is kept by the controller's task: a clone's backing is never pushed.
        let controller = tokio::spawn(async move {
            let _backing = backing;
            controller.run(backings).await;
        });
        Self {
            service,
            environments,
            clones,
            checkouts,
            terminals,
            reads,
            controllers: vec![controller, follower, terminal_follower],
            _directory: directory,
        }
    }
}

impl Drop for RunningClones {
    fn drop(&mut self) {
        for controller in &self.controllers {
            controller.abort();
        }
    }
}

/// A checkout provider whose every checkout is made, found present and inspected with nothing
/// judged, and whose removals succeed; it counts the checkouts it makes.
#[derive(Default)]
pub struct MadeCheckouts {
    pub made: std::sync::atomic::AtomicUsize,
}

impl MadeCheckouts {
    fn made(&self) -> flotilla_provisioning::checkout_runtime::PreparedCheckout {
        self.made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        flotilla_provisioning::checkout_runtime::PreparedCheckout {
            commit: Some("44982740".to_string()),
            branch_provenance: flotilla_resources::CheckoutBranchProvenance::CreatedForConvoy,
        }
    }
}

#[async_trait::async_trait]
impl flotilla_provisioning::engine::checkout_actuator::CheckoutProvider for MadeCheckouts {
    async fn path_exists(&self, _: &str, _: &str) -> Result<Option<bool>, String> {
        Ok(Some(true))
    }

    async fn create_worktree(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: &str,
    ) -> Result<flotilla_provisioning::checkout_runtime::PreparedCheckout, flotilla_vcs::vcs::CheckoutMaterialisationError> {
        Ok(self.made())
    }

    async fn continue_worktree(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<flotilla_provisioning::checkout_runtime::PreparedCheckout, flotilla_vcs::vcs::CheckoutMaterialisationError> {
        Ok(self.made())
    }

    async fn create_fresh_clone(
        &self,
        _: &str,
        _: flotilla_provisioning::engine::checkout_actuator::FreshClone<'_>,
    ) -> Result<flotilla_provisioning::checkout_runtime::PreparedCheckout, String> {
        Ok(self.made())
    }

    async fn create_local_clone(
        &self,
        _: &str,
        _: flotilla_vcs::vcs::LocalCloneRequest<'_>,
    ) -> Result<(flotilla_provisioning::checkout_runtime::PreparedCheckout, Option<String>), String> {
        Ok((self.made(), None))
    }

    async fn inspect(
        &self,
        _: &str,
        _: &str,
        _: &flotilla_resources::CheckoutSpec,
        _: flotilla_provisioning::engine::checkout_actuator::Against<'_>,
    ) -> Result<flotilla_resources::checkout_records::Inspection, String> {
        Ok(flotilla_resources::checkout_records::Inspection::default())
    }

    async fn remove(
        &self,
        _: &str,
        _: &flotilla_provisioning::checkout_runtime::CheckoutRemoval,
    ) -> Result<flotilla_provisioning::checkout_runtime::CheckoutRemovalOutcome, String> {
        Ok(flotilla_provisioning::checkout_runtime::CheckoutRemovalOutcome::Removed)
    }
}
