//! The Clone controller (#3108) over a decided store, with a recording clone runtime: clones are
//! requested through the clone port, and the outcomes are read back through it and from the
//! runtime.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::{open_home_with, StoreLog};
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::{
    engine::{
        clone_actuator::{CloneActuator, RemoteReachability},
        clones::EngineClones,
        controller::Controller,
        launch_log::{LaunchLog, StagedFacts},
        reads::StoreReads,
        LifecycleHome, Lifecycles,
    },
    reconcilers::CloneRuntime,
    stored::{
        clone::CloneKind,
        decided_registry,
        host::{Condition, HostObservation, Value},
    },
};
use flotilla_provisioning_testkit::environments::EnvironmentFixture;
use flotilla_resources::{
    clone_records::{clone_name, launched, CloneRecords, CloneRenewal},
    environment_records::LaunchFacts,
    remote_condition_type, CloneSpec, EnvironmentSpec, HostDirectEnvironmentSpec, RepositoryKey, RepositorySpec, SystemClock,
};
use flotilla_schema::{NewChange, Timestamp};
use flotilla_schema_sqlite::stream::{CommitListener, Service};
use tokio::sync::Notify;

const HOME: &str = "alpha";
const ENVIRONMENT: &str = "host-direct-alpha";
const URL: &str = "https://forge.example/o/widgets.git";

/// What the runtime was asked, in order, and what it answers.
#[derive(Default)]
struct Asked {
    /// Each clone asked: its environment, URL and path.
    cloned: Vec<(String, String, String)>,
    /// Each inspection of a clone already there: its environment and path.
    inspected: Vec<(String, String)>,
    /// The next answers, in order; once spent, each clone succeeds with branch `main`.
    answers: VecDeque<Result<Option<String>, String>>,
}

#[derive(Default)]
struct RecordingRuntime {
    asked: Mutex<Asked>,
}

impl RecordingRuntime {
    fn answering(answers: impl IntoIterator<Item = Result<Option<String>, String>>) -> Self {
        let runtime = Self::default();
        runtime.asked().answers.extend(answers);
        runtime
    }

    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        self.asked.lock().expect("asked")
    }

    fn answer(&self) -> Result<Option<String>, String> {
        self.asked().answers.pop_front().unwrap_or_else(|| Ok(Some("main".into())))
    }
}

#[async_trait]
impl CloneRuntime for RecordingRuntime {
    async fn clone_and_inspect(&self, _: &str, _: &str) -> Result<Option<String>, String> {
        unreachable!("the controller names the clone's environment")
    }

    async fn inspect_existing(&self, _: &str) -> Result<Option<String>, String> {
        unreachable!("the controller names the clone's environment")
    }

    async fn clone_and_inspect_in(&self, environment: &str, url: &str, path: &str) -> Result<Option<String>, String> {
        self.asked().cloned.push((environment.into(), url.into(), path.into()));
        self.answer()
    }

    async fn inspect_existing_in(&self, environment: &str, path: &str) -> Result<Option<String>, String> {
        self.asked().inspected.push((environment.into(), path.into()));
        self.answer()
    }
}

/// The remotes the controller recorded unreachable.
#[derive(Default)]
struct RecordedRemotes(Mutex<Vec<(String, String)>>);

impl RemoteReachability for RecordedRemotes {
    fn unreachable(&self, remote: &str, url: &str, _: &str) {
        self.0.lock().expect("remotes").push((remote.into(), url.into()));
    }
}

struct World {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    runtime: Arc<RecordingRuntime>,
    remotes: Arc<RecordedRemotes>,
    clones: Arc<EngineClones>,
    environments: EnvironmentFixture,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
}

fn now() -> Timestamp {
    Timestamp::from_datetime(&chrono::Utc::now())
}

fn host_direct() -> EnvironmentSpec {
    EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/work".into() }),
        ..EnvironmentSpec::default()
    }
}

/// The repository cloned: its identity is the canonical remote, which `URL` is a transport of.
fn repository() -> RepositorySpec {
    RepositorySpec::remote("https://forge.example/o/widgets").expect("repository")
}

/// The clone of `repository()` in this host's environment.
fn spec() -> CloneSpec {
    CloneSpec { repo_ref: repository().key(), url: URL.into(), env_ref: ENVIRONMENT.into(), path: "/work/widgets".into() }
}

/// The clone's name, from its repository and environment.
fn name() -> String {
    clone_name(&repository(), ENVIRONMENT).expect("name")
}

impl World {
    /// A decided store with its Clone controller started over `runtime`, and this host's
    /// direct environment requested.
    fn start(runtime: RecordingRuntime) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service =
            Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, decided_registry().expect("registry")).expect("store"));
        let log = StoreLog::new(service.store());
        let staged = StagedFacts::default();
        let lifecycles: Arc<dyn Lifecycles<CloneKind>> =
            Arc::new(LifecycleHome::new(HOME, open_home_with(&log, LaunchLog::new(log.clone(), staged.clone())).expect("clones")));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let environments = EnvironmentFixture::over(HOME, service.store());
        environments.request(ENVIRONMENT, &host_direct(), None);
        let runtime = Arc::new(runtime);
        let remotes = Arc::new(RecordedRemotes::default());
        let actuator = Arc::new(CloneActuator::new(
            Arc::clone(&runtime),
            StoreReads::new(service.store()),
            HOME,
            staged,
            Arc::clone(&remotes) as Arc<dyn RemoteReachability>,
        ));
        let controller =
            Controller::start(Arc::clone(&lifecycles), actuator, Arc::new(SystemClock), Arc::clone(&service)).expect("controller");
        let (_backing, receiver) = tokio::sync::mpsc::channel(1);
        // The sender is kept by the controller's task: a clone's backing is never pushed.
        tokio::spawn(async move {
            let _backing = _backing;
            controller.run(receiver).await;
        });
        Self {
            clones: Arc::new(EngineClones::new(lifecycles, service.store())),
            _directory: directory,
            service,
            runtime,
            remotes,
            environments,
            commits,
            _listener: listener,
        }
    }

    fn environment_ready(&self) {
        self.environments.ready(ENVIRONMENT, "/work", LaunchFacts::default());
    }

    fn reading(&self, name: &str) -> Option<LifecycleReading> {
        self.clones.get(name, None).expect("read").map(|record| record.reading)
    }

    fn ready(&self, name: &str) -> bool {
        matches!(self.reading(name), Some(LifecycleReading::Ready { .. }))
    }

    fn gave_up(&self, name: &str) -> Option<String> {
        match self.reading(name) {
            Some(LifecycleReading::GaveUp { cause }) => Some(cause),
            _ => None,
        }
    }

    fn failures(&self, name: &str) -> Option<u32> {
        match self.reading(name) {
            Some(LifecycleReading::Retrying { failures, .. }) => Some(failures),
            _ => None,
        }
    }

    fn cloned(&self) -> usize {
        self.runtime.asked().cloned.len()
    }

    /// Waits for `holds`, rechecked at each commit.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                tokio::pin!(committed);
                committed.as_mut().enable();
                if holds(self) {
                    return;
                }
                committed.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Writes this host's observation: below the free-space floor, above it or unknown, and how it last found
    /// the remote, if it tried.
    fn observe(&self, below_free_space_floor: Option<bool>, remote: Option<Value>) {
        let conditions = remote
            .map(|value| {
                let condition = Condition {
                    value,
                    reason: "Probe".into(),
                    message: String::new(),
                    blocks_readiness: false,
                    blocks_forge_observation: false,
                };
                BTreeMap::from([(remote_condition_type("forge.example"), condition)])
            })
            .unwrap_or_default();
        let observation = HostObservation { below_free_space_floor, conditions, ..HostObservation::default() };
        let store = self.service.store();
        let mut store = store.lock().expect("store");
        let mut write = store.write().expect("write");
        let change = write.open_change(&NewChange::new("probe", now())).expect("change");
        match write.resolve(<HostObservation as flotilla_schema::Kind>::CODE, HOME, HOME).expect("resolve") {
            Some(id) => {
                let current = write.load::<HostObservation>(id).expect("load").expect("held");
                write.update(&current, &observation, change).expect("update");
            }
            None => {
                write.insert(HOME, &observation, change).expect("insert");
            }
        }
        write.commit().expect("commit");
    }

    /// Waits until the controller has passed every change made before: a clone of another
    /// repository, from another remote into an environment of another host, whose observation
    /// says nothing, is realised.
    async fn passed(&self, marker: &str) {
        let environment = format!("host-direct-{marker}");
        if self.environments.lifecycles.get(&environment).is_none() {
            let spec = EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: marker.into(), repo_default_dir: "/elsewhere".into() }),
                ..EnvironmentSpec::default()
            };
            self.environments.put_ready(&environment, &spec, None, "/elsewhere", LaunchFacts::default());
        }
        let repository = RepositorySpec::remote(format!("https://{marker}.example/o/r")).expect("repository");
        let spec = CloneSpec {
            repo_ref: repository.key(),
            url: format!("https://{marker}.example/o/r.git"),
            env_ref: environment,
            path: format!("/elsewhere/{marker}"),
        };
        let launch = self.clones.request(&repository, &spec, false).expect("marker");
        self.until(marker, |world| world.ready(&launch)).await;
    }
}

// Behaviour: a clone waits, untried, while its environment is not realised; once it is, the clone
// is made at its path and realised under it, with the default branch it found, and is found
// among its repository's clones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clone_waits_for_its_environment_then_clones_once() {
    let world = World::start(RecordingRuntime::default());
    let launch = world.clones.request(&repository(), &spec(), false).expect("request");
    assert_eq!(launched(&launch), name());
    assert_eq!(world.reading(&launch), Some(LifecycleReading::InProgress));
    world.observe(Some(false), None);
    assert_eq!(world.cloned(), 0, "untried while its environment is not realised");
    world.environment_ready();
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.runtime.asked().cloned, vec![(ENVIRONMENT.into(), URL.into(), "/work/widgets".into())]);
    let record = world.clones.get(&name(), None).expect("read").expect("held");
    assert_eq!((record.name, record.default_branch.as_deref()), (launch.clone(), Some("main")));
    assert_eq!(record.reading, LifecycleReading::Ready { identity: "/work/widgets".into() });
    let found = world.clones.of_repository(&repository().key()).expect("index");
    assert_eq!(found.into_iter().map(|record| record.name).collect::<Vec<_>>(), std::slice::from_ref(&launch));
    // A request while it is realised is served by it.
    assert_eq!(world.clones.request(&repository(), &spec(), false).expect("again"), launch);
    assert_eq!(world.cloned(), 1);
}

// Behaviour: a clone startup discovery found is inspected where it is, never cloned over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discovered_clone_is_inspected_not_cloned() {
    let world = World::start(RecordingRuntime::default());
    world.environment_ready();
    let launch = world.clones.request(&repository(), &spec(), true).expect("request");
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.runtime.asked().inspected, vec![(ENVIRONMENT.into(), "/work/widgets".into())]);
    assert_eq!(world.cloned(), 0);
    assert!(world.clones.get(&launch, None).expect("read").expect("held").discovered);
}

// Behaviour: a misconfiguration gives up at once. A new request starts the next launch, which is
// tried afresh; renewing a live clone starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_misconfigured_clone_gives_up_at_once_and_a_new_request_launches_again() {
    let world =
        World::start(RecordingRuntime::answering([Err("clone target /work/widgets already exists with origin x, expected y".into())]));
    world.environment_ready();
    let first = world.clones.request(&repository(), &spec(), false).expect("request");
    world.until("gave up", |world| world.gave_up(&first).is_some()).await;
    assert!(world.gave_up(&first).is_some_and(|cause| cause.contains("already exists with origin")));
    assert_eq!(world.cloned(), 1);
    let second = world.clones.request(&repository(), &spec(), false).expect("request again");
    assert_eq!(launched(&second), name());
    assert_ne!(second, first);
    world.until("the next launch ready", |world| world.ready(&second)).await;
    assert_eq!(world.cloned(), 2);
    assert!(world.gave_up(&first).is_some(), "the launch that gave up is never revived");
    assert!(matches!(world.clones.renew(&name()), Ok(CloneRenewal::Live(record)) if record.name == second));
}

// Behaviour: a transport failure records the remote unreachable at once, and the clone waits,
// untried, while the host's observation says so; the probe reaching the remote again wakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_failure_waits_for_the_remote_to_be_reached_again() {
    let world = World::start(RecordingRuntime::answering([Err(
        "fatal: unable to access 'https://forge.example/o/widgets.git/': Could not resolve host: forge.example".into(),
    )]));
    world.observe(Some(false), None);
    world.environment_ready();
    let launch = world.clones.request(&repository(), &spec(), false).expect("request");
    world.until("a failure", |world| world.failures(&launch) == Some(1)).await;
    assert_eq!(*world.remotes.0.lock().expect("remotes"), vec![("forge.example".to_owned(), URL.to_owned())]);
    world.observe(Some(false), Some(Value::False));
    world.passed("marker").await;
    assert_eq!(world.cloned(), 2, "untried while the remote is unreachable: the marker's clone is the second");
    world.observe(Some(false), Some(Value::True));
    world.until("ready", |world| world.ready(&launch)).await;
    assert_eq!(world.cloned(), 3);
}

// Behaviour: a full disk waits for the host's free-space fact, below the floor or unknown alike,
// and is tried again once the host is known to be back above its floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_disk_waits_for_free_space() {
    let world = World::start(RecordingRuntime::answering([Err("error: could not write: No space left on device".into())]));
    world.observe(Some(false), None);
    world.environment_ready();
    let launch = world.clones.request(&repository(), &spec(), false).expect("request");
    world.until("a failure", |world| world.failures(&launch) == Some(1)).await;
    world.observe(Some(true), None);
    world.passed("marker").await;
    assert_eq!(world.cloned(), 2, "untried below the floor");
    world.observe(None, None);
    world.passed("marker").await;
    assert_eq!(world.cloned(), 2, "untried while free space is unknown");
    world.observe(Some(false), None);
    world.until("ready", |world| world.ready(&launch)).await;
}

// Behaviour: a clone gives up after its third failed try; renewing it starts its next launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clone_gives_up_after_three_tries_and_is_renewed() {
    let world = World::start(RecordingRuntime::answering((0..3).map(|_| Err("fatal: early EOF".to_owned()))));
    world.observe(Some(false), None);
    world.environment_ready();
    let launch = world.clones.request(&repository(), &spec(), false).expect("request");
    world.until("a failure", |world| world.failures(&launch) == Some(1)).await;
    // Each change of what it waits on is another try.
    world.observe(Some(true), None);
    world.observe(Some(false), None);
    world.until("a second failure", |world| world.failures(&launch) == Some(2)).await;
    world.observe(Some(true), None);
    world.observe(Some(false), None);
    world.until("gave up", |world| world.gave_up(&launch).is_some()).await;
    assert_eq!(world.cloned(), 3);
    let Ok(CloneRenewal::Started(next)) = world.clones.renew(&name()) else { panic!("renewal") };
    assert_eq!(launched(&next), name());
    assert_ne!(next, launch);
    world.until("the renewed launch ready", |world| world.ready(&next)).await;
}

// Behaviour: a clone is named for its repository's identity in its environment, whatever
// transport it is cloned over, so two transports of one repository share one clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clone_is_named_for_its_repository_not_its_transport() {
    let world = World::start(RecordingRuntime::default());
    let alias = CloneSpec { url: "git@forge-alias:o/widgets.git".into(), ..spec() };
    let launch = world.clones.request(&repository(), &alias, false).expect("request");
    assert_eq!(launched(&launch), name());
    assert_eq!(world.clones.request(&repository(), &spec(), false).expect("the other transport"), launch);
}

// Behaviour: a request whose spec names another repository than the one it is for is refused,
// and nothing is requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spec_for_another_repository_is_refused() {
    let world = World::start(RecordingRuntime::default());
    let other = CloneSpec { repo_ref: RepositoryKey("gadgets".into()), ..spec() };
    let refused = world.clones.request(&repository(), &other, false).expect_err("refused");
    assert!(refused.contains("repository key gadgets"), "{refused}");
    assert_eq!(world.clones.get(&name(), None).expect("read"), None);
}
