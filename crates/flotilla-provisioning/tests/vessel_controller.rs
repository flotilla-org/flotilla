//! The Vessel controller (#3108): the shared engine controller over the vessel planner's holdings.
//! A planner scripted per launch stands in for the crew's, whose own tests cover its plans; the
//! environments it holds are real records on the same store, whose commits wake the vessel.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::{
    engine::{controller::Controller, vessel_actuator::VesselActuator},
    stored::decided_registry,
};
use flotilla_provisioning_testkit::{EnvironmentFixture, VesselsFixture};
use flotilla_resources::{
    environment_records::{EnvironmentRecords, LaunchFacts},
    vessel_planning::{Held, HeldKind, VesselPlan, VesselPlanner},
    vessel_records::{VesselLaunchRequest, VesselRecords},
    EnvironmentSpec, HostDirectEnvironmentSpec, SystemClock,
};
use flotilla_schema_sqlite::stream::{CommitListener, Service};
use tokio::sync::Notify;

const HOME: &str = "alpha";

/// Each launch's plan as the test sets it, and how many times each was planned.
#[derive(Default)]
struct ScriptedPlanner {
    plans: Mutex<BTreeMap<String, VesselPlan>>,
    planned: Mutex<BTreeMap<String, usize>>,
}

impl ScriptedPlanner {
    fn set(&self, launch: &str, plan: VesselPlan) {
        self.plans.lock().expect("plans").insert(launch.to_owned(), plan);
    }

    fn planned(&self, launch: &str) -> usize {
        self.planned.lock().expect("planned").get(launch).copied().unwrap_or_default()
    }
}

#[async_trait]
impl VesselPlanner for ScriptedPlanner {
    async fn plan(&self, launch: &str) -> Result<VesselPlan, String> {
        *self.planned.lock().expect("planned").entry(launch.to_owned()).or_default() += 1;
        self.plans.lock().expect("plans").get(launch).cloned().ok_or_else(|| format!("no plan for {launch}"))
    }
}

struct World {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    planner: Arc<ScriptedPlanner>,
    vessels: VesselsFixture,
    environments: EnvironmentFixture,
    rechecks: Option<tokio::sync::mpsc::Sender<String>>,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
}

impl World {
    /// A decided store, its Vessel controller started.
    fn start() -> Self {
        let mut world = Self::new();
        world.run();
        world
    }

    /// A decided store whose Vessel controller [`Self::run`] starts.
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service =
            Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, decided_registry().expect("registry")).expect("store"));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let vessels = VesselsFixture::over_service(&service);
        let environments = EnvironmentFixture::over(HOME, service.store());
        let planner = Arc::new(ScriptedPlanner::default());
        Self { _directory: directory, service, planner, vessels, environments, rechecks: None, commits, _listener: listener }
    }

    fn run(&mut self) {
        let actuator = Arc::new(VesselActuator::new(Arc::clone(&self.planner) as Arc<dyn VesselPlanner>));
        let controller =
            Controller::start(Arc::clone(&self.vessels.lifecycles), actuator, Arc::new(SystemClock), Arc::clone(&self.service))
                .expect("controller");
        let (backing, backings) = tokio::sync::mpsc::channel(1);
        let (rechecks, recheck_events) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            // No backing is pushed for a vessel: its holdings' commits and rechecks look at it.
            let _backing = backing;
            controller.run_with(backings, recheck_events).await.expect("the controller runs")
        });
        self.rechecks = Some(rechecks);
    }

    fn reading(&self, launch: &str) -> LifecycleReading {
        self.vessels.vessels.get(launch, None).expect("read").expect("launch").reading
    }

    /// Waits for `what` to hold, looking again at each commit.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let committed = self.commits.notified();
                if holds(self) {
                    return;
                }
                let _ = tokio::time::timeout(Duration::from_millis(50), committed).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} within 5s"));
    }

    /// Environment `name`, requested here, as a holding of its newest launch.
    fn environment(&self, name: &str, ready: bool) -> Held {
        let spec = EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/repos".into() }),
            ..EnvironmentSpec::default()
        };
        if self.environments.environments.find(name).expect("read").is_none() {
            self.environments.request(name, &spec, None);
        }
        let record = self.environments.environments.find(name).expect("read").expect("requested");
        Held { kind: HeldKind::Environment, launch: record.name, home: record.home, ready }
    }
}

fn waiting(held: Vec<Held>) -> VesselPlan {
    VesselPlan::Waiting { reason: "its environment".into(), held, realised_in: None }
}

// Behaviour: a vessel waits for what it holds without spending a try, and is realised in its
// environment once its holding's commit says it is realised.
#[tokio::test]
async fn a_vessel_is_realised_once_its_holdings_are_and_waits_without_tries() {
    let world = World::start();
    let held = world.environment("env-w", false);
    let launch = world.vessels.request("convoy", "work", HOME);
    world.planner.set(&launch, waiting(vec![held.clone()]));
    world.until("planned", |world| world.planner.planned(&launch) > 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(world.reading(&launch), LifecycleReading::InProgress, "waiting is not a try");

    world.planner.set(&launch, VesselPlan::Ready { environment: "env-w".into(), held: vec![Held { ready: true, ..held }] });
    world.environments.ready("env-w", "/repos", LaunchFacts::default());
    world.until("realised", |world| matches!(world.reading(&launch), LifecycleReading::Ready { .. })).await;
    assert_eq!(world.reading(&launch), LifecycleReading::Ready { identity: "env-w".into() });
}

// Behaviour: a plan that cannot be made gives the vessel up at once, not retried.
#[tokio::test]
async fn a_planning_error_gives_the_vessel_up() {
    let world = World::start();
    let launch = world.vessels.request("convoy", "work", HOME);
    world.planner.set(&launch, VesselPlan::Failed { cause: "vessel work missing from convoy snapshot".into(), retryable: false });
    world.until("given up", |world| matches!(world.reading(&launch), LifecycleReading::GaveUp { .. })).await;
    let LifecycleReading::GaveUp { cause } = world.reading(&launch) else { unreachable!() };
    assert!(cause.contains("missing from convoy snapshot"), "{cause}");
}

// Behaviour: an input outside the engine (its convoy's snapshot) arriving is told by a recheck,
// which plans the vessel again.
#[tokio::test]
async fn a_recheck_plans_a_waiting_vessel_again() {
    let world = World::start();
    let held = world.environment("env-r", true);
    world.environments.ready("env-r", "/repos", LaunchFacts::default());
    let launch = world.vessels.request("convoy", "work", HOME);
    world
        .planner
        .set(&launch, VesselPlan::Waiting { reason: "its convoy's workflow snapshot".into(), held: Vec::new(), realised_in: None });
    world.until("planned", |world| world.planner.planned(&launch) > 0).await;

    world.planner.set(&launch, VesselPlan::Ready { environment: "env-r".into(), held: vec![held] });
    world.rechecks.as_ref().expect("running").send(launch.clone()).await.expect("recheck");
    world.until("realised", |world| matches!(world.reading(&launch), LifecycleReading::Ready { .. })).await;
}

// Behaviour: a realised vessel whose environment is lost ends lost, its holding's commit waking it.
#[tokio::test]
async fn a_lost_environment_ends_its_realised_vessel() {
    let world = World::start();
    let held = world.environment("env-l", true);
    world.environments.ready("env-l", "/repos", LaunchFacts::default());
    let launch = world.vessels.vessels.request(&request()).expect("requested");
    world.planner.set(&launch, VesselPlan::Ready { environment: "env-l".into(), held: vec![held] });
    world.rechecks.as_ref().expect("running").send(launch.clone()).await.expect("recheck");
    world.until("realised", |world| matches!(world.reading(&launch), LifecycleReading::Ready { .. })).await;

    world.planner.set(&launch, VesselPlan::Lost { reason: "environment env-l was lost".into() });
    world.environments.lose("env-l", "cgroup_oom");
    world.until("lost", |world| matches!(world.reading(&launch), LifecycleReading::Lost { .. })).await;
}

// Behaviour: a vessel realised before its controller started, as across a restart, is adopted
// and follows what it holds: its environment's loss still ends it.
#[tokio::test]
async fn a_vessel_adopted_at_start_follows_its_holdings() {
    let mut world = World::new();
    let held = world.environment("env-a", true);
    world.environments.ready("env-a", "/repos", LaunchFacts::default());
    let launch = world.vessels.request("convoy", "work", HOME);
    world.vessels.ready(&launch, "env-a");
    world.planner.set(&launch, VesselPlan::Ready { environment: "env-a".into(), held: vec![held] });
    world.run();
    world.until("holdings read", |world| world.planner.planned(&launch) >= 2).await;

    world.planner.set(&launch, VesselPlan::Lost { reason: "environment env-a was lost".into() });
    world.environments.lose("env-a", "cgroup_oom");
    world.until("lost", |world| matches!(world.reading(&launch), LifecycleReading::Lost { .. })).await;
}

fn request() -> VesselLaunchRequest {
    VesselLaunchRequest { convoy: "convoy".into(), convoy_home: HOME.into(), work: "work".into(), host: HOME.into() }
}

// Behaviour (#3108): a home's vessel launches reach a peer by following its log, read there at
// their home, as the sessions they hold are.
#[test]
fn a_homes_vessel_launches_reach_a_peer_by_its_log() {
    use flotilla_provisioning_testkit::{replicate_home, EngineFixture, OtherHome};
    use flotilla_resources::vessel_records::VesselRecords;

    let (alpha, beta) = (OtherHome::open("alpha"), OtherHome::open("beta"));
    let at_beta = EngineFixture::over_service(beta.service());
    let launch = at_beta.vessels.request("c", "work", "host");
    replicate_home(beta.service(), "beta", alpha.service());
    let at_alpha = EngineFixture::over_service(alpha.service());
    let read = at_alpha.vessels.vessels.get(&launch, Some("beta")).expect("read").expect("the launch, followed in");
    assert_eq!((read.home.as_str(), read.convoy.as_str()), ("beta", "c"));
    assert_eq!(VesselRecords::every(at_alpha.vessels.vessels.as_ref()).expect("every").len(), 1);
}
