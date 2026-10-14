//! The dummy fleet (#3206) drives convoys end to end through the real daemon
//! runtime: admission, placement, the environment, checkout, terminal-session,
//! message and convoy controllers, and change-request observation. Only the
//! providers at the seams are dummies.

use std::{sync::Arc, time::Duration};

use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    runtime::{DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_PLACEMENT_POLICY},
    DummyFleet, DummyFleetSnapshot,
};
use flotilla_protocol::{
    commands::{CommandValue, CrewSupervisionAction},
    Command, CommandAction, DaemonEvent, HostName, PrincipalRef,
};
use flotilla_resources::{
    Convoy, ConvoyPhase, CrewWorkPhase, Forge, Host, InputMeta, PlacementPolicy, Repository, ResourceObject, WorkflowTemplate,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const LONG_RUNNER_TURNS: u32 = 3;

struct DummyDaemon {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    _runtime: DaemonRuntime,
    _dir: tempfile::TempDir,
}

impl DummyDaemon {
    /// A daemon whose `daemon.toml` enables the dummy fleet with these agent
    /// and environment settings, started through the composition-root switch.
    async fn start(agent: &str, environment: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let daemon_toml = format!(
            "machine_id = \"dummy-load\"\n\n[dummy_fleet]\n[dummy_fleet.agent]\nlong_runner_turns = {LONG_RUNNER_TURNS}\n{agent}\n[dummy_fleet.environment]\n{environment}"
        );
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml).expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery(false);
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon =
            InProcessDaemon::new_with_resource_backend(vec![], Arc::clone(&config), discovery, HostName::new("dummy-load"), backend).await;
        attach_dummy_fleet(&fleet, Arc::clone(&daemon) as Arc<dyn DaemonHandle>).expect("attach fleet");
        let options = RuntimeOptions {
            namespace: NAMESPACE.into(),
            heartbeat_interval: Duration::from_secs(5),
            controller_resync_interval: Duration::from_millis(200),
            ..RuntimeOptions::default()
        };
        let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
        let started = Self { daemon, fleet, _runtime: runtime, _dir: dir };
        started.seed().await;
        started
    }

    fn backend(&self) -> ResourceBackend {
        self.daemon.resource_backend()
    }

    async fn seed(&self) {
        let backend = self.backend();
        let host = backend.clone().using::<Host>(NAMESPACE).list().await.expect("hosts").items.into_iter().next().expect("local host");
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        backend
            .clone()
            .using::<Forge>(NAMESPACE)
            .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
            .await
            .expect("dummy forge");
        // A dispatched repository is tracked first; admission reads its resolved default branch.
        let repository = catalog::repository(&forge).expect("dummy repository");
        let repositories = backend.clone().using::<Repository>(NAMESPACE);
        repositories.create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository).await.expect("repository");
        backend
            .clone()
            .using::<PlacementPolicy>(NAMESPACE)
            .create(
                &InputMeta::builder().name(DUMMY_PLACEMENT_POLICY.to_string()).labels(catalog::placement_policy_labels()).build(),
                &catalog::placement_policy(&host.metadata.name),
            )
            .await
            .expect("dummy placement policy");
        for mix in CrewMix::ALL {
            backend
                .clone()
                .using::<WorkflowTemplate>(NAMESPACE)
                .create(&InputMeta::builder().name(mix.workflow_name().to_string()).build(), &mix.workflow(LONG_RUNNER_TURNS))
                .await
                .expect("dummy workflow");
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while repositories
                .get(&repository.key().to_string())
                .await
                .ok()
                .and_then(|repository| repository.status?.default_branch)
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the dummy forge resolves the default branch");
    }

    async fn execute(&self, action: CommandAction) -> CommandValue {
        self.execute_as(action, None).await
    }

    /// Execute as an operator, as a supervisor's socket client does.
    async fn execute_as_operator(&self, action: CommandAction) -> CommandValue {
        self.execute_as(action, Some(PrincipalRef::implicit_for_namespace(NAMESPACE))).await
    }

    async fn execute_as(&self, action: CommandAction, principal: Option<PrincipalRef>) -> CommandValue {
        let mut events = self.daemon.subscribe();
        let id = self.daemon.execute_for_principal(Command::builder().action(action).build(), principal).await.expect("execute");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(DaemonEvent::CommandFinished { command_id, result, .. }) = events.recv().await {
                    if command_id == id {
                        return result;
                    }
                }
            }
        })
        .await
        .expect("command result")
    }

    async fn dispatch(&self, name: &str, mix: CrewMix) {
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        let created = self
            .execute(CommandAction::ConvoyCreate {
                name: name.into(),
                workflow_ref: mix.workflow_name().into(),
                inputs: Vec::new(),
                repository_url: Some(catalog::repository_url(&forge)),
                r#ref: Some("main".into()),
                project_ref: None,
                placement_policy: Some(DUMMY_PLACEMENT_POLICY.into()),
                adopted_checkout: None,
            })
            .await;
        assert_eq!(created, CommandValue::ConvoyCreated { name: name.into() });
    }

    async fn convoy(&self, name: &str) -> Option<ResourceObject<Convoy>> {
        let convoys = self.backend().using::<Convoy>(NAMESPACE).list().await.expect("convoys");
        convoys.items.into_iter().find(|convoy| convoy.metadata.labels.values().any(|value| value == name))
    }

    /// Wait until `done` holds for the convoy, reporting where it stopped.
    async fn wait_for(&self, name: &str, what: &str, done: impl Fn(&ResourceObject<Convoy>) -> bool) -> ResourceObject<Convoy> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let convoy = self.convoy(name).await;
            match convoy {
                Some(convoy) if done(&convoy) => return convoy,
                convoy if tokio::time::Instant::now() > deadline => {
                    let status = convoy.and_then(|convoy| convoy.status);
                    panic!(
                        "convoy {name} never reached {what}: phase {:?}, message {:?}, crew {:?}, fleet {:?}",
                        status.as_ref().map(|status| status.phase),
                        status.as_ref().and_then(|status| status.message.clone()),
                        status.as_ref().map(|status| &status.crew_work),
                        self.fleet.snapshot()
                    );
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    async fn wait_for_phase(&self, name: &str, phase: ConvoyPhase) -> ResourceObject<Convoy> {
        self.wait_for(name, &format!("{phase:?}"), |convoy| convoy.status.as_ref().is_some_and(|status| status.phase == phase)).await
    }

    async fn wait_for_crew(&self, name: &str, phase: CrewWorkPhase) -> ResourceObject<Convoy> {
        self.wait_for(name, &format!("coder {phase:?}"), |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.crew_work.get("implement").and_then(|roles| roles.get("coder")).is_some_and(|work| work.phase == phase)
            })
        })
        .await
    }

    /// A crew stall a supervisor can act on: the crew's work and the convoy's
    /// stall condition, which the leaf engine raises after the crew stalls.
    async fn wait_for_stall(&self, name: &str) -> ResourceObject<Convoy> {
        self.wait_for(name, "a supervisable stall", |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.stalled.is_some()
                    && status
                        .crew_work
                        .get("implement")
                        .and_then(|roles| roles.get("coder"))
                        .is_some_and(|work| work.phase == CrewWorkPhase::Stalled)
            })
        })
        .await
    }

    async fn deliver(&self, name: &str, mix: CrewMix) -> DummyFleetSnapshot {
        self.dispatch(name, mix).await;
        self.wait_for_phase(name, ConvoyPhase::Landed).await;
        self.fleet.snapshot()
    }
}

// Scenarios use the default agent behaviour; failure and stall rates are added per test.
const SCENARIO_AGENT: &str = "";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lone_coder_keeps_its_change_request_promise_and_lands() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("lone-coder", CrewMix::Coder).await;
    assert_eq!((snapshot.crews_completed, snapshot.change_requests_opened, snapshot.crews_failed), (1, 1, 0));
    let convoy = fleet.convoy("lone-coder").await.expect("convoy");
    let promises = convoy.status.expect("status").promises;
    let promise = promises.get("implement").and_then(|roles| roles.get("coder")).and_then(|promises| promises.first()).expect("pr promise");
    assert_eq!(promise.state, flotilla_resources::promises::PromiseState::Kept);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("reviewed-coder", CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_runner_exchanges_its_turns_before_landing() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("long-runner", CrewMix::LongRunner).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, u64::from(2 * LONG_RUNNER_TURNS), "every task and reply is a delivered turn: {snapshot:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_that_fails_to_realise_fails_its_convoy() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "failure_rate = 1.0\n").await;
    fleet.dispatch("unrealised", CrewMix::Coder).await;
    fleet.wait_for_phase("unrealised", ConvoyPhase::Failed).await;
    assert_eq!(fleet.fleet.snapshot().sessions_launched, 0);
}

fn supervise(convoy: &ResourceObject<Convoy>, operation: CrewSupervisionAction) -> CommandAction {
    CommandAction::CrewSupervise {
        namespace: Some(NAMESPACE.into()),
        convoy: convoy.metadata.name.clone(),
        vessel: "implement".into(),
        role: "coder".into(),
        operation,
        message: "Supervisor decision.".into(),
        actor_crew_id: None,
    }
}

// A crew cannot fail its own work; it stalls proposing failure, and a
// supervisor's decision fails the convoy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_crew_proposes_failure_and_its_supervisor_fails_the_convoy() {
    let fleet = DummyDaemon::start(&format!("{SCENARIO_AGENT}fail_rate = 1.0\n"), "").await;
    fleet.dispatch("failing", CrewMix::Coder).await;
    let convoy = fleet.wait_for_stall("failing").await;
    assert_eq!(fleet.fleet.snapshot().crews_failed, 1);
    let failed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Fail)).await;
    assert!(!matches!(failed, CommandValue::Error { .. }), "{failed:?}");
    fleet.wait_for_crew("failing", CrewWorkPhase::Failed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_crew_resumes_on_supervision_and_lands() {
    let fleet = DummyDaemon::start(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "").await;
    fleet.dispatch("stalled", CrewMix::Coder).await;
    let convoy = fleet.wait_for_stall("stalled").await;
    assert_eq!(fleet.fleet.snapshot().crews_stalled, 1);
    let resumed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Resume)).await;
    assert!(!matches!(resumed, CommandValue::Error { .. }), "{resumed:?}");
    fleet.wait_for_phase("stalled", ConvoyPhase::Landed).await;
}
