//! The TerminalSession controller's provider work (#3108): the shared [`Controller`] realises a
//! terminal launch by starting it in its environment's terminal pool, and releases it by killing
//! it once its holder has ended. This home's [`TerminalProbe`] looks at its running launches at an
//! interval, the host edge that makes their facts.
//!
//! - **Realise** starts the launch's process, adopting nothing: a launch is one run, and its
//!   pool session is replaced if a stale one holds the name. It stages what the launch was given
//!   (its cleat endpoint, its harness, its build parallelism) as the launch's
//!   [`TerminalLaunchFacts`] for the Realise edge, and records the command it ran as a local
//!   observation. A launch that cannot start gives up at once; its holder renews it. A brief whose
//!   artifact has not reached this host's blob store is a try that fails; the probe asks for the
//!   next try once the artifact has arrived.
//! - **Holder**: a crew's session is held for its vessel's lease, homed here; a tool's for its
//!   environment. Either ending releases the launch. A launch is usable only while its environment
//!   is realised.
//! - **Release** kills the process, unless the environment has ended and taken it with it. A turn
//!   already typed into it settles first, so its receipt is recorded. The launch's exit receipt and
//!   its harness's files go with it.
//! - **Backing**: an agent whose exit receipt is written has exited; a session the pool no longer
//!   runs is gone, as the pool says why; a screen that shows the harness refused its credential
//!   failed it. Each ends the launch, which is never revived: its holder asks for the next.
//!
//! [`Controller`]: super::controller::Controller

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_resources::{
    agent_activity::ProgressFacts,
    terminal_records::{TerminalLaunchFacts as FactsView, TerminalRecord, TerminalRecords, PROCESS_EXITED},
    ScreenObstruction, TerminalAttention, TerminalAttentionState, TerminalOccupancy,
};
use flotilla_schema::{Identity, Kind, KindCode};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::{
    machines::{
        lifecycle::{Lifecycle, LifecycleState},
        vessel::VesselKind,
    },
    Cause, CauseCategory, Resource,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{
    controller::{Actuator, Backing, BackingEvent, Holding, ProviderFailure, Usability},
    environment_actuator::state_of,
    launch_log::StagedFacts,
    reads::StoreReads,
    terminals::{holder_vessel, stored_facts},
};
use crate::stored::{
    environment::EnvironmentKind,
    terminal_facts::TerminalLaunchFacts,
    terminal_session::{TerminalSessionKind, TerminalSessionRequest},
};

/// Whether a launch's pool session runs, as its pool reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLiveness {
    Running,
    /// Its process exited.
    Stopped,
    /// The pool could not be asked: no evidence the session is gone.
    Unavailable(String),
    /// Positive evidence the session is gone for good, and why.
    Lost(String),
}

/// What starting a launch gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLaunched {
    /// The pool session it runs as: the launch's identity.
    pub session_id: String,
    pub facts: FactsView,
    /// The command it ran, which a passthrough attach runs again.
    pub launch_command: String,
}

/// One read of a running launch's screen and controller seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenReading {
    /// A digest of its output: a new one is crew progress.
    pub output_digest: Option<String>,
    pub attention: Option<TerminalAttention>,
    /// What the same read says blocks an idle-looking agent (#3460).
    pub obstruction: Option<ScreenObstruction>,
    pub occupancy: TerminalOccupancy,
}

/// This host's terminal pools and agent harnesses, as the TerminalSession controller uses them.
#[async_trait]
pub trait TerminalProvider: Send + Sync {
    /// Whether the brief `launch` starts with is on this host.
    async fn brief_ready(&self, launch: &TerminalRecord) -> Result<bool, String>;
    /// Starts `launch`'s process: its harness prepared, its credentials delivered, its brief
    /// written.
    async fn launch(&self, launch: &TerminalRecord) -> Result<TerminalLaunched, String>;
    async fn liveness(&self, launch: &TerminalRecord, session_id: &str) -> Result<TerminalLiveness, String>;
    /// The exit code an agent launch's process left in its receipt, once it exited.
    async fn exit_code(&self, launch: &TerminalRecord) -> Result<Option<i32>, String>;
    /// Why an agent that exited with `code` failed, if its harness says it did.
    async fn exit_failure(&self, launch: &TerminalRecord, session_id: &str, code: i32) -> Result<Option<String>, String>;
    /// Why an agent's settled screen says it cannot work (a refused credential), if it does.
    async fn screen_failure(&self, launch: &TerminalRecord, session_id: &str) -> Result<Option<String>, String>;
    /// Why `launch`'s crew cannot reach this daemon, if it cannot.
    async fn daemon_reach(&self, launch: &TerminalRecord) -> Result<Option<String>, String>;
    /// Reads `launch`'s screen; none when it cannot be read now.
    async fn read_screen(&self, launch: &TerminalRecord, session_id: &str) -> Result<Option<ScreenReading>, String>;
    /// Waits, within the provider's bound, for a turn already typed into `session_id` to settle.
    async fn settle_turn(&self, session_id: &str);
    async fn kill(&self, launch: &TerminalRecord, session_id: &str) -> Result<(), String>;
    /// Removes `launch`'s exit receipt, named by its launch.
    async fn remove_exit_receipt(&self, launch: &TerminalRecord) -> Result<(), String>;
    /// Removes the files `launch`'s harness was prepared with.
    async fn cleanup_artifacts(&self, launch: &TerminalRecord) -> Result<(), String>;
}

const ENVIRONMENT: KindCode = <Resource<Lifecycle<EnvironmentKind>> as Kind>::CODE;
const VESSEL: KindCode = <Resource<Lifecycle<VesselKind>> as Kind>::CODE;

fn transient(message: impl Into<String>) -> Cause {
    Cause::new(CauseCategory::Transient, message)
}

/// What became of a realised launch, or `None` while it runs.
async fn ended<P: TerminalProvider + ?Sized>(provider: &P, record: &TerminalRecord, session_id: &str) -> Result<Option<Backing>, Cause> {
    // An agent's exit is read before its session's liveness: its session can outlive it.
    if record.is_agent() {
        if let Some(code) = provider.exit_code(record).await.map_err(transient)? {
            return Ok(Some(match provider.exit_failure(record, session_id, code).await.map_err(transient)? {
                Some(reason) => Backing::Failed { reason },
                None => Backing::Absent {
                    reason: Some(format!("{PROCESS_EXITED}: agent exited with code {code}; resume relaunches it in the existing checkout")),
                },
            }));
        }
    }
    match provider.liveness(record, session_id).await.map_err(transient)? {
        TerminalLiveness::Running => {}
        TerminalLiveness::Stopped => return Ok(Some(Backing::Absent { reason: Some(PROCESS_EXITED.into()) })),
        TerminalLiveness::Lost(reason) => return Ok(Some(Backing::Absent { reason: Some(reason) })),
        TerminalLiveness::Unavailable(message) => return Err(transient(message)),
    }
    if record.is_agent() {
        if let Some(reason) = provider.screen_failure(record, session_id).await.map_err(transient)? {
            return Ok(Some(Backing::Failed { reason }));
        }
    }
    Ok(None)
}

/// The launches whose last try found their brief's artifact missing, in memory: the probe looks
/// for each artifact and asks for the next try once it has arrived.
#[derive(Clone, Default)]
pub struct AwaitedBriefs(Arc<Mutex<BTreeSet<String>>>);

impl AwaitedBriefs {
    fn await_brief(&self, launch: &str) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(launch.to_owned());
    }

    fn arrived(&self, launch: &str) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(launch);
    }

    fn awaited(&self) -> BTreeSet<String> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// The TerminalSession kind's [`Actuator`] over this host's terminal provider.
pub struct TerminalActuator<P: ?Sized> {
    provider: Arc<P>,
    records: Arc<dyn TerminalRecords>,
    reads: StoreReads,
    home: String,
    staged: StagedFacts<TerminalLaunchFacts>,
    briefs: AwaitedBriefs,
}

impl<P: ?Sized> TerminalActuator<P> {
    pub fn new(
        provider: Arc<P>,
        records: Arc<dyn TerminalRecords>,
        reads: StoreReads,
        home: impl Into<String>,
        staged: StagedFacts<TerminalLaunchFacts>,
    ) -> Self {
        Self { provider, records, reads, home: home.into(), staged, briefs: AwaitedBriefs::default() }
    }

    /// The launches waiting on their brief's artifact, which the probe looks for.
    pub fn awaited_briefs(&self) -> AwaitedBriefs {
        self.briefs.clone()
    }

    fn record(&self, name: &str) -> Result<TerminalRecord, Cause> {
        self.records
            .get(name, Some(&self.home))
            .map_err(|error| transient(format!("read terminal launch {name}: {error}")))?
            .ok_or_else(|| transient(format!("terminal launch {name} is not held here")))
    }

    fn environment_ended(&self, request: &TerminalSessionRequest) -> Result<bool, Cause> {
        let environment = &request.environment;
        let state = self
            .reads
            .read(|read| Ok(state_of::<EnvironmentKind>(read, &environment.name, &environment.home)?))
            .map_err(|error| transient(format!("read environment {}: {error}", environment.name)))?;
        Ok(state.is_none_or(|state| !state.live()))
    }

    /// Whether the vessel `request` is held for, if any, has ended.
    fn vessel_ended(&self, request: &TerminalSessionRequest) -> Result<bool, Cause> {
        let Some(vessel) = request.holder.as_ref().and_then(holder_vessel) else { return Ok(false) };
        let state = self
            .reads
            .read(|read| Ok(state_of::<VesselKind>(read, vessel, &self.home)?))
            .map_err(|error| transient(format!("read vessel {vessel}: {error}")))?;
        Ok(state.is_none_or(|state| !state.live()))
    }
}

#[async_trait]
impl<P> Actuator<TerminalSessionKind> for TerminalActuator<P>
where
    P: TerminalProvider + ?Sized + 'static,
{
    async fn observe(&self, name: &str, _: &TerminalSessionRequest) -> Result<Backing, Cause> {
        let record = self.record(name)?;
        // A launch never started is started now; a realised one is looked at.
        let Some(session_id) = record.session_id().map(str::to_owned) else { return Ok(Backing::Absent { reason: None }) };
        Ok(ended(&*self.provider, &record, &session_id).await?.unwrap_or(Backing::Present { identity: session_id }))
    }

    async fn realise(&self, name: &str, _: &TerminalSessionRequest) -> Result<String, ProviderFailure> {
        let record = self.record(name).map_err(|cause| ProviderFailure::new(cause, true))?;
        match self.provider.brief_ready(&record).await {
            Ok(true) => self.briefs.arrived(name),
            Ok(false) => {
                self.briefs.await_brief(name);
                return Err(ProviderFailure::new(transient("its brief artifact is not in this host's blob store yet"), true));
            }
            Err(message) => return Err(ProviderFailure::new(transient(message), true)),
        }
        match self.provider.launch(&record).await {
            Ok(launched) => {
                self.staged.stage(name, stored_facts(&launched.facts));
                self.records.observations().update(name, |observation| observation.launch_command = Some(launched.launch_command));
                Ok(launched.session_id)
            }
            // The launch gives up: its holder renews it once what failed is fixed.
            Err(message) => Err(ProviderFailure::new(Cause::new(CauseCategory::External, message), false)),
        }
    }

    async fn release(&self, name: &str, request: &TerminalSessionRequest, identity: Option<&str>) -> Result<(), ProviderFailure> {
        let failed = |message: String| ProviderFailure::new(transient(message), true);
        self.records.observations().forget(name);
        self.briefs.arrived(name);
        // An ended environment's teardown removed its sessions, pool and harness with it.
        if self.environment_ended(request).map_err(|cause| ProviderFailure::new(cause, true))? {
            return Ok(());
        }
        let record = self.record(name).map_err(|cause| ProviderFailure::new(cause, true))?;
        let mut errors = Vec::new();
        if let Some(session_id) = identity {
            self.provider.settle_turn(session_id).await;
            if let Err(error) = self.provider.kill(&record, session_id).await {
                errors.push(error);
            }
        }
        // A live process could still write its receipt: it is removed once the kill succeeded.
        if errors.is_empty() && record.is_agent() {
            if let Err(error) = self.provider.remove_exit_receipt(&record).await {
                errors.push(error);
            }
        }
        if let Err(error) = self.provider.cleanup_artifacts(&record).await {
            errors.push(error);
        }
        if errors.is_empty() {
            return Ok(());
        }
        // A crew's session whose vessel ended goes with its environment, whose release waits
        // for it: one that cannot be reached here, as when its environment was not adopted after
        // a restart, is removed by that teardown.
        if self.vessel_ended(request).map_err(|cause| ProviderFailure::new(cause, true))? {
            warn!(session = name, errors = %errors.join("; "), "a session of an ended vessel is left to its environment's teardown");
            return Ok(());
        }
        Err(failed(errors.join("; ")))
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![ENVIRONMENT, VESSEL]
    }

    fn dependencies(&self, request: &TerminalSessionRequest) -> Vec<Identity> {
        let mut depends = vec![request.environment.identity()];
        if let Some(vessel) = request.holder.as_ref().and_then(holder_vessel) {
            depends.push(Identity { kind: VESSEL, name: vessel.to_owned(), home: self.home.clone() });
        }
        depends
    }

    fn holder(&self, read: &mut Read<'_>, _: &str, request: &TerminalSessionRequest) -> Result<Holding, StoreError> {
        let environment = &request.environment;
        if state_of::<EnvironmentKind>(read, &environment.name, &environment.home)?.is_none_or(|state| !state.live()) {
            return Ok(Holding { ended: true, record: Some(environment.identity()) });
        }
        // A crew's vessel lease is homed here, and its sessions are requested only once the lease
        // exists (every vessel reconciled here is leased, placed or not): an absent lease has ended.
        let Some(vessel) = request.holder.as_ref().and_then(holder_vessel) else {
            return Ok(Holding { ended: false, record: Some(environment.identity()) });
        };
        let ended = state_of::<VesselKind>(read, vessel, &self.home)?.is_none_or(|state| !state.live());
        Ok(Holding { ended, record: Some(Identity { kind: VESSEL, name: vessel.to_owned(), home: self.home.clone() }) })
    }

    fn usability(&self, read: &mut Read<'_>, request: &TerminalSessionRequest) -> Result<Vec<Usability>, StoreError> {
        let environment = &request.environment;
        let realised =
            matches!(state_of::<EnvironmentKind>(read, &environment.name, &environment.home)?, Some(LifecycleState::Realised { .. }));
        Ok(vec![Usability { fact: environment.identity(), usable: realised }])
    }
}

/// How often this home's running launches are looked at. This is an edge probe: it reads the
/// pools, screens and blob store and pushes what it finds as facts; the controller itself only
/// wakes on those facts and on its log. Its brief check polls too, because nothing signals a
/// brief's arrival here: the writer's daemon puts the blob in its local tier, then writes the
/// Artifact record, and uploads to the fleet tier in the background, so the replicated record can
/// arrive before the blob is readable from this host, and no tier notifies a reader.
pub const TERMINAL_PROBE_EVERY: Duration = Duration::from_secs(5);

/// This home's probe of its running launches: it pushes each launch it finds ended to the
/// controller, and records what it observes of the rest (attention, the screen, the seat, the
/// crew's path back to the daemon) in memory, and their new output as crew progress. It also looks
/// for the briefs launches wait on, and asks the controller to try each again once it is here.
pub struct TerminalProbe<P: ?Sized> {
    provider: Arc<P>,
    records: Arc<dyn TerminalRecords>,
    progress: Option<Arc<ProgressFacts>>,
    backings: mpsc::Sender<BackingEvent>,
    briefs: Option<(AwaitedBriefs, mpsc::Sender<String>)>,
}

impl<P> TerminalProbe<P>
where
    P: TerminalProvider + ?Sized + 'static,
{
    pub fn new(provider: Arc<P>, records: Arc<dyn TerminalRecords>, backings: mpsc::Sender<BackingEvent>) -> Self {
        Self { provider, records, progress: None, backings, briefs: None }
    }

    /// Looks for the briefs in `briefs`, asking for each launch's next try on `rechecks`.
    pub fn with_briefs(mut self, briefs: AwaitedBriefs, rechecks: mpsc::Sender<String>) -> Self {
        self.briefs = Some((briefs, rechecks));
        self
    }

    /// Records each launch's new output in `progress` (#3108).
    pub fn with_progress(mut self, progress: Arc<ProgressFacts>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Looks at the running launches every `every` until the controller stops.
    pub async fn run(self, every: Duration) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            if !self.pass().await {
                return;
            }
        }
    }

    /// Looks at each running launch once; false once the controller has stopped.
    pub async fn pass(&self) -> bool {
        // A store read, off the async workers.
        let reads = Arc::clone(&self.records);
        let records = match tokio::task::spawn_blocking(move || reads.at(reads.home())).await.unwrap_or_else(|error| Err(error.to_string()))
        {
            Ok(records) => records,
            Err(error) => {
                warn!(%error, "terminal probe could not read this home's launches");
                return true;
            }
        };
        if let Some((briefs, rechecks)) = &self.briefs {
            for record in records.iter().filter(|record| briefs.awaited().contains(&record.name)) {
                match self.provider.brief_ready(record).await {
                    Ok(true) => {
                        briefs.arrived(&record.name);
                        if rechecks.send(record.name.clone()).await.is_err() {
                            return false;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => debug!(launch = %record.name, %error, "terminal probe could not look for the brief"),
                }
            }
        }
        for record in records.into_iter().filter(TerminalRecord::is_running) {
            let Some(session_id) = record.session_id().map(str::to_owned) else { continue };
            match ended(&*self.provider, &record, &session_id).await {
                Ok(Some(observed)) => {
                    self.records.observations().forget(&record.name);
                    if self.backings.send(BackingEvent { name: record.name.clone(), observed }).await.is_err() {
                        return false;
                    }
                }
                Ok(None) => self.observe(&record, &session_id).await,
                Err(cause) => debug!(launch = %record.name, ?cause, "terminal probe could not tell whether the launch runs"),
            }
        }
        true
    }

    async fn observe(&self, record: &TerminalRecord, session_id: &str) {
        let adapter = record.agent().map(|agent| agent.adapter.clone());
        match self.provider.daemon_reach(record).await {
            Ok(unreachable) => {
                self.records.observations().update(&record.name, |observation| observation.daemon_unreachable = unreachable);
            }
            Err(error) => warn!(launch = %record.name, %error, "daemon reach observation failed"),
        }
        let now = Utc::now();
        match self.provider.read_screen(record, session_id).await {
            Ok(Some(reading)) => {
                if let (Some(progress), Some(digest)) = (&self.progress, reading.output_digest.as_deref()) {
                    progress.output(record.session_name(), digest, now);
                }
                // A screen read restates the obstruction; other evidence leaves it.
                let obstruction = reading
                    .attention
                    .as_ref()
                    .filter(|attention| attention.source == flotilla_resources::TerminalAttentionSource::Screen)
                    .map(|_| reading.obstruction.clone());
                self.records.observations().update(&record.name, |observation| {
                    observation.observe(reading.attention, obstruction, reading.occupancy, adapter.as_deref())
                });
            }
            Ok(None) => {
                // Evidence that aged out says nothing any more.
                self.records.observations().update(&record.name, |observation| {
                    if let Some(attention) = observation.attention.as_mut().filter(|attention| attention.is_stale_at(now)) {
                        attention.state = TerminalAttentionState::Unobservable;
                        attention.as_of = now;
                    }
                });
            }
            Err(error) => warn!(launch = %record.name, %error, "terminal screen observation failed"),
        }
    }
}
