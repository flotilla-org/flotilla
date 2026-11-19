//! The TerminalSession controller (#3108) over a decided store, with a recording terminal
//! provider: sessions are requested through the terminal port, held for a vessel's lease or their
//! environment, and looked at by this home's probe; the outcomes are read back through the port
//! and from the provider.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::{
    engine::{
        controller::Controller,
        reads::StoreReads,
        terminal_actuator::{
            LaunchFailure, ScreenReading, TerminalActuator, TerminalLaunched, TerminalLiveness, TerminalProbe, TerminalProvider,
        },
    },
    stored::decided_registry,
};
use flotilla_provisioning_testkit::{EngineFixture, TerminalsFixture};
use flotilla_resources::{
    environment_records::LaunchFacts,
    terminal_records::{TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource, TerminalRecord, TerminalRecords},
    vessel_records::VesselStep,
    DockerEnvironmentSpec, EnvironmentSpec, HostDirectEnvironmentSpec, RetryBackoff, SystemClock, TerminalOccupancy,
};
use flotilla_schema_sqlite::stream::{CommitListener, Service};
use tokio::sync::Notify;

const HOME: &str = "home";
const ENVIRONMENT: &str = "host-direct-home";

/// What the provider was asked, in order, and what it answers.
#[derive(Default)]
struct Asked {
    /// Each launch started, by launch name.
    launches: Vec<String>,
    settled: Vec<String>,
    killed: Vec<String>,
    receipts_removed: Vec<String>,
    cleaned: Vec<String>,
    /// Whether the brief's artifact is still missing from this host.
    brief_missing: bool,
    /// Why a launch fails to start, when it does.
    launch_error: Option<LaunchFailure>,
    /// What a launch waits for besides its brief, such as its environment's adoption.
    waits_for: Option<String>,
    /// What the pool says of the session; running when unset.
    liveness: Option<TerminalLiveness>,
    /// The code an agent's exit receipt holds, once it exited.
    exit_code: Option<i32>,
    /// Why the harness says its exit failed it.
    exit_failure: Option<String>,
    /// Why the crew cannot reach the daemon.
    unreachable: Option<String>,
    /// Why the agent's settled screen says it cannot work.
    screen_failure: Option<String>,
    /// Why the session cannot be reached to kill it, as when its environment was not adopted.
    unreachable_pool: Option<String>,
    /// Each launch a person was told may still run, and the reason they read.
    left_running: Vec<(String, String)>,
}

#[derive(Default)]
struct RecordingProvider {
    asked: Mutex<Asked>,
}

impl RecordingProvider {
    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        self.asked.lock().expect("asked")
    }
}

#[async_trait]
impl TerminalProvider for RecordingProvider {
    async fn launch_waits_for(&self, _: &TerminalRecord) -> Result<Option<String>, String> {
        let asked = self.asked();
        Ok(asked.brief_missing.then(|| "its brief artifact is not on this host yet".into()).or_else(|| asked.waits_for.clone()))
    }

    async fn launch(&self, launch: &TerminalRecord) -> Result<TerminalLaunched, LaunchFailure> {
        let mut asked = self.asked();
        asked.launches.push(launch.name.clone());
        if let Some(error) = asked.launch_error.clone() {
            return Err(error);
        }
        Ok(TerminalLaunched {
            session_id: format!("pool-{}", launch.name),
            facts: TerminalLaunchFacts { endpoint: Some(("/run/cleat".into(), "cleatd".into())), ..TerminalLaunchFacts::default() },
            launch_command: format!("run {}", launch.name),
        })
    }

    async fn liveness(&self, _: &TerminalRecord, _: &str) -> Result<TerminalLiveness, String> {
        Ok(self.asked().liveness.clone().unwrap_or(TerminalLiveness::Running))
    }

    async fn exit_code(&self, _: &TerminalRecord) -> Result<Option<i32>, String> {
        Ok(self.asked().exit_code)
    }

    async fn exit_failure(&self, _: &TerminalRecord, _: &str, _: i32) -> Result<Option<String>, String> {
        Ok(self.asked().exit_failure.clone())
    }

    async fn screen_failure(&self, _: &TerminalRecord, _: &str) -> Result<Option<String>, String> {
        Ok(self.asked().screen_failure.clone())
    }

    async fn daemon_reach(&self, _: &TerminalRecord) -> Result<Option<String>, String> {
        Ok(self.asked().unreachable.clone())
    }

    async fn read_screen(&self, _: &TerminalRecord, _: &str) -> Result<Option<ScreenReading>, String> {
        Ok(Some(ScreenReading { output_digest: None, attention: None, obstruction: None, occupancy: TerminalOccupancy::default() }))
    }

    async fn settle_turn(&self, session_id: &str) {
        self.asked().settled.push(session_id.into());
    }

    async fn kill(&self, _: &TerminalRecord, session_id: &str) -> Result<(), String> {
        let mut asked = self.asked();
        asked.killed.push(session_id.into());
        asked.unreachable_pool.clone().map_or(Ok(()), Err)
    }

    async fn remove_exit_receipt(&self, launch: &TerminalRecord) -> Result<(), String> {
        self.asked().receipts_removed.push(launch.name.clone());
        Ok(())
    }

    async fn cleanup_artifacts(&self, launch: &TerminalRecord) -> Result<(), String> {
        self.asked().cleaned.push(launch.name.clone());
        Ok(())
    }

    async fn left_running(&self, launch: &TerminalRecord, reason: &str) {
        self.asked().left_running.push((launch.name.clone(), reason.into()));
    }
}

struct World {
    _directory: tempfile::TempDir,
    provider: Arc<RecordingProvider>,
    /// The vessel leases sessions are held for, and the environments they run in.
    vessels: EngineFixture,
    terminals: TerminalsFixture,
    /// This home's probe, passed by hand.
    probe: TerminalProbe<RecordingProvider>,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
}

fn host_direct() -> EnvironmentSpec {
    EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    }
}

/// A container, whose teardown ends the processes run in it.
fn docker() -> EnvironmentSpec {
    EnvironmentSpec {
        docker: Some(DockerEnvironmentSpec {
            host_ref: HOME.into(),
            image: "crew:pinned".into(),
            image_build_ref: None,
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            mounts: vec![],
            env: Default::default(),
        }),
        ..EnvironmentSpec::default()
    }
}

/// How often a failed release is tried in these tests, and how soon again.
const RELEASE_TRIES: u32 = 3;
const RELEASE_BACKOFF: RetryBackoff = RetryBackoff { initial: Duration::from_millis(20), maximum: Duration::from_millis(20) };

fn tool(session: &str) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        session: session.into(),
        environment: ENVIRONMENT.into(),
        vessel: None,
        role: "shell".into(),
        pool: "cleat".into(),
        cwd: "/work".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: "bash".into() },
    }
}

impl World {
    /// A decided store with its TerminalSession controller started over `provider`, and this
    /// host's direct environment requested, realised when `ready`.
    fn start(provider: RecordingProvider, ready: bool) -> Self {
        Self::start_in(provider, ready, &host_direct())
    }

    /// As [`World::start`], with the environment sessions run in made from `environment`.
    fn start_in(provider: RecordingProvider, ready: bool, environment: &EnvironmentSpec) -> Self {
        Self::start_with(provider, ready, environment, RELEASE_TRIES)
    }

    /// As [`World::start_in`], a failed release tried at most `release_tries` times.
    fn start_with(provider: RecordingProvider, ready: bool, environment: &EnvironmentSpec, release_tries: u32) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service =
            Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, decided_registry().expect("registry")).expect("store"));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let vessels = EngineFixture::over(HOME, service.store());
        vessels.environments.request(ENVIRONMENT, environment, None);
        if ready {
            vessels.environments.ready(ENVIRONMENT, "/work", LaunchFacts::default());
        }
        let terminals = TerminalsFixture::over(HOME, service.store());
        let provider = Arc::new(provider);
        let records = Arc::clone(&terminals.terminals) as Arc<dyn TerminalRecords>;
        let actuator = TerminalActuator::new(
            Arc::clone(&provider),
            Arc::clone(&records),
            StoreReads::new(service.store()),
            HOME,
            terminals.staged.clone(),
        )
        .with_release_retry(RELEASE_BACKOFF, release_tries);
        let briefs = actuator.awaited_briefs();
        let controller =
            Controller::start(Arc::clone(&terminals.lifecycles), Arc::new(actuator), Arc::new(SystemClock), service).expect("controller");
        let (backings, backing_events) = tokio::sync::mpsc::channel(8);
        let (rechecks, recheck_events) = tokio::sync::mpsc::channel(8);
        tokio::spawn(controller.run_with(backing_events, recheck_events));
        let probe = TerminalProbe::new(Arc::clone(&provider), records, backings).with_briefs(briefs, rechecks);
        Self { _directory: directory, provider, vessels, terminals, probe, commits, _listener: listener }
    }

    /// A crew's agent session for a launch of work `work`, its lease held here; the session and
    /// the launch.
    fn agent(&self, work: &str) -> (String, String) {
        let lease = self.vessels.vessels.request("c1", work, HOME);
        (self.terminals.request(&TerminalsFixture::agent(&lease, "lead", ENVIRONMENT)), lease)
    }

    fn reading(&self, launch: &str) -> LifecycleReading {
        self.terminals.record(launch).reading
    }

    fn ready(&self, launch: &str) -> bool {
        matches!(self.reading(launch), LifecycleReading::Ready { .. })
    }

    /// Waits for `holds`, rechecked at each commit and every 50ms.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                tokio::pin!(committed);
                committed.as_mut().enable();
                if holds(self) {
                    return;
                }
                let _ = tokio::time::timeout(Duration::from_millis(50), committed).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// An agent session for `vessel`, launched and realised, and its lease.
    async fn running_agent(&self, vessel: &str) -> (String, String) {
        let (launch, lease) = self.agent(vessel);
        self.until("ready", |world| world.ready(&launch)).await;
        (launch, lease)
    }
}

// Behaviour: a session waits, unstarted, while its environment is not realised; once it is, the
// session starts once, realised as its pool session with what the launch was given recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_waits_for_its_environment_then_starts_once() {
    let world = World::start(RecordingProvider::default(), false);
    let launch = world.terminals.request(&tool("shell"));
    assert_eq!(world.reading(&launch), LifecycleReading::InProgress);
    assert!(world.provider.asked().launches.is_empty(), "unstarted while its environment is not realised");
    world.vessels.environments.ready(ENVIRONMENT, "/work", LaunchFacts::default());
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.provider.asked().launches, vec![launch.clone()]);
    let record = world.terminals.record(&launch);
    let expected = format!("pool-{launch}");
    assert_eq!(record.session_id(), Some(expected.as_str()));
    assert_eq!(record.facts.and_then(|facts| facts.endpoint), Some(("/run/cleat".into(), "cleatd".into())));
    let observation = world.terminals.terminals.observations().get(&launch).expect("observed");
    assert_eq!(observation.launch_command, Some(format!("run {launch}")));
}

// Behaviour: a brief whose artifact is not on this host fails its try and waits; the probe finds
// the artifact once it arrives and the session starts then, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_waits_for_its_brief_artifact_then_starts() {
    let world = World::start(RecordingProvider::default(), true);
    world.provider.asked().brief_missing = true;
    let (launch, _lease) = world.agent("v1");
    world.until("retrying", |world| matches!(world.reading(&launch), LifecycleReading::Retrying { .. })).await;
    assert!(world.probe.pass().await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(world.provider.asked().launches.is_empty(), "not started while its brief is missing");
    world.provider.asked().brief_missing = false;
    assert!(world.probe.pass().await);
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.provider.asked().launches, vec![launch]);
}

const UNREGISTERED: &str = "environment host-direct-home is not registered on this host yet";

// Behaviour: a launch that reaches its pool before its environment is registered here, as after a
// restart before the environment's adoption, fails a retryable try recorded on the launch rather
// than giving up. The probe does not try it again while the registration is missing, and starts it
// once it is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_waiting_for_its_environments_adoption_starts_once_it_is_registered() {
    let world = World::start(RecordingProvider::default(), true);
    world.provider.asked().launch_error = Some(LaunchFailure::waits(UNREGISTERED));
    let (launch, _lease) = world.agent("v1");
    world.until("retrying", |world| matches!(world.reading(&launch), LifecycleReading::Retrying { .. })).await;
    let LifecycleReading::Retrying { failures, cause, .. } = world.reading(&launch) else { unreachable!("retrying") };
    assert_eq!(failures, 1);
    assert!(cause.contains(UNREGISTERED), "{cause}");
    {
        let mut asked = world.provider.asked();
        asked.launch_error = None;
        asked.waits_for = Some(UNREGISTERED.into());
    }
    assert!(world.probe.pass().await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(world.provider.asked().launches, vec![launch.clone()], "not tried again while unregistered");
    world.provider.asked().waits_for = None;
    assert!(world.probe.pass().await);
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.provider.asked().launches, vec![launch.clone(), launch]);
}

// Behaviour: a launch waiting for its environment's adoption is released when its vessel ends, as
// when the restart lost the environment and the vessel's next launch replaces it: it does not
// stay listed beside its successor's session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_waiting_for_its_environment_is_released_with_its_vessel() {
    let world = World::start(RecordingProvider::default(), true);
    world.provider.asked().launch_error = Some(LaunchFailure::waits(UNREGISTERED));
    let (launch, lease) = world.agent("v1");
    world.until("retrying", |world| matches!(world.reading(&launch), LifecycleReading::Retrying { .. })).await;
    world.vessels.vessels.ready(&lease, "vessel-v1");
    world.vessels.vessels.step(&lease, VesselStep::Lost { reason: "its environment disappeared".into() });
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
}

// Behaviour: a launch that cannot start gives up at once, tried once; its holder renews it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_that_cannot_start_gives_up() {
    let world = World::start(RecordingProvider::default(), true);
    world.provider.asked().launch_error = Some(LaunchFailure::refused("terminal pool cleat unavailable"));
    let (launch, _lease) = world.agent("v1");
    world.until("gave up", |world| matches!(world.reading(&launch), LifecycleReading::GaveUp { .. })).await;
    assert_eq!(world.provider.asked().launches, vec![launch]);
}

// Behaviour: an agent whose exit receipt is written ends exited, read as stopped for a resume, and
// what was observed of it is forgotten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exited_agent_ends_awaiting_its_resume() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    world.provider.asked().exit_code = Some(0);
    assert!(world.probe.pass().await);
    world.until("ended", |world| matches!(world.reading(&launch), LifecycleReading::Lost { .. })).await;
    assert!(world.terminals.record(&launch).exited());
    assert!(world.terminals.terminals.observations().get(&launch).is_none());
}

// Behaviour: an agent whose harness says its exit failed it ends failed, not awaiting a resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_failure_on_exit_fails_the_launch() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    {
        let mut asked = world.provider.asked();
        asked.exit_code = Some(1);
        asked.exit_failure = Some("credential refused".into());
    }
    assert!(world.probe.pass().await);
    world.until("failed", |world| matches!(world.reading(&launch), LifecycleReading::Failed { .. })).await;
}

// Behaviour: an agent whose screen shows its harness refused its credential fails, though its
// session still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_credential_on_screen_fails_a_running_agent() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    world.provider.asked().screen_failure = Some("credential `anthropic` was refused".into());
    assert!(world.probe.pass().await);
    world.until("failed", |world| matches!(world.reading(&launch), LifecycleReading::Failed { .. })).await;
}

// Behaviour: a pool that cannot be asked is no evidence the session is gone: the launch holds,
// not started again. A pool that says the session is gone ends it lost, not exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswering_pool_holds_and_a_gone_session_is_lost() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    world.provider.asked().liveness = Some(TerminalLiveness::Unavailable("cleat socket refused".into()));
    assert!(world.probe.pass().await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(world.ready(&launch), "held while the pool cannot be asked");
    assert_eq!(world.provider.asked().launches.len(), 1);
    world.provider.asked().liveness = Some(TerminalLiveness::Lost("session gone from the pool".into()));
    assert!(world.probe.pass().await);
    world.until("lost", |world| matches!(world.reading(&launch), LifecycleReading::Lost { .. })).await;
    assert!(!world.terminals.record(&launch).exited());
}

// Behaviour: the probe records, in memory, why a running crew cannot reach the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_records_what_it_observes_of_a_running_launch() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    world.provider.asked().unreachable = Some("no route to the daemon".into());
    assert!(world.probe.pass().await);
    let observation = world.terminals.terminals.observations().get(&launch).expect("observed");
    assert_eq!(observation.daemon_unreachable.as_deref(), Some("no route to the daemon"));
    assert!(world.ready(&launch));
}

// Behaviour (d9): the probe reads only the launches that can still change. Ended launches are
// retained history; with three ended and one running, a pass reads the running one alone, so an
// idle home's probe costs nothing per launch it has ever run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_reads_only_the_launches_that_have_not_ended() {
    let world = World::start(RecordingProvider::default(), true);
    for vessel in ["v1", "v2", "v3"] {
        let (launch, lease) = world.running_agent(vessel).await;
        world.vessels.vessels.ready(&lease, &format!("vessel-{vessel}"));
        world.vessels.vessels.step(&lease, VesselStep::Released);
        world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    }
    let (running, _lease) = world.running_agent("v4").await;
    let before = world.terminals.terminals.launches_read();
    assert!(world.probe.pass().await);
    assert_eq!(world.terminals.terminals.launches_read() - before, 1, "only the running launch is read");
    assert!(world.terminals.terminals.observations().get(&running).is_some(), "the running launch is still looked at");
}

// Behaviour: once its vessel's lease ends, a crew's session is released: a turn already typed
// settles, then the process is killed and its exit receipt and harness files removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_lease_releases_its_session() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, lease) = world.running_agent("v1").await;
    world.vessels.vessels.ready(&lease, "vessel-v1");
    world.vessels.vessels.step(&lease, VesselStep::Lost { reason: "its environment disappeared".into() });
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    let asked = world.provider.asked();
    let session = format!("pool-{launch}");
    assert_eq!(asked.settled, vec![session.clone()]);
    assert_eq!(asked.killed, vec![session]);
    assert_eq!(asked.receipts_removed, vec![launch.clone()]);
    assert_eq!(asked.cleaned, vec![launch]);
}

// Behaviour: an ended vessel's session that cannot be reached, as when a restart did not adopt
// its environment, is released all the same when its environment is a container: its
// environment's release waits for it, and that teardown removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_vessels_unreachable_session_is_left_to_its_environment() {
    let world = World::start_in(RecordingProvider::default(), true, &docker());
    let (launch, lease) = world.running_agent("v1").await;
    world.provider.asked().unreachable_pool = Some("provider registry unavailable".into());
    world.vessels.vessels.ready(&lease, "vessel-v1");
    world.vessels.vessels.step(&lease, VesselStep::Released);
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    assert_eq!(world.provider.asked().killed, vec![format!("pool-{launch}")], "it was asked once");
}

// Behaviour: an ended container took its sessions with it: they are released without asking
// its gone runtime anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_environment_releases_its_sessions_without_provider_work() {
    let world = World::start_in(RecordingProvider::default(), true, &docker());
    let (launch, _lease) = world.running_agent("v1").await;
    world.vessels.environments.lose(ENVIRONMENT, "container removed");
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    let asked = world.provider.asked();
    assert!(asked.killed.is_empty() && asked.cleaned.is_empty() && asked.receipts_removed.is_empty());
}

// Behaviour (item 12): a host-direct environment's end stops none of its processes, so its
// sessions are still killed and cleaned when they are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_host_direct_environments_sessions_are_still_killed() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, _lease) = world.running_agent("v1").await;
    world.vessels.environments.lose(ENVIRONMENT, "host-direct environment ended");
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    let asked = world.provider.asked();
    assert_eq!(asked.killed, vec![format!("pool-{launch}")]);
    assert_eq!(asked.cleaned, vec![launch]);
}

// Behaviour (item 12): on a host-direct environment nothing else ends an ended vessel's session,
// so a failed kill is not left to the environment: the release is tried again on time, and the
// session is released once its kill succeeds, with no person told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_vessels_host_direct_session_whose_kill_fails_is_tried_again() {
    // A limit this test never reaches: the release is tried again until the kill succeeds.
    let world = World::start_with(RecordingProvider::default(), true, &host_direct(), u32::MAX);
    let (launch, lease) = world.running_agent("v1").await;
    world.provider.asked().unreachable_pool = Some("cleat socket refused".into());
    world.vessels.vessels.ready(&lease, "vessel-v1");
    world.vessels.vessels.step(&lease, VesselStep::Released);
    world.until("tried again", |world| world.provider.asked().killed.len() >= 2).await;
    assert_ne!(world.reading(&launch), LifecycleReading::Released, "not released while its kill fails");
    world.provider.asked().unreachable_pool = None;
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    assert!(world.provider.asked().left_running.is_empty());
}

// Behaviour (item 12): a host-direct session whose kill keeps failing is tried a bounded number of
// times; then a person is told, once, that its process may still run, and it is released so its
// vessel's teardown goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_direct_session_whose_kill_keeps_failing_is_told_to_a_person_and_released() {
    let world = World::start(RecordingProvider::default(), true);
    let (launch, lease) = world.running_agent("v1").await;
    world.provider.asked().unreachable_pool = Some("cleat socket refused".into());
    world.vessels.vessels.ready(&lease, "vessel-v1");
    world.vessels.vessels.step(&lease, VesselStep::Released);
    world.until("released", |world| world.reading(&launch) == LifecycleReading::Released).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let asked = world.provider.asked();
    assert_eq!(asked.killed.len(), RELEASE_TRIES as usize, "tried the bounded number of times, then no more");
    let [(told, reason)] = asked.left_running.as_slice() else { panic!("told once: {:?}", asked.left_running) };
    assert_eq!(told, &launch);
    assert!(reason.contains("cleat socket refused") && reason.contains("may still be running"), "{reason}");
}
