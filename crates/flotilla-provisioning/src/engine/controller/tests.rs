//! The shared controller over a decided store, with a scripted actuator: each test drives it by
//! the events the design names, and reads the outcome from the engine's records.
//!
//! The scripted actuator keys each environment by its host-direct root. Its records live as
//! ImageBuild lifecycles at the same home, each the first launch of its name (`<name>#<n>`), read
//! and followed by that name: `input-<key>` is an input, `holder-<key>` ends the holder once Gone,
//! `fact-<key>` reports the provider usable while Realised, and a live `served-<key>` holds its
//! release back. An environment given holdings holds `hold-<key>-<i>` records, each requested only
//! once the one before it is realised, as a vessel's later holdings need its earlier ones' facts.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::{open_home, open_home_with, StoreLog};
use flotilla_resources::{Clock, SystemClock};
use flotilla_schema::{Frozen, Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{
    stream::{CommitListener, Service},
    Read, StoreError,
};
use flotilla_state_machines::{
    edge_event,
    machines::lifecycle::{Lifecycle, LifecycleEdgeKind, LifecycleState},
    Cause, CauseCategory, Edge, Resource,
};
use flotilla_store_testkit::VirtualClock;
use tokio::sync::{mpsc, Notify};

use super::{Actuator, Backing, BackingEvent, Controller, Holding, Holdings, ProviderFailure, Usability};
use crate::{
    engine::{environment_actuator::latest_state_of, LifecycleHome, Lifecycles, Step},
    stored::{
        environment::{EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe},
        image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation},
    },
};

const HOME: &str = "alpha";

fn record_code() -> KindCode {
    <Resource<Lifecycle<ImageBuildKind>> as Kind>::CODE
}

fn record(name: &str) -> Identity {
    Identity { kind: record_code(), name: name.to_owned(), home: HOME.to_owned() }
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

fn record_request() -> ImageBuildRequest {
    let layer = flotilla_resources::ImageLayerSpec::builder()
        .name("base".into())
        .version("1.0.0".into())
        .tier(flotilla_resources::ImageLayerTier::Base)
        .source(flotilla_resources::ImageLayerSource::Stdlib)
        .from("docker.io/library/ubuntu@sha256:00".into())
        .fragment("RUN true".into())
        .probe("true".into())
        .build();
    ImageBuildRequest {
        launch_of: "image-feed-arm64".into(),
        launch: 0,
        composition: "sha256:feed".into(),
        architecture: Architecture::Amd64,
        layers: Frozen::new(vec![layer]).expect("freeze"),
        delivery: Delivery::Local,
        reservation: Reservation { cpu: 2, disk_bytes: 1 << 30 },
    }
}

fn state_of(read: &mut Read<'_>, name: &str) -> Result<Option<LifecycleState>, StoreError> {
    latest_state_of::<ImageBuildKind>(read, name, HOME)
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
    /// How many holdings each key holds, staged one after another; none holds nothing.
    holdings: BTreeMap<String, usize>,
    /// Each key's planning failure: its holdings cannot be planned.
    unplannable: BTreeMap<String, ProviderFailure>,
    /// Where holdings are requested and read.
    records: Option<Arc<LifecycleHome<ImageBuildKind, StoreLog>>>,
    /// While set, a release tells the notify that it runs and waits for a permit.
    release_gate: Option<(Arc<Notify>, Arc<tokio::sync::Semaphore>)>,
    /// How many times the controller has read a holder, as it does for each resource it looks at.
    reads: usize,
    /// `reads` at the latest try at realising: a later read is the look the try's end leads to.
    reads_at_try: usize,
}

#[derive(Default)]
struct Scripted {
    script: Mutex<Script>,
    /// Told at each try at realising and each holder read, neither of which need write anything.
    progress: Notify,
}

impl Scripted {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().expect("script")
    }

    /// Whether the latest try has ended and been recorded: the controller has looked at a
    /// resource since.
    fn settled(&self) -> bool {
        let script = self.script();
        script.reads > script.reads_at_try
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
        let realised = {
            let mut script = self.script();
            script.realised.push(key.clone());
            script.reads_at_try = script.reads;
            script.realise.get_mut(&key).and_then(VecDeque::pop_front).unwrap_or_else(|| Ok(format!("id-{key}")))
        };
        self.progress.notify_waiters();
        realised
    }

    async fn release(&self, _: &str, request: &EnvironmentRequest, identity: Option<&str>) -> Result<(), ProviderFailure> {
        let gate = self.script().release_gate.clone();
        if let Some((releasing, permits)) = gate {
            releasing.notify_one();
            permits.acquire().await.expect("permits").forget();
        }
        self.script().released.push((key(request), identity.map(str::to_owned)));
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![record_code()]
    }

    fn dependencies(&self, request: &EnvironmentRequest) -> Vec<Identity> {
        let key = key(request);
        vec![record(&format!("input-{key}")), record(&format!("holder-{key}"))]
    }

    fn holder(&self, read: &mut Read<'_>, _: &str, request: &EnvironmentRequest) -> Result<Holding, StoreError> {
        self.script().reads += 1;
        self.progress.notify_waiters();
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

    fn holds(&self) -> bool {
        !self.script().holdings.is_empty() || !self.script().unplannable.is_empty()
    }

    async fn holdings(&self, _: &str, request: &EnvironmentRequest) -> Result<Holdings, ProviderFailure> {
        let key = key(request);
        // The script is not held across the store's writes: the controller's reads ask it for
        // usability while they hold the store.
        let (count, records) = {
            let script = self.script();
            if let Some(failure) = script.unplannable.get(&key) {
                return Err(failure.clone());
            }
            (script.holdings.get(&key).copied().unwrap_or_default(), Arc::clone(script.records.as_ref().expect("records")))
        };
        let mut held = Vec::new();
        for index in 0..count {
            let name = format!("hold-{key}-{index}");
            if records.newest_launch(&name).is_none() {
                records
                    .request(&name, ImageBuildRequest { launch_of: name.clone(), ..record_request() }, World::now())
                    .expect("request holding");
            }
            let usable = records.newest_launch(&name).is_some_and(|holding| holding.state().realised());
            held.push(Usability { fact: record(&name), usable });
            // The next holding is planned from this one's facts.
            if !usable {
                return Ok(Holdings { held, complete: index + 1 == count });
            }
        }
        Ok(Holdings { held, complete: true })
    }
}

/// A retryable or final failure.
fn failure(detail: &str, retryable: bool) -> ProviderFailure {
    ProviderFailure::new(Cause::new(CauseCategory::Transient, detail), retryable)
}

struct World {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    environments: Arc<LifecycleHome<EnvironmentKind, StoreLog>>,
    records: Arc<LifecycleHome<ImageBuildKind, StoreLog>>,
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
        let records = Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("records")));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let actuator = Arc::new(Scripted::default());
        actuator.script().records = Some(Arc::clone(&records));
        Self { _directory: directory, service, environments, records, actuator, commits, _listener: listener, backing: None }
    }

    /// Starts the controller over the records as they are now.
    fn start(&mut self) {
        self.start_with(Arc::new(SystemClock));
    }

    /// Starts the controller over the records as they are now, its time read from `clock`.
    fn start_with(&mut self, clock: Arc<dyn Clock>) {
        let controller = Controller::start(
            Arc::clone(&self.environments) as Arc<dyn Lifecycles<EnvironmentKind>>,
            Arc::clone(&self.actuator),
            clock,
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

    fn record(&self, name: &str) {
        self.records.request(name, ImageBuildRequest { launch_of: name.into(), ..record_request() }, Self::now()).expect("record");
    }

    fn record_step(&self, name: &str, step: Step) {
        let launch = self.records.newest_launch(name).expect("recorded").name().to_owned();
        self.records.step(&launch, &step, Self::now()).expect("record step");
    }

    /// The edges environment `name`'s log entries record, in order.
    fn edges(&self, name: &str) -> Vec<LifecycleEdgeKind> {
        type Environment = Resource<Lifecycle<EnvironmentKind>>;
        let store = self.service.store();
        let mut store = store.lock().expect("store");
        let mut read = store.read().expect("read");
        let id = read.resolve(<Environment as Kind>::CODE, name, HOME).expect("resolve").expect("stored");
        let (mut edges, mut after) = (Vec::new(), 0);
        loop {
            let entries = read.changes(id, after, 64).expect("changes");
            let Some(last) = entries.last() else { return edges };
            after = last.home_seq;
            for entry in &entries {
                let at = read.change(HOME, entry.change).expect("read").expect("its change").at;
                if let Some(event) = edge_event::<Lifecycle<EnvironmentKind>>(entry, at).expect("entry decodes") {
                    edges.push(event.arguments.kind());
                }
            }
        }
    }

    fn state(&self, name: &str) -> LifecycleState {
        self.environments.get(name).expect("held").state().clone()
    }

    /// Waits for `holds` to hold of the environments, rechecked at each commit, each try and
    /// each look the controller takes.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                let progressed = self.actuator.progress.notified();
                tokio::pin!(committed, progressed);
                committed.as_mut().enable();
                progressed.as_mut().enable();
                if holds(self) {
                    return;
                }
                tokio::select! {
                    () = committed => {}
                    () = progressed => {}
                }
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

// A resource that holds others requests them one by one, each once the one before it is
// realised, and is realised itself once all are. Waiting on them spends none of its tries, and
// it is tried once.
#[tokio::test]
async fn waiting_for_holdings_spends_no_tries() {
    let mut world = World::open();
    world.actuator.script().holdings.insert("a".into(), 3);
    world.start();
    world.request("a");
    for index in 0..3 {
        let holding = format!("hold-a-{index}");
        world.until("the next holding requested", |world| world.records.newest_launch(&holding).is_some()).await;
        assert_eq!(failures(&world.state("a")), Some(0), "waiting is not a try");
        assert!(world.actuator.script().realised.is_empty(), "not tried before its holdings are realised");
        world.record_step(&holding, Step::Realised { identity: holding.clone() });
    }
    world.until("a realised", |world| world.state("a").realised()).await;
    assert_eq!(world.actuator.script().realised, ["a"], "tried once, when everything it holds is realised");
}

// A holding kind whose holdings cannot be planned fails as its planning says: a planning error
// gives up at once, without a try at realising it.
#[tokio::test]
async fn a_planning_error_gives_up_without_a_try() {
    let mut world = World::open();
    world.actuator.script().unplannable.insert("a".into(), failure("work missing from its convoy", false));
    world.start();
    world.request("a");
    world.until("a gave up", |world| matches!(world.state("a"), LifecycleState::GaveUp { .. })).await;
    assert!(world.actuator.script().realised.is_empty());
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

    world.record("input-a");
    world.until("a realised after its input changed", |world| world.state("a").realised()).await;
    assert_eq!(world.actuator.script().realised.iter().filter(|key| *key == "a").count(), 2);
}

// A holding kind whose holdings could not be planned, a retryable failure, is tried again when
// its input changes, though what it holds is realised already: the holdings read that the change
// starts judges the try, as the change itself would. A vessel whose convoy had not yet arrived,
// its environment ready, waited forever.
#[tokio::test]
async fn a_retryable_planning_failure_is_tried_again_at_an_input_change() {
    let mut world = World::open();
    world.actuator.script().holdings.insert("a".into(), 1);
    world.actuator.script().unplannable.insert("a".into(), failure("convoy not found", true));
    world.start();
    world.request("a");
    world.until("a failed once", |world| failures(&world.state("a")) == Some(1)).await;
    world.record("hold-a-0");
    world.record_step("hold-a-0", Step::Realised { identity: "hold-a-0".into() });
    world.actuator.script().unplannable.clear();
    world.record("input-a");
    world.until("a realised", |world| world.state("a").realised()).await;
    assert_eq!(world.actuator.script().realised, ["a"], "tried once, when everything it holds is realised");
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

// The failure limit counts the kind's retry schedule, not wakes. A burst of input changes before
// the pending retry is due tries again at each, so an arrival is used at once, but counts none
// and writes nothing while the cause stays; a changed cause is written once. A try once the
// retry is due counts, so a flapping input still reaches the limit, a backoff at a time.
#[tokio::test]
async fn early_tries_count_none_and_the_limit_counts_due_ones() {
    let clock = Arc::new(VirtualClock::new(chrono::Utc::now()));
    let mut world = World::open();
    let failing = (0..20)
        .map(|_| Err(failure("flapping", true)))
        .chain([Err(failure("flapping harder", true))])
        .chain((0..10).map(|_| Err(failure("flapping", true))))
        .collect();
    world.actuator.script().realise.insert("a".into(), failing);
    world.record("input-a");
    world.start_with(Arc::clone(&clock) as Arc<dyn Clock>);
    world.request("a");
    world.until("a tried", |world| failures(&world.state("a")) == Some(1)).await;
    let tries = |world: &World| world.actuator.script().realised.len();
    let wake = |world: &World| world.record_step("input-a", Step::Attempted(flotilla_state_machines::Outcome::Succeeded));

    // A wake while a try runs is not one for that try, so each waits for the last to settle.
    for _ in 0..20 {
        let tried = tries(&world);
        wake(&world);
        world.until("a tried again and settled", |world| tries(world) > tried && world.actuator.settled()).await;
    }
    let changed = |world: &World| match world.state("a") {
        LifecycleState::Requested { last_failure: Some(last), .. } => last.cause.detail == "flapping harder",
        _ => false,
    };
    world.until("the changed cause recorded", changed).await;
    assert_eq!(tries(&world), 21);
    assert_eq!(failures(&world.state("a")), Some(1), "early tries count none");
    assert_eq!(world.edges("a"), [LifecycleEdgeKind::Attempted, LifecycleEdgeKind::FailedEarly], "only the changed cause is written");

    let limit = <EnvironmentKind as flotilla_state_machines::machines::lifecycle::LifecycleKind>::policy();
    for counted in 2..=limit.max_failures {
        clock.advance(chrono::Duration::microseconds(limit.cap));
        wake(&world);
        world
            .until("a due try counted", |world| {
                failures(&world.state("a")) == Some(counted) || matches!(world.state("a"), LifecycleState::GaveUp { .. })
            })
            .await;
    }
    world.until("a gave up", |world| matches!(world.state("a"), LifecycleState::GaveUp { .. })).await;
    assert_eq!(tries(&world) as u32, 21 + limit.max_failures - 1);
}

/// A retryable failure whose provider names when time alone could fix it.
fn failure_until(detail: &str, retry_at: Timestamp) -> ProviderFailure {
    ProviderFailure { retry_at: Some(retry_at), ..failure(detail, true) }
}

// A failure whose provider named a time is tried again then, with no event, but no sooner than
// the kind's backoff (a second after the first failure).
#[tokio::test]
async fn a_failure_with_a_retry_time_is_tried_again_then() {
    let mut world = World::open();
    let failed_at = std::time::Instant::now();
    world.actuator.script().realise.insert("a".into(), VecDeque::from([Err(failure_until("rate limited", World::now()))]));
    world.start();
    world.request("a");
    world.until("a realised at its retry time", |world| world.state("a").realised()).await;
    assert_eq!(world.actuator.script().realised.iter().filter(|key| *key == "a").count(), 2);
    assert!(failed_at.elapsed() >= Duration::from_millis(900), "tried again after {:?}, before the backoff", failed_at.elapsed());
}

// Retry times do not lift the failure limit: a provider that keeps naming one gives up there.
#[tokio::test]
async fn retry_times_stop_at_the_failure_limit() {
    let mut world = World::open();
    let failing = (0..10).map(|_| Err(failure_until("rate limited", World::now()))).collect();
    world.actuator.script().realise.insert("a".into(), failing);
    world.start();
    world.request("a");
    world.until("a gave up", |world| matches!(world.state("a"), LifecycleState::GaveUp { .. })).await;
    let limit = <EnvironmentKind as flotilla_state_machines::machines::lifecycle::LifecycleKind>::policy().max_failures;
    assert_eq!(world.actuator.script().realised.len() as u32, limit);
}

// While the host's facts report the provider unusable, nothing is tried; the fact turning
// usable is the event that runs the request.
#[tokio::test]
async fn an_unusable_provider_waits_for_its_fact() {
    let mut world = World::open();
    world.actuator.script().with_fact.insert("a".into());
    world.record("fact-a");
    world.start();
    world.request("a");
    world.request("b");
    world.until("b realised", |world| world.state("b").realised()).await;
    assert!(world.actuator.script().realised.iter().all(|key| key != "a"), "a waits while its provider is unusable");
    world.record_step("fact-a", Step::Realised { identity: "up".into() });
    world.until("a realised once its provider is usable", |world| world.state("a").realised()).await;
}

// A holder's ending releases what it held, with the identity it was realised under.
#[tokio::test]
async fn a_holder_ending_releases_the_resource() {
    let mut world = World::open();
    world.record("holder-a");
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    world.record_step("holder-a", Step::Released);
    world.until("a released", |world| world.state("a").gone()).await;
    assert_eq!(world.actuator.script().released, [("a".to_owned(), Some("id-a".to_owned()))]);
}

// A holder's ending waits for the records the resource still serves: the release runs when the
// last of them ends, and not before.
#[tokio::test]
async fn a_release_waits_for_what_the_resource_serves() {
    let mut world = World::open();
    world.record("holder-a");
    world.record("served-a");
    world.start();
    world.request("a");
    world.until("a realised", |world| world.state("a").realised()).await;
    world.record_step("holder-a", Step::Released);
    // Another resource realised after the holder ended: the controller has passed a.
    world.request("b");
    world.until("b realised", |world| world.state("b").realised()).await;
    assert!(world.state("a").realised(), "a is held while it serves a live record");
    assert!(world.actuator.script().released.is_empty());
    world.record_step("served-a", Step::Realised { identity: "served".into() });
    world.record_step("served-a", Step::Released);
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

// A backing found absent while the controller releases it is the release's own doing: the
// resource ends released, so what releasing cleans up is cleaned up, not lost (load_driver_smoke's
// sessions left listed under an ended convoy: the probe saw the release's kill first).
#[tokio::test]
async fn an_absence_pushed_while_releasing_ends_the_resource_released() {
    let mut world = World::open();
    let (releasing, permits) = (Arc::new(Notify::new()), Arc::new(tokio::sync::Semaphore::new(0)));
    world.actuator.script().release_gate = Some((Arc::clone(&releasing), Arc::clone(&permits)));
    world.record("holder-a");
    world.start();
    world.request("a");
    world.request("b");
    world.until("a and b realised", |world| world.state("a").realised() && world.state("b").realised()).await;

    let reached = releasing.notified();
    world.record_step("holder-a", Step::Released);
    reached.await;
    let backing = world.backing.as_ref().expect("started");
    backing
        .send(BackingEvent { name: "a".into(), observed: Backing::Absent { reason: Some("process exited".into()) } })
        .await
        .expect("send");
    // The controller takes pushed facts in order: once b is lost, a's absence has been read.
    backing.send(BackingEvent { name: "b".into(), observed: Backing::Absent { reason: None } }).await.expect("send");
    world.until("b lost", |world| world.state("b").gone()).await;
    permits.add_permits(1);

    world.until("a ended", |world| world.state("a").gone()).await;
    let LifecycleState::Gone { ending } = world.state("a") else { unreachable!() };
    assert_eq!(ending, flotilla_state_machines::machines::lifecycle::Ending::Released);
    assert_eq!(world.actuator.script().released, [("a".to_owned(), Some("id-a".to_owned()))]);
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
