//! The shared controller over a decided store, with a scripted actuator: each test drives it by
//! the events the design names, and reads the outcome from the engine's records.
//!
//! The scripted actuator keys each environment by its host-direct root. Its records live as
//! Clone lifecycles at the same home: `input-<key>` is an input, `holder-<key>` ends the holder
//! once Gone, `fact-<key>` reports the provider usable while Realised, and a live `served-<key>`
//! holds its release back.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::{open_home, open_home_with, StoreLog};
use flotilla_resources::SystemClock;
use flotilla_schema::{Frozen, Identity, Kind, KindCode, Ref, Timestamp};
use flotilla_schema_sqlite::{
    stream::{CommitListener, Service},
    Read, StoreError,
};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleState},
    Cause, CauseCategory, Resource,
};
use tokio::sync::{mpsc, Notify};

use super::{Actuator, Backing, BackingEvent, Controller, Holding, ProviderFailure, Usability};
use crate::{
    engine::{launch, LifecycleHome, Lifecycles, Step},
    stored::{
        clone::{CloneKind, CloneRequest},
        environment::{EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe},
    },
};

const HOME: &str = "alpha";

fn clone_code() -> KindCode {
    <Resource<Lifecycle<CloneKind>> as Kind>::CODE
}

fn record(name: &str) -> Identity {
    Identity { kind: clone_code(), name: name.to_owned(), home: HOME.to_owned() }
}

fn key(request: &EnvironmentRequest) -> String {
    request.recipe.value().read::<HostDirectRecipe>().expect("recipe").repo_default_dir
}

fn environment(key: &str) -> EnvironmentRequest {
    EnvironmentRequest {
        provider: "host:alpha/provider:host-direct".parse().expect("provider"),
        image_build: None,
        recipe: Frozen::new(Recipe::of(&HostDirectRecipe { repo_default_dir: key.into() }).expect("recipe")).expect("freeze"),
        holder: None,
    }
}

fn clone_request() -> CloneRequest {
    CloneRequest {
        repository: "repo".into(),
        url: "https://example/repo".into(),
        environment: Ref::new("env", HOME),
        path: "/c".into(),
        discovered: false,
    }
}

fn state_of(read: &mut Read<'_>, name: &str) -> Result<Option<LifecycleState>, StoreError> {
    let Some(id) = read.resolve(clone_code(), name, HOME)? else { return Ok(None) };
    Ok(read.load::<Resource<Lifecycle<CloneKind>>>(id)?.map(|loaded| loaded.value.state().clone()))
}

#[derive(Default)]
struct Script {
    /// Each key's realise results, in turn; an empty queue realises under `id-<key>`.
    realise: BTreeMap<String, VecDeque<Result<String, ProviderFailure>>>,
    /// What observe finds, by key; absent when unset.
    present: BTreeMap<String, String>,
    /// The keys whose provider a fact reports.
    with_fact: BTreeSet<String>,
    realised: Vec<String>,
    released: Vec<(String, Option<String>)>,
}

#[derive(Default)]
struct Scripted(Mutex<Script>);

impl Scripted {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.0.lock().expect("script")
    }
}

#[async_trait]
impl Actuator<EnvironmentKind> for Scripted {
    async fn observe(&self, _: &str, request: &EnvironmentRequest) -> Result<Backing, Cause> {
        Ok(match self.script().present.get(&key(request)) {
            Some(identity) => Backing::Present { identity: identity.clone() },
            None => Backing::Absent { reason: None },
        })
    }

    async fn realise(&self, _: &str, request: &EnvironmentRequest) -> Result<String, ProviderFailure> {
        let key = key(request);
        let mut script = self.script();
        script.realised.push(key.clone());
        script.realise.get_mut(&key).and_then(VecDeque::pop_front).unwrap_or_else(|| Ok(format!("id-{key}")))
    }

    async fn release(&self, _: &str, request: &EnvironmentRequest, identity: Option<&str>) -> Result<(), ProviderFailure> {
        self.script().released.push((key(request), identity.map(str::to_owned)));
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![clone_code()]
    }

    fn dependencies(&self, request: &EnvironmentRequest) -> Vec<Identity> {
        let key = key(request);
        vec![record(&format!("input-{key}")), record(&format!("holder-{key}"))]
    }

    fn holder(&self, read: &mut Read<'_>, _: &str, request: &EnvironmentRequest) -> Result<Holding, StoreError> {
        Ok(Holding::ended(state_of(read, &format!("holder-{}", key(request)))?.is_some_and(|state| state.gone())))
    }

    fn usability(&self, read: &mut Read<'_>, request: &EnvironmentRequest) -> Result<Vec<Usability>, StoreError> {
        let key = key(request);
        if !self.script().with_fact.contains(&key) {
            return Ok(Vec::new());
        }
        let name = format!("fact-{key}");
        let usable = state_of(read, &name)?.is_some_and(|state| state.realised());
        Ok(vec![Usability { fact: record(&name), usable }])
    }

    fn release_blockers(&self, read: &mut Read<'_>, _: &str, request: &EnvironmentRequest) -> Result<Vec<Identity>, StoreError> {
        let name = format!("served-{}", key(request));
        let live = state_of(read, &name)?.is_some_and(|state| state.live());
        Ok(if live { vec![record(&name)] } else { Vec::new() })
    }
}

/// A retryable or final failure.
fn failure(detail: &str, retryable: bool) -> ProviderFailure {
    ProviderFailure { cause: Cause::new(CauseCategory::Transient, detail), retryable }
}

struct World {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    environments: Arc<LifecycleHome<EnvironmentKind, StoreLog>>,
    clones: Arc<LifecycleHome<CloneKind, StoreLog>>,
    actuator: Arc<Scripted>,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
    backing: Option<mpsc::Sender<BackingEvent>>,
}

impl World {
    fn open() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let registry = crate::stored::decided_registry().expect("registry");
        let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, registry).expect("store"));
        let log = || StoreLog::new(service.store());
        let environments = Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("environments")));
        let clones = Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("clones")));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move || commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        Self { _directory: directory, service, environments, clones, actuator: Arc::default(), commits, _listener: listener, backing: None }
    }

    /// Starts the controller over the records as they are now.
    fn start(&mut self) {
        let controller = Controller::start(
            Arc::clone(&self.environments) as Arc<dyn Lifecycles<EnvironmentKind>>,
            Arc::clone(&self.actuator),
            Arc::new(SystemClock),
            Arc::clone(&self.service),
        )
        .expect("controller");
        let (sender, receiver) = mpsc::channel(8);
        self.backing = Some(sender);
        tokio::spawn(controller.run(receiver));
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&chrono::Utc::now())
    }

    fn request(&self, name: &str) {
        self.environments.request(name, environment(name), Self::now()).expect("request");
    }

    fn clone_record(&self, name: &str) {
        self.clones.request(name, clone_request(), Self::now()).expect("clone");
    }

    fn clone_step(&self, name: &str, step: Step) {
        self.clones.step(name, &step, Self::now()).expect("clone step");
    }

    fn state(&self, name: &str) -> LifecycleState {
        self.environments.get(name).expect("held").state().clone()
    }

    /// Waits for `holds` to hold of the environments, rechecked at each commit.
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
}

fn failures(state: &LifecycleState) -> Option<u32> {
    match state {
        LifecycleState::Requested { failures, .. } => Some(*failures),
        _ => None,
    }
}

// A new request is realised under the identity the provider reports.
#[tokio::test]
async fn a_request_is_realised() {
    let mut world = World::open();
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a") == LifecycleState::Realised { identity: "id-a".into() }).await;
}

// A retryable failure is recorded and nothing more: the controller does not try again on its
// own. An input's commit is an event that could change the outcome, and the next try runs then.
#[tokio::test]
async fn a_retryable_failure_waits_for_an_input_to_change() {
    let mut world = World::open();
    world.actuator.script().realise.insert("a".into(), VecDeque::from([Err(failure("registry unreachable", true))]));
    world.start();
    world.request("a");
    world.until("a failed once", |world| failures(&world.state("a")) == Some(1)).await;
    // Another resource realised after a's failure: the controller has passed it, and a was
    // not tried again.
    world.request("b");
    world.until("b realised", |world| world.state("b").realised()).await;
    assert_eq!(world.actuator.script().realised.iter().filter(|key| *key == "a").count(), 1, "a is tried once");

    world.clone_record("input-a");
    world.until("a realised after its input changed", |world| world.state("a").realised()).await;
    assert_eq!(world.actuator.script().realised.iter().filter(|key| *key == "a").count(), 2);
}

// A failure that no change can fix gives up at once.
#[tokio::test]
async fn a_final_failure_gives_up_at_once() {
    let mut world = World::open();
    world.actuator.script().realise.insert("a".into(), VecDeque::from([Err(failure("image refused", false))]));
    world.start();
    world.request("a");
    world.until("a gave up", |world| matches!(world.state("a"), LifecycleState::GaveUp { .. })).await;
    assert_eq!(world.actuator.script().realised, ["a"]);
}

// The failure limit bounds retries even when every event retries: a flapping input cannot loop.
#[tokio::test]
async fn the_failure_limit_bounds_retries() {
    let mut world = World::open();
    let failing = (0..10).map(|_| Err(failure("flapping", true))).collect();
    world.actuator.script().realise.insert("a".into(), failing);
    world.clone_record("input-a");
    world.start();
    world.request("a");
    world.until("a tried", |world| failures(&world.state("a")) == Some(1)).await;
    let gave_up = |world: &World| matches!(world.state("a"), LifecycleState::GaveUp { .. });
    for _ in 0..10 {
        if gave_up(&world) {
            break;
        }
        // Each input change is one more chance, until the policy's limit.
        let tried = world.actuator.script().realised.len();
        world.clone_step("input-a", Step::Attempted(flotilla_state_machines::Outcome::Succeeded));
        world.until("a tried again or gave up", |world| world.actuator.script().realised.len() > tried || gave_up(world)).await;
        world
            .until("the try recorded", |world| {
                failures(&world.state("a")) == Some(world.actuator.script().realised.len() as u32) || gave_up(world)
            })
            .await;
    }
    assert!(gave_up(&world), "the limit stops the retries");
    let limit = <EnvironmentKind as flotilla_state_machines::machines::lifecycle::LifecycleKind>::policy().max_failures;
    assert_eq!(world.actuator.script().realised.len() as u32, limit);
}

// While the host's facts report the provider unusable, nothing is tried; the fact turning
// usable is the event that runs the request.
#[tokio::test]
async fn an_unusable_provider_waits_for_its_fact() {
    let mut world = World::open();
    world.actuator.script().with_fact.insert("a".into());
    world.clone_record("fact-a");
    world.start();
    world.request("a");
    world.request("b");
    world.until("b realised", |world| world.state("b").realised()).await;
    assert!(world.actuator.script().realised.iter().all(|key| key != "a"), "a waits while its provider is unusable");
    world.clone_step("fact-a", Step::Realised { identity: "up".into() });
    world.until("a realised once its provider is usable", |world| world.state("a").realised()).await;
}

// A holder's ending releases what it held, with the identity it was realised under.
#[tokio::test]
async fn a_holder_ending_releases_the_resource() {
    let mut world = World::open();
    world.clone_record("holder-a");
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    world.clone_step("holder-a", Step::Released);
    world.until("a released", |world| world.state("a").gone()).await;
    assert_eq!(world.actuator.script().released, [("a".to_owned(), Some("id-a".to_owned()))]);
}

// A holder's ending waits for the records the resource still serves: the release runs when the
// last of them ends, and not before.
#[tokio::test]
async fn a_release_waits_for_what_the_resource_serves() {
    let mut world = World::open();
    world.clone_record("holder-a");
    world.clone_record("served-a");
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    world.clone_step("holder-a", Step::Released);
    // Another resource realised after the holder ended: the controller has passed a.
    world.request("b");
    world.until("b realised", |world| world.state("b").realised()).await;
    assert!(world.state("a").realised(), "a is held while it serves a live record");
    assert!(world.actuator.script().released.is_empty());
    world.clone_step("served-a", Step::Realised { identity: "served".into() });
    world.clone_step("served-a", Step::Released);
    world.until("a released once what it serves ended", |world| world.state("a").gone()).await;
    assert_eq!(world.actuator.script().released, [("a".to_owned(), Some("id-a".to_owned()))]);
}

// At start each live resource is observed: a backing already there is adopted, and a realised
// resource whose backing is gone is lost.
#[tokio::test]
async fn start_adopts_and_loses_backings() {
    let mut world = World::open();
    world.request("adopted");
    world.request("lost");
    world.environments.step("lost", &Step::Realised { identity: "old".into() }, World::now()).expect("realised before");
    world.actuator.script().present.insert("adopted".into(), "found".into());
    world.start();
    world.until("adopted", |world| world.state("adopted") == LifecycleState::Realised { identity: "found".into() }).await;
    world.until("lost", |world| world.state("lost").gone()).await;
    assert!(world.actuator.script().realised.is_empty(), "nothing is made at start");
}

// A pushed backing fact is recorded: a realised resource whose backing died is lost.
#[tokio::test]
async fn a_pushed_absence_loses_the_resource() {
    let mut world = World::open();
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    world
        .backing
        .as_ref()
        .expect("started")
        .send(BackingEvent { name: "a".into(), observed: Backing::Absent { reason: None } })
        .await
        .expect("send");
    world.until("a lost", |world| world.state("a").gone()).await;
}

// A pushed broken backing ends the resource as failed, with the reason the provider gave.
#[tokio::test]
async fn a_pushed_failure_ends_the_resource_as_failed() {
    let mut world = World::open();
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    let observed = Backing::Failed { reason: "adoption failed".into() };
    world.backing.as_ref().expect("started").send(BackingEvent { name: "a".into(), observed }).await.expect("send");
    world.until("a failed", |world| world.state("a").gone()).await;
    let LifecycleState::Gone { ending } = world.state("a") else { unreachable!() };
    assert_eq!(ending, flotilla_state_machines::machines::lifecycle::Ending::Failed { reason: "adoption failed".into() });
}

// A successor launch's request ends its predecessor, written by the controller.
#[tokio::test]
async fn a_successor_launch_ends_its_predecessor() {
    let mut world = World::open();
    world.start();
    world.request(&launch("s", 1));
    world.until("first launch realised", |world| world.state(&launch("s", 1)).realised()).await;
    world.environments.request(&launch("s", 2), environment("s2"), World::now()).expect("successor");
    world.until("predecessor gone", |world| world.state(&launch("s", 1)).gone()).await;
    world.until("successor realised", |world| world.state(&launch("s", 2)).realised()).await;
}

/// The store's log, refusing every GiveUp: a failure whose giving up was never written, as when
/// the write failed or the daemon stopped between the two.
struct RefusingGiveUp(StoreLog);

impl flotilla_state_machines::Log<Lifecycle<EnvironmentKind>> for RefusingGiveUp {
    fn create(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: &Resource<Lifecycle<EnvironmentKind>>,
        reads: &[flotilla_state_machines::InputRead],
    ) -> Result<flotilla_state_machines::Version, flotilla_state_machines::LogError> {
        self.0.create(writer, at, resource, reads)
    }

    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<Lifecycle<EnvironmentKind>>,
        reads: &[flotilla_state_machines::InputRead],
    ) -> Result<Resource<Lifecycle<EnvironmentKind>>, flotilla_state_machines::LogError> {
        self.0.create_resource(writer, at, resource, reads)
    }

    fn edge(
        &mut self,
        writer: &str,
        previous: &flotilla_state_machines::StoredState<Lifecycle<EnvironmentKind>>,
        next: &Resource<Lifecycle<EnvironmentKind>>,
        event: &flotilla_state_machines::EdgeEventOf<Lifecycle<EnvironmentKind>>,
    ) -> Result<flotilla_state_machines::Version, flotilla_state_machines::LogError> {
        if matches!(next.state(), LifecycleState::GaveUp { .. }) {
            return Err(flotilla_state_machines::LogError::new("the giving up was not written"));
        }
        self.0.edge(writer, previous, next, event)
    }
}

// A final failure whose giving up was not written is given up when the controller next starts,
// without the provider being tried again (#3108; the engine half of the old split-write test).
#[tokio::test]
async fn a_failure_whose_giving_up_was_not_written_gives_up_at_start_without_trying_again() {
    let mut world = World::open();
    {
        let store = StoreLog::new(world.service.store());
        let refusing = LifecycleHome::new(HOME, open_home_with(&store, RefusingGiveUp(store.clone())).expect("refusing home"));
        refusing.request("a", environment("a"), World::now()).expect("request");
        let failed = Step::Attempted(flotilla_state_machines::Outcome::Failed {
            cause: Cause::new(CauseCategory::Refused, "image refused"),
            retryable: false,
        });
        assert!(refusing.step("a", &failed, World::now()).is_err(), "the giving up is refused");
    }
    // The daemon restarts: its home is read back from the store, the failure recorded and not
    // given up.
    world.environments = Arc::new(LifecycleHome::new(HOME, open_home(StoreLog::new(world.service.store())).expect("environments")));
    assert_eq!(failures(&world.state("a")), Some(1));
    world.start();
    world
        .until("a gave up", |world| matches!(world.state("a"), LifecycleState::GaveUp { ref cause } if cause.detail == "image refused"))
        .await;
    assert!(world.actuator.script().realised.is_empty(), "the provider is not tried again");
}
