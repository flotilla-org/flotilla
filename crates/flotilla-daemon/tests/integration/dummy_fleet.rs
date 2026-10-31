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
use flotilla_discovery_testkit::fake_discovery_with_host_directories;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_FULFILMENT_KIND},
    DummyFleet, DummyFleetSnapshot, DUMMY_AGENT_ADAPTER,
};
use flotilla_protocol::{
    commands::{CommandValue, CrewSupervisionAction},
    Command, CommandAction, DaemonEvent, HostName, PrincipalRef,
};
use flotilla_resources::{Convoy, ConvoyPhase, CrewWorkPhase, Forge, Host, InputMeta, Repository, ResourceObject, WorkflowTemplate};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const LONG_RUNNER_TURNS: u32 = 3;

struct DummyDaemon {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    _runtime: DaemonRuntime,
    relay: Option<Arc<RestartRelay>>,
    _dir: tempfile::TempDir,
}

#[bon::bon]
impl DummyDaemon {
    /// A daemon whose `daemon.toml` enables the dummy fleet with these agent
    /// and environment settings, started through the composition-root switch.
    async fn start(agent: &str, environment: &str) -> Self {
        Self::start_with_resync(agent, environment, Duration::from_millis(200)).await
    }

    async fn start_with_resync(agent: &str, environment: &str, resync: Duration) -> Self {
        Self::start_switched(agent, environment, resync, false).await
    }

    /// A daemon with `[primitives]` on: Message runs on the engine (#3317).
    async fn start_on_primitives(agent: &str, environment: &str) -> Self {
        Self::start_switched(agent, environment, Duration::from_millis(200), true).await
    }

    async fn start_switched(agent: &str, environment: &str, resync: Duration, primitives: bool) -> Self {
        Self::start_with_relay().agent(agent).environment(environment).resync(resync).primitives(primitives).host("dummy-load").call().await
    }

    #[builder]
    async fn start_with_relay(
        #[builder(default)] agent: &str,
        #[builder(default)] environment: &str,
        #[builder(default = Duration::from_millis(200))] resync: Duration,
        #[builder(default = true)] primitives: bool,
        host: &str,
        #[builder(default)] restartable: bool,
        #[builder(default = true)] designated: bool,
        #[builder(default)] supervision: flotilla_daemon::supervisor::ControllerSupervision,
    ) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let switch = if primitives { "[primitives]\n" } else { "" };
        let daemon_toml = format!(
            "machine_id = \"{host}\"\n\n{switch}[dummy_fleet]\n[dummy_fleet.agent]\nlong_runner_turns = {LONG_RUNNER_TURNS}\n{agent}\n[dummy_fleet.environment]\n{environment}"
        );
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml).expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery_with_host_directories(dir.path());
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&config),
            discovery,
            HostName::new(host),
            backend,
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let relay = if restartable {
            let relay = Arc::new(RestartRelay {
                commands: std::sync::Mutex::new(None),
                ready: tokio::sync::Notify::new(),
                backend: daemon.resource_backend(),
            });
            relay.connect(&daemon);
            fleet.attach(Arc::clone(&relay) as Arc<dyn flotilla_dummy_fleet::CrewCommandPort>).expect("attach restart relay");
            Some(relay)
        } else {
            attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
            None
        };
        let options = RuntimeOptions {
            namespace: NAMESPACE.into(),
            heartbeat_interval: Duration::from_secs(5),
            controller_resync_interval: resync,
            controller_supervision: supervision,
            ..RuntimeOptions::default()
        };
        let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
        let started = Self { daemon, fleet, _runtime: runtime, relay, _dir: dir };
        started.seed(designated).await;
        started
    }

    async fn designate(&self) {
        self.backend()
            .definitions::<flotilla_resources::FleetDesignation>(NAMESPACE)
            .create(
                &InputMeta::builder().name(flotilla_resources::FLEET_DESIGNATION_NAME.to_string()).build(),
                &flotilla_resources::FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
            )
            .await
            .expect("fleet designation");
    }

    fn backend(&self) -> ResourceBackend {
        self.daemon.resource_backend()
    }

    async fn seed(&self, designated: bool) {
        let backend = self.backend();
        if self.daemon.message_primitives().is_some() {
            // Fleet identity and charter Project are declared independently.
            backend
                .clone()
                .using::<flotilla_resources::Project>(NAMESPACE)
                .create(
                    &InputMeta::builder().name("fleet".to_string()).build(),
                    &flotilla_resources::ProjectSpec::builder()
                        .display_name("Fleet".to_string())
                        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                        .repositories(Vec::new())
                        .build(),
                )
                .await
                .expect("fleet project");
            if designated {
                self.designate().await;
            }
        }
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
            .using::<flotilla_resources::FulfilmentKind>(NAMESPACE)
            .create(&InputMeta::builder().name(DUMMY_FULFILMENT_KIND.to_string()).build(), &catalog::fulfilment_kind(&host.metadata.name))
            .await
            .expect("dummy fulfilment kind");
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
        // Admission places a literal image only once its host has probed the image's harnesses.
        let hosts = backend.clone().using::<Host>(NAMESPACE);
        tokio::time::timeout(Duration::from_secs(20), async {
            while !hosts.get(&host.metadata.name).await.ok().and_then(|host| host.status).is_some_and(|status| {
                status.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).is_some_and(|facts| facts.harnesses.contains_key(DUMMY_AGENT_ADAPTER))
            }) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the dummy kind's harness is observed");
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
                placement_policy: Some(DUMMY_FULFILMENT_KIND.into()),
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
                        "convoy {name} never reached {what}: phase {:?}, message {:?}, crew {:?}, stall {:?}, supervision {:?}, fleet {:?}",
                        status.as_ref().map(|status| status.phase),
                        status.as_ref().and_then(|status| status.message.clone()),
                        status.as_ref().map(|status| &status.crew_work),
                        status.as_ref().and_then(|status| status.stalled.as_ref()),
                        status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|w| w.supervision.as_ref()),
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
    /// stall condition, both published by the crew and maintained by the leaf engine.
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
        let landed = self.wait_for_phase(name, ConvoyPhase::Landed).await;
        if self.daemon.message_primitives().is_some() {
            self.assert_engine_delivered(&landed.metadata.name);
        }
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
    let status = convoy.status.expect("status");
    let promises = &status.promises["implement"]["coder"];
    let promise = promises.iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).expect("pr promise");
    assert_eq!(promise.state, flotilla_resources::promises::PromiseState::Kept);
    assert_eq!(promise.id, "template/pr");
    assert_eq!(promise.source, flotilla_resources::promises::PromiseSource::Template);
    let done = &status.crew_work["implement"]["coder"];
    assert_eq!(done.completion_authority, flotilla_resources::WorkCompletionAuthority::Promises);
    assert!(done.finished_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("reviewed-coder", CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
    let status = fleet.convoy("reviewed-coder").await.unwrap().status.unwrap();
    let pr = status.promises["implement"]["coder"].iter().find(|p| p.kind == flotilla_resources::promises::PromiseKind::Pr).unwrap();
    assert_eq!(pr.id, "template/pr");
    assert_eq!(pr.state, flotilla_resources::promises::PromiseState::Kept);
    assert!(status.crew_work.values().flat_map(|crew| crew.values()).all(|work| work.phase == CrewWorkPhase::Done));
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
    // The crew publishes its stall before the script records its failure. Wait for
    // accounting before supervision can tear down the crew session.
    fleet.wait_for("failing", "failure accounting", |_| fleet.fleet.snapshot().crews_failed > 0).await;
    assert_eq!(fleet.fleet.snapshot().crews_failed, 1);
    let convoy = fleet.wait_for_stall("failing").await;
    let failed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Fail)).await;
    assert!(!matches!(failed, CommandValue::Error { .. }), "{failed:?}");
    fleet.wait_for_crew("failing", CrewWorkPhase::Failed).await;
    // A supervisor's failure decision ends the convoy, not just the crew.
    let ended = fleet.wait_for_phase("failing", ConvoyPhase::Failed).await;
    assert!(ended.status.as_ref().expect("status").stalled.is_none(), "terminal convoys have no stall");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_crew_resumes_on_supervision_and_lands() {
    let fleet = DummyDaemon::start(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "").await;
    fleet.dispatch("stalled", CrewMix::Coder).await;
    // The crew publishes its stall before recording accounting; wait for both
    // before supervision resumes the script.
    fleet.wait_for("stalled", "stall accounting", |_| fleet.fleet.snapshot().crews_stalled > 0).await;
    let convoy = fleet.wait_for_stall("stalled").await;
    assert_eq!(fleet.fleet.snapshot().crews_stalled, 1);
    let resumed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Resume)).await;
    assert!(!matches!(resumed, CommandValue::Error { .. }), "{resumed:?}");
    fleet.wait_for_phase("stalled", ConvoyPhase::Landed).await;
}

// #3439: force the handoff-after-Done ordering without racing the scripts.
// A zero-task long runner finishes first; its aide then hands back work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_long_runner_handles_a_late_handoff_and_lands() {
    late_handoff_scenario(false).await;
}

// A late handoff reopens the recipient and transfers the engine promises; the
// recipient delivers its engine Convoy after handling that additional turn.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_finished_long_runner_handles_a_late_handoff() {
    late_handoff_scenario(true).await;
}

async fn late_handoff_scenario(primitives: bool) {
    let fleet = DummyDaemon::start_switched(SCENARIO_AGENT, "", Duration::from_millis(200), primitives).await;
    let workflows = fleet.backend().using::<WorkflowTemplate>(NAMESPACE);
    let workflow = workflows.get(CrewMix::LongRunner.workflow_name()).await.expect("workflow");
    // Isolate the legacy same-vessel hand-back path. Normal peer tasks retain
    // their separate responsibilities on distinct vessels in the load catalog.
    let mut same_vessel = CrewMix::LongRunner.workflow(0);
    let mut aide = same_vessel.vessels.pop().unwrap();
    same_vessel.vessels[0].crew.append(&mut aide.crew);
    for crew in &mut same_vessel.vessels[0].crew {
        if let flotilla_resources::CrewSource::Agent { prompt: Some(prompt), .. } = &mut crew.source {
            *prompt = prompt.replace("assist:aide", "aide").replace("govern:governor", "governor");
        }
    }
    workflows
        .update(&InputMeta::builder().name(workflow.metadata.name.clone()).build(), &workflow.metadata.resource_version, &same_vessel)
        .await
        .expect("zero-task governor");
    fleet.dispatch("late-handoff", CrewMix::LongRunner).await;
    let convoy = fleet
        .wait_for("late-handoff", "finished governor and available aide", |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.crew_work.get("govern").is_some_and(|crew| {
                    crew.get("governor").is_some_and(|work| work.phase == CrewWorkPhase::Done)
                        && crew.get("aide").is_some_and(|work| work.phase == CrewWorkPhase::Pending)
                })
            })
        })
        .await;
    let session = fleet
        .backend()
        .using::<flotilla_resources::TerminalSession>(NAMESPACE)
        .list()
        .await
        .expect("sessions")
        .items
        .into_iter()
        .find(|session| session.spec.role == "governor")
        .expect("governor session");
    let flotilla_resources::TerminalSessionSource::Agent { context: session_context, .. } = session.spec.source else { panic!("agent") };
    let context = flotilla_protocol::query::CrewCommandContext {
        namespace: Some(NAMESPACE.into()),
        convoy: Some(convoy.metadata.name.clone()),
        vessel_ref: Some(session_context.vessel_ref),
        role: Some("aide".into()),
        ..Default::default()
    };
    let before = fleet.fleet.snapshot();
    let handed_off = fleet
        .execute(CommandAction::CrewHandoff {
            context: context.clone(),
            target: "governor".into(),
            message: "Done.".into(),
            carries: Vec::new(),
        })
        .await;
    assert_eq!(handed_off, CommandValue::Ok);
    // The artificial sender is HandedBack, a terminal phase excluded from
    // derived Done. Keep this legacy handoff fixture's explicit operator override.
    assert_eq!(
        fleet
            .execute_as_operator(CommandAction::CrewComplete {
                context,
                message: None,
                disposition: None,
                decision_ledger_ref: None,
                force: true,
            })
            .await,
        CommandValue::Ok
    );
    let landed = fleet.wait_for_phase("late-handoff", ConvoyPhase::Landed).await;
    if primitives {
        fleet.assert_engine_delivered(&landed.metadata.name);
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if fleet
                    .daemon
                    .message_primitives()
                    .expect("engine")
                    .messages()
                    .expect("messages")
                    .iter()
                    .any(|(_, message)| message.state().receiver().is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("engine handoff receipt");
        fleet.assert_on_the_engine().await;
    }
    assert_eq!(fleet.fleet.snapshot().turns_received, before.turns_received + 1, "the reopened governor receives the handoff turn");
    assert_eq!(
        landed.status.expect("status").crew_work["govern"]["governor"].completion_authority,
        flotilla_resources::WorkCompletionAuthority::Promises
    );
}

// The supervisor is eagerly launched with no claim, waits for a delivered
// review task, answers it across vessels, and reports so the convoy can land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_eager_supervisor_waits_for_work_and_lands_after_answering() {
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("supervised-coder", CrewMix::CoderSupervisor).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the task and answer are delivered: {snapshot:?}");
    assert_eq!(snapshot.crews_stalled, 0, "{snapshot:?}");
}

// A missed event must not be hidden by the scenario harness's 200ms resync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coder_lands_at_the_production_resync_interval() {
    let fleet = DummyDaemon::start_with_resync(SCENARIO_AGENT, "", RuntimeOptions::default().controller_resync_interval).await;
    fleet.dispatch("production-resync", CrewMix::Coder).await;
    fleet.wait_for_phase("production-resync", ConvoyPhase::Landed).await;
    assert_eq!(fleet.fleet.snapshot().command_errors, 0, "{:?}", fleet.fleet.command_error_samples());
}

// Behaviour (#3317): with `[primitives]` on, crew turns run end to end on the
// engine path: every review request and answer is an engine Message, typed by
// the delivery adapter, with its receipt on the session's AgentSession, and
// the old store holds no Message at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_reviewed_coder_lands_after_its_reviewer_answers() {
    let fleet = DummyDaemon::start_on_primitives(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("engine-reviewed", CrewMix::CoderReviewer).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, 2, "the review request and its answer are delivered turns: {snapshot:?}");
    fleet.assert_on_the_engine().await;
}

// Behaviour (#3317): a long runner's turns all travel the engine path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_long_runner_exchanges_its_turns_before_landing() {
    let fleet = DummyDaemon::start_on_primitives(SCENARIO_AGENT, "").await;
    let snapshot = fleet.deliver("engine-long-runner", CrewMix::LongRunner).await;
    assert_eq!(snapshot.crews_completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.turns_received, u64::from(2 * LONG_RUNNER_TURNS), "every task and reply is a delivered turn: {snapshot:?}");
    fleet.assert_on_the_engine().await;
}

// Behaviour: with no FleetDesignation the controllers keep running and health names the missing
// record; once it is recorded the waiting convoy delivers without a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_fleet_designation_waits_and_delivery_resumes_when_it_arrives() {
    let supervision = flotilla_daemon::supervisor::ControllerSupervision {
        max_consecutive_failures: 2,
        recovery_backoff: Duration::from_secs(3600),
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
        ..Default::default()
    };
    let fleet = DummyDaemon::start_with_relay().host("dummy-load").designated(false).supervision(supervision).call().await;
    fleet.dispatch("undesignated", CrewMix::Coder).await;
    let named = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let conditions = fleet.host_conditions().await;
            if let Some(condition) = conditions.iter().find(|c| c.condition_type == "Message/FleetDesignation") {
                return condition.message.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("health names the missing FleetDesignation");
    assert!(named.contains("FleetDesignation `fleet` is missing"), "{named}");
    let convoy = fleet.convoy("undesignated").await.expect("convoy");
    let explained = fleet.daemon.explain_convoy_internal(Some(NAMESPACE), &convoy.metadata.name).await.expect("explain");
    assert!(explained.warnings.iter().any(|warning| warning.contains("FleetDesignation `fleet` is missing")), "{:?}", explained.warnings);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let conditions = fleet.host_conditions().await;
    assert!(conditions.iter().all(|c| !c.condition_type.starts_with("Controller/")), "no controller exhausted its budget: {conditions:?}");
    fleet.designate().await;
    fleet.wait_for_phase("undesignated", ConvoyPhase::Landed).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while fleet.host_conditions().await.iter().any(|c| c.condition_type == "Message/FleetDesignation") {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("health clears the wait");
}

impl DummyDaemon {
    async fn host_conditions(&self) -> Vec<flotilla_resources::HostCondition> {
        let hosts = self.backend().using::<Host>(NAMESPACE).list().await.expect("hosts");
        hosts.items.into_iter().flat_map(|h| h.status.map(|s| s.conditions).unwrap_or_default()).collect()
    }

    /// Every Message is on the engine, delivered with a receipt on a real
    /// AgentSession; the old store holds none.
    async fn assert_on_the_engine(&self) {
        use flotilla_state_machines::machines::message::MessageState;
        let old = self.backend().using::<flotilla_resources::Message>(NAMESPACE).list().await.expect("old messages");
        assert!(old.items.is_empty(), "the old store holds Messages: {:?}", old.items);
        let primitives = self.daemon.message_primitives().expect("engine path");
        let messages = primitives.messages().expect("engine messages");
        assert!(!messages.is_empty(), "the turns were engine Messages");
        let delivered: Vec<_> = messages.iter().filter_map(|(_, message)| message.state().receiver().cloned()).collect();
        assert!(!delivered.is_empty(), "turns were delivered: {messages:?}");
        for receiver in &delivered {
            let session = primitives.sessions().get(&receiver.id).expect("each receiver is an AgentSession record");
            assert_eq!(receiver.host, primitives.home());
            let _ = session;
        }
        for (_, message) in &messages {
            assert!(!matches!(message.state(), MessageState::Dropped { .. }), "no turn was dropped: {message:?}");
            assert_eq!(message.name().parse::<flotilla_address::Address>().map(|a| a.kind()), Ok(flotilla_address::Kind::Message));
        }
        eprintln!(
            "ENGINE senders: {:?}",
            messages
                .iter()
                .map(|(_, m)| (m.spec().sender.to_string(), m.spec().inbox.to_string(), format!("{:?}", m.state())))
                .collect::<Vec<_>>()
        );
    }
}

// Behaviour (#3938): an operator sends as the operator role of the convoy's
// project. On the engine path a CLI message to a stalled crew and the
// operator's ruling are each admitted and delivered to the crew's session as
// an engine Message from that role, and the resumed engine Convoys deliver.
// Each goes to its own stalled crew: whichever turn reaches a stalled crew
// first resumes it, and a later one may find its session already ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_an_operators_message_and_ruling_reach_the_crew_from_the_projects_operator() {
    use flotilla_state_machines::machines::convoy::ConvoyState;

    let fleet = DummyDaemon::start_on_primitives(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "").await;
    let primitives = fleet.daemon.message_primitives().expect("engine path");
    for (role, noted) in [("engine-noted", true), ("engine-ruled", false)] {
        fleet.dispatch(role, CrewMix::Coder).await;
        let convoy = fleet.wait_for_stall(role).await;
        let engine = primitives.convoys().get(&convoy.metadata.name).expect("admitted engine Convoy");
        assert!(matches!(engine.state(), ConvoyState::Open { .. }), "the stalled convoy remains open: {engine:?}");
        let sent = if noted {
            fleet
                .execute_as_operator(CommandAction::ConvoyResume {
                    namespace: Some(NAMESPACE.into()),
                    name: convoy.metadata.name.clone(),
                    prompt: "Operator note: the API is stable now.".into(),
                    vessel: None,
                    role: None,
                    reopen: false,
                })
                .await
        } else {
            fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Resume)).await
        };
        assert!(!matches!(sent, CommandValue::Error { .. }), "{role}: {sent:?}");
        fleet.wait_for_phase(role, ConvoyPhase::Landed).await;
        fleet.assert_engine_delivered(&convoy.metadata.name);
        let operator = format!("project:{NAMESPACE}/role:operator");
        let coder = format!("project:{NAMESPACE}/convoy:{}/role:coder", convoy.metadata.name);
        let from_operator: Vec<_> = primitives
            .messages()
            .expect("engine messages")
            .into_iter()
            .filter(|(_, message)| message.spec().sender.as_str() == operator && message.spec().inbox.as_str() == coder)
            .collect();
        assert_eq!(from_operator.len(), 1, "{role}: {from_operator:?}");
        let (_, message) = &from_operator[0];
        let receiver = message.state().receiver().unwrap_or_else(|| panic!("{role} delivered: {message:?}"));
        assert!(primitives.sessions().get(&receiver.id).is_some(), "{role} received by the crew's session: {message:?}");
    }
}

// Behaviour (#3938): a crew stall escalated past every supervisor reaches the
// operator role of the convoy's project, where the person consumer is bound.
// Listing it takes nothing; the person's acknowledgement is the receipt, and
// it settles the escalation, which expects nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_a_stall_past_every_supervisor_reaches_the_projects_operator_and_the_person() {
    use flotilla_resources::SupervisionTarget;
    use flotilla_state_machines::machines::{consumer::ConsumerKind, message::MessageState};

    let fleet = DummyDaemon::start_on_primitives(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "").await;
    let mut workflow = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    workflow.supervision = Some(vec![
        SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
        SupervisionTarget::ProjectCrew { convoy_role: "governor".into(), vessel: String::new(), role: "governor".into() },
        SupervisionTarget::Address { address: "principal:operator".into() },
    ]);
    fleet
        .backend()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(&InputMeta::builder().name("engine-escalated".into()).build(), &workflow)
        .await
        .expect("escalating workflow");
    fleet.create_named("engine-escalated", "engine-escalated").await;
    fleet.wait_for_stall("engine-escalated").await;
    let inbox = CommandAction::PersonInbox { namespace: Some(NAMESPACE.into()) };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let item = loop {
        let CommandValue::PersonInbox(listing) = fleet.execute_as_operator(inbox.clone()).await else { panic!("a person inbox") };
        if let Some(item) = listing.items.into_iter().find(|item| item.inbox == "project:fleet/role:operator") {
            break item;
        }
        assert!(tokio::time::Instant::now() < deadline, "the escalation never reached the operator role");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(item.sender, "project:fleet/convoy:engine-escalated/role:coder");
    assert!(item.body.starts_with("Escalated from coder@implement"), "{item:?}");
    let primitives = fleet.daemon.message_primitives().expect("engine path");
    let (id, _) = primitives.locate(&item.message).expect("locate").expect("the escalation is homed here");
    let pending = primitives.inbox().get_async(&id).await.expect("read").expect("escalation");
    assert_eq!(*pending.state(), MessageState::Pending, "listing it is no receipt");

    let acknowledged = fleet
        .execute_as_operator(CommandAction::PersonAcknowledge { namespace: Some(NAMESPACE.into()), message: item.message.clone() })
        .await;
    assert_eq!(acknowledged, CommandValue::Ok);
    let settled = primitives.inbox().get_async(&id).await.expect("read").expect("escalation");
    let MessageState::Settled { receiver: Some(receiver) } = settled.state() else { panic!("settled on the receipt: {settled:?}") };
    assert_eq!((receiver.kind, receiver.id.as_str()), (ConsumerKind::Person, flotilla_core::primitives::PERSON));
    assert!(primitives.people().get(flotilla_core::primitives::PERSON).is_some(), "the receipt is on the person's record");
    let CommandValue::PersonInbox(listing) = fleet.execute_as_operator(inbox).await else { panic!("a person inbox") };
    assert!(listing.items.iter().all(|left| left.message != item.message), "an acknowledged message waits no more: {listing:?}");

    // Answering it after acknowledging takes nothing more and replies to the
    // crew as the role it was addressed to.
    let answer = CommandAction::PersonAnswer {
        namespace: Some(NAMESPACE.into()),
        message: item.message.clone(),
        body: "Carry on with the stable API.".into(),
    };
    assert_eq!(fleet.execute_as_operator(answer.clone()).await, CommandValue::Ok);
    assert_eq!(fleet.execute_as_operator(answer).await, CommandValue::Ok, "the same answer again is the reply already sent");
    let CommandValue::Error { message } = fleet
        .execute_as_operator(CommandAction::PersonAnswer {
            namespace: Some(NAMESPACE.into()),
            message: item.message.clone(),
            body: "A different answer.".into(),
        })
        .await
    else {
        panic!("a different answer to the same message is refused")
    };
    assert!(message.contains("already answered"), "{message}");
    let reply = primitives
        .messages()
        .expect("engine messages")
        .into_iter()
        .map(|(_, message)| message)
        .find(|message| message.spec().in_reply_to.as_ref().is_some_and(|original| original.name == id))
        .expect("the answer is a reply to the escalation");
    assert_eq!(reply.spec().sender.as_str(), "project:fleet/role:operator");
    assert_eq!(reply.spec().inbox.as_str(), "project:fleet/convoy:engine-escalated/role:coder");
}

// A lone coder delivers its engine Convoy after realising the engine's vessel,
// environment, checkout and terminal-session records through the daemon runtime.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_lone_coder_realises_its_resources_and_delivers() {
    use flotilla_provisioning::stored::{checkout, environment, terminal_session};
    use flotilla_state_machines::{
        machines::{convoy::ConvoyState, lifecycle::LifecycleState, vessel::Vessel},
        Resource,
    };

    let fleet = DummyDaemon::start_on_primitives(SCENARIO_AGENT, "").await;
    fleet.dispatch("engine-lone-proof", CrewMix::Coder).await;
    let convoy = fleet.wait_for_phase("engine-lone-proof", ConvoyPhase::Landed).await;
    let engine = fleet.daemon.message_primitives().expect("engine path").convoys().get(&convoy.metadata.name).expect("engine Convoy");
    assert!(matches!(engine.state(), ConvoyState::Delivered { .. }), "the engine convoy delivers: {engine:?}");
    let service = fleet.daemon.decided_store().expect("decided store");
    service
        .with_store(|store| {
            let home = store.home().to_string();
            let mut read = store.read()?;
            macro_rules! realised {
                ($kind:ty) => {{
                    let page = read.scan::<Resource<$kind>>(&home, None, 64)?;
                    assert!(!page.ids.is_empty(), "missing {} engine records", stringify!($kind));
                    for id in page.ids {
                        let record = read.load::<Resource<$kind>>(id)?.expect("engine record").value;
                        assert!(
                            matches!(record.state(), LifecycleState::Realised { .. } | LifecycleState::Gone { .. }),
                            "unrealised engine record: {record:?}"
                        );
                    }
                }};
            }
            realised!(environment::Environment);
            realised!(checkout::Checkout);
            realised!(terminal_session::TerminalSession);
            assert!(!read.scan::<Resource<Vessel>>(&home, None, 64)?.ids.is_empty(), "the engine holds the placement vessel");
            Ok(())
        })
        .expect("read engine provisioning records");
}

// An environment that cannot realise must record its failure in the engine
// and launch no engine TerminalSession; an old-store Failed phase alone is insufficient.
#[tokio::test(start_paused = true)]
async fn on_the_engine_an_unrealised_environment_records_failure_without_a_terminal() {
    use flotilla_provisioning::stored::{environment::Environment, terminal_session::TerminalSession};
    use flotilla_state_machines::{machines::lifecycle::LifecycleState, Resource};

    let fleet = DummyDaemon::start_on_primitives(SCENARIO_AGENT, "failure_rate = 1.0\n").await;
    fleet.dispatch("engine-environment-failure", CrewMix::Coder).await;
    let convoy = fleet.wait_for_phase("engine-environment-failure", ConvoyPhase::Failed).await;
    let engine = fleet.daemon.message_primitives().expect("engine path").convoys().get(&convoy.metadata.name).expect("engine Convoy");
    assert!(!engine.spec().promises.is_empty(), "the engine retains the unmet convoy obligations");
    fleet
        .daemon
        .decided_store()
        .expect("decided store")
        .with_store(|store| {
            let home = store.home().to_string();
            let mut read = store.read()?;
            let environments = read.scan::<Resource<Environment>>(&home, None, 64)?;
            assert!(!environments.ids.is_empty(), "the engine records the failed environment");
            let mut failed = 0;
            for id in environments.ids {
                let record = read.load::<Resource<Environment>>(id)?.expect("environment").value;
                if record.spec().holder.is_some() {
                    assert!(matches!(record.state(), LifecycleState::GaveUp { .. }), "engine failure: {record:?}");
                    failed += 1;
                }
            }
            assert_eq!(failed, 1, "one vessel environment failed");
            assert!(read.scan::<Resource<TerminalSession>>(&home, None, 64)?.ids.is_empty(), "no engine terminal launches");
            Ok(())
        })
        .expect("read engine terminals");
}

// A standing Ensure binds an admitted engine convoy. Abandoning that convoy
// must let the same Ensure admit a fresh generation under the stable engine
// identity, retaining the ended generation in the per-home log.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_standing_ensure_readmits_after_abandon() {
    use flotilla_resources::{ConvoyEnsure, ConvoyEnsureSpec};
    use flotilla_schema::Intern;
    use flotilla_state_machines::{
        machines::ensure::{Ensure, EnsureState},
        Resource,
    };

    let fleet = DummyDaemon::start_on_primitives(SCENARIO_AGENT, "").await;
    let workflows = fleet.backend().using::<WorkflowTemplate>(NAMESPACE);
    let mut standing = CrewMix::Coder.workflow(LONG_RUNNER_TURNS);
    standing.exit = None;
    workflows.create(&InputMeta::builder().name("engine-standing-workflow".into()).build(), &standing).await.expect("standing workflow");
    let forge = catalog::forge(&fleet.fleet.config().forge_service_url);
    let repository = catalog::repository(&forge).expect("repository");
    let projects = fleet.backend().using::<flotilla_resources::Project>(NAMESPACE);
    let project = projects.get("fleet").await.expect("project");
    let mut project_spec = project.spec;
    project_spec.repositories = vec![flotilla_resources::ProjectRepositorySpec::builder().repo(repository.key()).build()];
    projects
        .update(&InputMeta::builder().name("fleet".into()).build(), &project.metadata.resource_version, &project_spec)
        .await
        .expect("project membership");
    fleet
        .backend()
        .using::<ConvoyEnsure>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("engine-standing".into())
                .annotations(std::collections::BTreeMap::from([(
                    flotilla_charter::ops_entry::SOURCE_COMMIT_ANNOTATION.into(),
                    "fixture".into(),
                )]))
                .build(),
            &ConvoyEnsureSpec::builder()
                .project_ref("fleet".into())
                .role("governor".into())
                .workflow_ref("engine-standing-workflow".into())
                .placement_policy(DUMMY_FULFILMENT_KIND.into())
                .repositories(vec![repository.key()])
                .build(),
        )
        .await
        .expect("standing ensure");
    let service = fleet.daemon.decided_store().expect("decided store");
    let bound = || {
        service
            .with_store(|store| {
                let home = store.home().to_string();
                let mut read = store.read()?;
                let Some(id) = read.resolve(<Resource<Ensure> as flotilla_schema::Kind>::CODE, "engine-standing", &home)? else {
                    return Ok(None);
                };
                Ok(read.load::<Resource<Ensure>>(id)?.and_then(|loaded| match loaded.value.state() {
                    EnsureState::Fulfilled { bound, .. } => Some(bound.clone()),
                    _ => None,
                }))
            })
            .expect("read engine Ensure")
    };
    let first = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(bound) = bound() {
                break bound;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    let first = match first {
        Ok(first) => first,
        Err(error) => panic!(
            "engine Ensure binds its first convoy: {error}; {:?}",
            fleet.backend().using::<ConvoyEnsure>(NAMESPACE).get("engine-standing").await.map(|ensure| ensure.status)
        ),
    };
    let name = first.as_str();
    tokio::time::timeout(Duration::from_secs(30), async {
        while fleet.daemon.message_primitives().expect("engine path").convoys().get(name).is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the bound convoy is admitted on the engine");
    let first_old = fleet
        .backend()
        .using::<ConvoyEnsure>(NAMESPACE)
        .get("engine-standing")
        .await
        .expect("ensure")
        .status
        .expect("status")
        .convoy_ref
        .expect("generation");
    let abandoned = fleet
        .execute_as_operator(CommandAction::ConvoyAbandon {
            namespace: Some(NAMESPACE.into()),
            name: first_old.clone(),
            reason: "exercise standing re-admission".into(),
        })
        .await;
    assert!(!matches!(abandoned, CommandValue::Error { .. }), "{abandoned:?}");
    // Explicit reconciliation bypasses the wall-clock retry backoff under paused time.
    fleet.daemon.reconcile_convoy_ensure_now(NAMESPACE, "engine-standing", fleet.daemon.as_ref()).await.expect("re-admit ensure now");
    let second = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let ensure = fleet.backend().using::<ConvoyEnsure>(NAMESPACE).get("engine-standing").await.expect("ensure");
            if let Some(next) = ensure.status.and_then(|status| status.convoy_ref).filter(|next| next != &first_old) {
                if fleet
                    .daemon
                    .message_primitives()
                    .expect("engine")
                    .convoys()
                    .get(first.as_str())
                    .is_some_and(|convoy| matches!(convoy.state(), flotilla_state_machines::machines::convoy::ConvoyState::Open { .. }))
                {
                    break next;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "Ensure admits a fresh generation after abandon: {error}; engine={:?}",
            fleet.daemon.message_primitives().expect("engine").convoys().get(first.as_str())
        )
    });
    assert_ne!(first_old, second);
    assert_eq!(bound(), Some(first.clone()), "the engine fulfiller identity stays stable");
    let engine = fleet.daemon.message_primitives().expect("engine").convoys().get(first.as_str()).expect("standing convoy");
    assert!(
        matches!(engine.state(), flotilla_state_machines::machines::convoy::ConvoyState::Open { .. }),
        "new generation must be active: {engine:?}"
    );
}

impl DummyDaemon {
    // A fixed admitted identity makes the dummy provider's seeded failure draw
    // reproducible while the runtime still bootstraps and reconciles the convoy.
    async fn create_named(&self, name: &str, workflow: &str) {
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        let repository = catalog::repository(&forge).expect("repository");
        self.backend()
            .using::<Convoy>(NAMESPACE)
            .create(
                &InputMeta::builder().name(name.into()).labels(std::collections::BTreeMap::from([("proof".into(), name.into())])).build(),
                &flotilla_resources::ConvoySpec::builder()
                    .workflow_ref(workflow.into())
                    .role(name.into())
                    .r#ref("main".into())
                    .project_ref("fleet".into())
                    .placement_policy(DUMMY_FULFILMENT_KIND.into())
                    .repositories(vec![flotilla_resources::ConvoyRepositorySpec {
                        url: catalog::repository_url(&forge),
                        repo_ref: repository.key(),
                        source_ref: "main".into(),
                        target_ref: "main".into(),
                        workspace_slug: "dummy".into(),
                        subpaths: Vec::new(),
                    }])
                    .build(),
            )
            .await
            .expect("named convoy");
    }

    fn assert_engine_delivered(&self, name: &str) {
        use flotilla_state_machines::machines::convoy::ConvoyState;
        let engine = self.daemon.message_primitives().expect("engine").convoys().get(name).expect("engine convoy");
        assert!(matches!(engine.state(), ConvoyState::Delivered { .. }), "engine delivery: {engine:?}");
    }
}

// A real governor must resume a stalled coder and deliver its engine Convoy.
#[tokio::test(start_paused = true)]
async fn on_the_engine_crew_supervision_resumes_a_stalled_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Resume).await;
}

// A failing crew proposes failure; its named governor must fail it while its
// engine promises stay unmet and its supervision is recorded on the engine.
#[tokio::test(start_paused = true)]
async fn on_the_engine_crew_supervision_fails_a_failing_coder() {
    supervised_coder_scenario(CrewSupervisionAction::Fail).await;
}

async fn supervised_coder_scenario(action: CrewSupervisionAction) {
    use flotilla_environment::environment::dummy::dummy_draw;
    use flotilla_resources::SupervisionTarget;
    use flotilla_state_machines::machines::awaiting_reply::AwaitingReply;
    use flotilla_state_machines::{machines::convoy::ConvoyState, Resource};

    let (domain, agent) =
        if action == CrewSupervisionAction::Resume { ("crew-stall", "stall_rate = 0.5\n") } else { ("crew-failure", "fail_rate = 0.5\n") };
    // Explicitly cover a faulting coder and a healthy supervisor, without
    // random UUIDs changing the dummy provider's deterministic draws.
    let name = (0..128)
        .map(|n| format!("engine-supervised-{n}"))
        .find(|name| {
            dummy_draw(0, domain, &format!("{name}/{name}-implement/coder")) < 0.5
                && dummy_draw(0, domain, &format!("{name}/{name}-supervise/governor")) >= 0.5
        })
        .expect("seeded case");
    let fleet = DummyDaemon::start_on_primitives(agent, "").await;
    let mut workflow = CrewMix::CoderSupervisor.workflow(LONG_RUNNER_TURNS);
    workflow.supervision =
        Some(vec![SupervisionTarget::ConvoyCrew { vessel: "supervise".into(), role: "governor".into() }, SupervisionTarget::Operator]);
    fleet
        .backend()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(&InputMeta::builder().name("engine-supervised".into()).build(), &workflow)
        .await
        .expect("supervised workflow");
    fleet.create_named(&name, "engine-supervised").await;
    let convoy = fleet
        .wait_for(&name, "crew-supervised stall", |convoy| {
            convoy.status.as_ref().and_then(|s| s.stalled.as_ref()).is_some_and(|s| s.supervisor.is_some())
        })
        .await;
    let engine = fleet.daemon.message_primitives().expect("engine").convoys().get(&name).expect("engine convoy");
    assert!(matches!(engine.state(), ConvoyState::Open { .. }), "engine obligations remain open while stalled");
    let supervisor = fleet
        .backend()
        .using::<flotilla_resources::TerminalSession>(NAMESPACE)
        .list()
        .await
        .expect("terminals")
        .items
        .into_iter()
        .find(|session| session.spec.role == "governor")
        .expect("governor session");
    let actor = supervisor.status.expect("status").crew.expect("crew identity").id;
    let mut ruling = supervise(&convoy, action);
    if let CommandAction::CrewSupervise { actor_crew_id, .. } = &mut ruling {
        *actor_crew_id = Some(actor);
    }
    assert_eq!(fleet.execute(ruling).await, CommandValue::Ok, "the named crew supervisor can rule");
    if action == CrewSupervisionAction::Resume {
        fleet.wait_for_phase(&name, ConvoyPhase::Landed).await;
        fleet.assert_engine_delivered(&name);
    } else {
        fleet.wait_for_phase(&name, ConvoyPhase::Failed).await;
        let engine = fleet.daemon.message_primitives().expect("engine").convoys().get(&name).expect("engine convoy");
        assert!(engine.spec().promises.iter().any(|promise| promise.record.open()), "failure must not keep the engine promises");
    }
    fleet
        .daemon
        .decided_store()
        .expect("store")
        .with_store(|store| {
            let home = store.home().to_string();
            assert!(
                !store.read()?.scan::<Resource<AwaitingReply>>(&home, None, 64)?.ids.is_empty(),
                "the crew stall has an engine AwaitingReply"
            );
            Ok(())
        })
        .expect("engine supervision records");
}

impl DummyDaemon {
    // Transport fixtures for kinds the dummy workflow does not request. These
    // enter through real engine homes; they prove replication, not provisioning
    // or standing-convoy admission.
    fn seed_transport_kinds(&self) {
        use flotilla_engine_store::StoreLog;
        use flotilla_provisioning::stored::{
            clone::{Clone, CloneRequest},
            image_build::{Architecture, Delivery, ImageBuild, ImageBuildRequest, Reservation},
        };
        use flotilla_schema::{Address, Frozen, Timestamp};
        use flotilla_state_machines::{
            machines::{
                ensure::{Ensure, EnsureSpec, MemoryEnsureWorld},
                lifecycle::MemoryLifecycleWorld,
            },
            Home,
        };
        let service = self.daemon.decided_store().expect("store");
        let home = self.daemon.node_id().as_str();
        let at = Timestamp::from_datetime(&chrono::Utc::now());
        let mut builds = Home::<ImageBuild, _>::with_log(home, StoreLog::new(service.store())).expect("image home");
        builds
            .create(
                "transport-build",
                ImageBuildRequest {
                    composition: "sha256:transport".into(),
                    architecture: Architecture::Amd64,
                    layers: Frozen::new(Vec::new()).expect("layers"),
                    delivery: Delivery::Local,
                    reservation: Reservation { cpu: 1, disk_bytes: 1 },
                },
                &MemoryLifecycleWorld::default(),
                at,
            )
            .expect("image record");
        let mut clones = Home::<Clone, _>::with_log(home, StoreLog::new(service.store())).expect("clone home");
        clones
            .create(
                "transport-clone",
                CloneRequest {
                    repository: "transport-repository".into(),
                    url: "https://forge.dummy.invalid/transport".into(),
                    environment: flotilla_schema::Ref::new("transport-environment", home),
                    path: "/transport".into(),
                },
                &MemoryLifecycleWorld::default(),
                at,
            )
            .expect("clone record");
        let mut awaiting =
            Home::<flotilla_state_machines::machines::awaiting_reply::AwaitingReply, _>::with_log(home, StoreLog::new(service.store()))
                .expect("awaiting home");
        awaiting
            .create(
                "transport-awaiting",
                flotilla_state_machines::machines::awaiting_reply::AwaitingReplySpec {
                    message: flotilla_schema::Ref::new("transport-message", home),
                    sender: Address::project("fleet").expect("sender"),
                    recipient: Address::project("fleet").expect("recipient"),
                    subject: None,
                    kind: "transport".into(),
                    awaited: flotilla_state_machines::machines::awaiting_reply::Awaited::Reply,
                    deadline: None,
                },
                &flotilla_state_machines::machines::awaiting_reply::MemoryAwaitingReplyWorld::default(),
                at,
            )
            .expect("awaiting record");
        let mut ensures = Home::<Ensure, _>::with_log(home, StoreLog::new(service.store())).expect("ensure home");
        ensures
            .create(
                "transport-ensure",
                EnsureSpec {
                    inbox: Address::project("fleet").expect("project"),
                    policy: flotilla_state_machines::AttemptPolicy::immediate(3),
                },
                &MemoryEnsureWorld::default(),
                at,
            )
            .expect("ensure record");
        // #3824 layer 1: the account kinds, a binding machine and its provision, a plain kind.
        let binding = self.transport_binding(at);
        let mut bindings =
            Home::<flotilla_provisioning::stored::account::AccountBinding, _>::with_log(home, StoreLog::new(service.store()))
                .expect("binding home");
        bindings.create("transport-binding", binding, &(), at).expect("binding record");
        flotilla_engine_store::plain::PlainLog::new(service.store())
            .create(
                "transport-provision",
                &flotilla_provisioning::stored::account::ProvisionSpec {
                    binding: flotilla_schema::Ref::new("transport-binding", home),
                    // The daemon's own host-direct place, which its provision controller keeps.
                    place: flotilla_resources::host_direct_environment_name(
                        &self.daemon.local_host_id().expect("local host id").to_string(),
                    ),
                    environment_kind: "host-direct".into(),
                    plan: flotilla_resources::account_access::ProvisionPlan {
                        implementation: "claude-direct".into(),
                        effective_access: "account".into(),
                        mechanism: "oauth token".into(),
                        update_mode: flotilla_resources::account_access::UpdateMode::Push,
                    },
                    home: "/transport".into(),
                    delivered: None,
                },
                &flotilla_engine_store::plain::PendingChange::new(flotilla_schema::NewChange::new("transport", at)),
            )
            .expect("provision record");
        // #3824 layer 2c: a ManifestRoot, declared with no records yet.
        let mut roots =
            Home::<flotilla_provisioning::stored::manifest_root::ManifestRoot, _>::with_log(home, StoreLog::new(service.store()))
                .expect("root home");
        let request = flotilla_provisioning::stored::manifest_root::RootRequest {
            host: "transport".into(),
            path: "/transport/manifests".into(),
            source: "transport".into(),
            binding: None,
        };
        roots.create("manifest-transport", request, &(), at).expect("root record");
        // #3824 layer 2b-ii: one record of each definition kind.
        Self::seed_definitions(&flotilla_engine_store::plain::PlainLog::new(service.store()), at);
    }

    fn seed_definitions(log: &flotilla_engine_store::plain::PlainLog, at: flotilla_schema::Timestamp) {
        use flotilla_provisioning::stored::definition::{
            CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition, ForgeDefinition,
            ImageLayerDefinition, WorkflowTemplateDefinition,
        };
        fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
            serde_json::from_value(value).expect("fixture spec")
        }
        let builtin = |kind: &str| flotilla_resources::builtins::builtin_definitions(kind).expect("built-ins")[0].spec.clone();
        // One seeding pass, one change.
        let change = flotilla_engine_store::plain::PendingChange::new(flotilla_schema::NewChange::new("transport", at));
        let forge: flotilla_resources::ForgeSpec = decode(serde_json::json!({
            "forge_id": "transport", "kind": "forgejo", "hosts": ["transport.example"],
            "https_url": "https://transport.example", "git_ssh_host": "transport.example"
        }));
        log.create("transport", &ForgeDefinition::new(&forge), &change).expect("forge record");
        let fleet: flotilla_resources::FleetDesignationSpec = decode(serde_json::json!({"project": "transport"}));
        log.create("transport-fleet", &FleetDesignationDefinition::new(&fleet), &change).expect("fleet record");
        let credential: flotilla_resources::CredentialSpecSpec = decode(serde_json::json!({
            "consumer": {"adapter": "claude-oauth", "account_email": "transport@example.invalid"},
            "lifecycle": "static", "source": {"kind": "file", "path": "/transport/token"}
        }));
        log.create("transport-credential", &CredentialSpecDefinition::new(&credential), &change).expect("spec record");
        let grant: flotilla_resources::CredentialGrantSpec =
            decode(serde_json::json!({"credentials": ["transport-credential"], "selector": {"projects": ["transport"]}}));
        log.create("transport-grant", &CredentialGrantDefinition::new(&grant), &change).expect("grant record");
        log.create("transport-defaults", &CrewDefaultsDefinition::new(&Default::default()), &change).expect("defaults record");
        let layer: flotilla_resources::ImageLayerResourceSpec = decode(builtin("ImageLayer"));
        let flotilla_resources::ImageLayerResourceSpec::Current(layer) = layer else { panic!("a built-in layer is current") };
        log.create("transport-layer", &ImageLayerDefinition::new(&layer).expect("layer"), &change).expect("layer record");
        let template: flotilla_resources::WorkflowTemplateSpec = decode(builtin("WorkflowTemplate"));
        log.create("transport-template", &WorkflowTemplateDefinition::new(None, &template).expect("template"), &change)
            .expect("template record");
        // #3824 layer 3a: a Repository, named by its key.
        let repository = flotilla_resources::RepositorySpec::remote("https://transport.example/transport/app").expect("repository");
        let record = flotilla_provisioning::stored::repository::RepositoryRecord::new(repository.clone()).expect("record");
        log.create(&repository.key().to_string(), &record, &change).expect("repository record");
        // #3824 layer 3b: a Project, and its project operations' record.
        let project = flotilla_resources::ProjectSpec::builder().display_name("transport".into()).build();
        let record = flotilla_provisioning::stored::project::ProjectDefinition::new(&project).expect("project");
        log.create("transport", &record, &change).expect("project record");
        let ops = flotilla_provisioning::stored::project::ProjectOpsRecord { refused: None, entries: None };
        log.create("transport", &ops, &change).expect("project ops record");
        let artifact = flotilla_resources::ArtifactSpec::builder()
            .convoy("transport-convoy".into())
            .producer("coder".into())
            .kind("brief".into())
            .subject("transport-convoy".into())
            .digest("ef37".into())
            .size(1)
            .media_type("text/markdown".into())
            .expires_at(chrono::DateTime::UNIX_EPOCH)
            .build();
        let record = flotilla_provisioning::stored::artifact::ArtifactRecord::from_spec(&artifact).expect("artifact");
        log.create("transport-artifact", &record, &change).expect("artifact record");
        // One record of each forge observation.
        use flotilla_provisioning::stored::forge_observation::{
            ChangeRequestRecord, ForgeReadRecord, IssueRecord, IssueState, IssueSubject, ReadValue,
        };
        let change_request = ChangeRequestRecord {
            service: "github.com".into(),
            scope: "transport/app".into(),
            number: 1,
            title: Some("transport".into()),
            author: None,
            state: Some(flotilla_provisioning::stored::forge_observation::ChangeRequestState::Open),
            review_decision: None,
            review_requested_from_owner: None,
            head_sha: Some("abc".into()),
            checks: None,
            actionable_at_head: None,
            mergeable: None,
            failing_since: None,
            head_branch: Some("transport".into()),
            base_branch: Some("main".into()),
            closes: vec![IssueSubject { service: "github.com".into(), scope: "transport/app".into(), number: 2 }],
            closes_observed: true,
        };
        log.create("transport-cr", &change_request, &change).expect("change request record");
        let issue = IssueRecord {
            service: "github.com".into(),
            scope: "transport/app".into(),
            number: 2,
            title: Some("transport".into()),
            assignees: Some(Vec::new()),
            state: Some(IssueState::Open),
            labels: None,
            updated_at: Some(at),
            failing_since: None,
            body: Some("transport".into()),
            issue_type: Some("Task".into()),
            parent_service: None,
            parent_scope: None,
            parent_number: None,
            blocked_by: Vec::new(),
            blocked_by_observed: true,
            fields: Some(std::collections::BTreeMap::from([("Weight".into(), "2".into())])),
        };
        log.create("transport-issue", &issue, &change).expect("issue record");
        let read = ForgeReadRecord {
            service: "https://github.com".into(),
            scope: "transport/app".into(),
            request: flotilla_resources::ForgeReadRequest::Board,
            value: Some(ReadValue { value: serde_json::Value::Null }),
            error: None,
            retry_at: None,
        };
        log.create("transport-forge-read", &read, &change).expect("forge read record");
    }

    fn transport_binding(&self, at: flotilla_schema::Timestamp) -> flotilla_provisioning::stored::account::BindingSpec {
        use flotilla_resources::account_access::{requester_address, AccessName, CredentialName, Selection};
        flotilla_provisioning::stored::account::BindingSpec {
            requester: requester_address(Some("fleet"), "transport-convoy", "coder").expect("requester"),
            item: "claude-code".into(),
            requirement: AccessName("claude-code:account".into()),
            credential: CredentialName("transport-credential".into()),
            account: None,
            grants: Default::default(),
            candidates: Default::default(),
            exclusions: Default::default(),
            selection: Selection::Draw { index: 0 },
            epochs: flotilla_schema::Records::new(),
            opened_at: at,
        }
    }

    // A replica may consume a later edge before the driver checks its earlier
    // snapshot. Exercise that ordering without depending on teardown timing.
    fn advance_transport_ensure(&self) {
        use flotilla_engine_store::{open_home, StoreLog};
        use flotilla_schema::Timestamp;
        use flotilla_state_machines::{
            machines::ensure::{Ensure, MemoryEnsureWorld, Record},
            Ref,
        };
        let service = self.daemon.decided_store().expect("store");
        let mut home = open_home::<Ensure>(StoreLog::new(service.store())).expect("ensure home");
        home.apply(
            "transport-ensure",
            Record { fulfiller: Ref::new("later-transport-generation") },
            &MemoryEnsureWorld::default(),
            Timestamp::from_datetime(&chrono::Utc::now()),
        )
        .expect("advance transport ensure");
    }

    fn engine_snapshots(&self) -> Vec<flotilla_schema_sqlite::Snapshot> {
        use flotilla_provisioning::stored::{
            checkout::Checkout, clone::Clone, environment::Environment, image_build::ImageBuild, terminal_session::TerminalSession,
        };
        use flotilla_state_machines::{
            machines::{
                agent_session::AgentSession, awaiting_reply::AwaitingReply, convoy::Convoy, ensure::Ensure, message::Message,
                person::Person, vessel::Vessel,
            },
            Resource,
        };
        self.daemon
            .decided_store()
            .expect("store")
            .with_store(|store| {
                let home = store.home().to_string();
                let registered: std::collections::BTreeSet<_> = store.kinds().iter().map(|kind| kind.code).collect();
                let mut covered = std::collections::BTreeSet::new();
                let mut snapshots = Vec::new();
                let mut read = store.read()?;
                macro_rules! collect {
                    ($kind:ty) => {
                        collect!(stored Resource<$kind>)
                    };
                    (stored $stored:ty) => {{
                        let code = <$stored as flotilla_schema::Kind>::CODE;
                        covered.insert(code);
                        let mut after = None;
                        let before = snapshots.len();
                        loop {
                            let page = read.scan::<$stored>(&home, after, 64)?;
                            for id in page.ids {
                                snapshots.push(read.load_snapshot(id)?.expect("snapshot"));
                            }
                            after = page.next;
                            if after.is_none() {
                                break;
                            }
                        }
                        assert!(snapshots.len() > before, "transport proof has no {} record", stringify!($stored));
                    }};
                }
                collect!(Message);
                collect!(AgentSession);
                // The delivery adapter keeps the person consumer's record.
                collect!(Person);
                collect!(Convoy);
                collect!(AwaitingReply);
                collect!(Environment);
                collect!(ImageBuild);
                collect!(Clone);
                collect!(Checkout);
                collect!(TerminalSession);
                collect!(Vessel);
                collect!(Ensure);
                collect!(flotilla_provisioning::stored::account::AccountBinding);
                collect!(stored flotilla_provisioning::stored::account::ProvisionSpec);
                collect!(flotilla_provisioning::stored::manifest_root::ManifestRoot);
                collect!(stored flotilla_provisioning::stored::definition::ForgeDefinition);
                collect!(stored flotilla_provisioning::stored::definition::FleetDesignationDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CredentialSpecDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CredentialGrantDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CrewDefaultsDefinition);
                collect!(stored flotilla_provisioning::stored::definition::ImageLayerDefinition);
                collect!(stored flotilla_provisioning::stored::definition::WorkflowTemplateDefinition);
                collect!(stored flotilla_provisioning::stored::repository::RepositoryRecord);
                collect!(stored flotilla_provisioning::stored::project::ProjectDefinition);
                collect!(stored flotilla_provisioning::stored::project::ProjectOpsRecord);
                collect!(stored flotilla_provisioning::stored::artifact::ArtifactRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::ChangeRequestRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::IssueRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::ForgeReadRecord);
                collect!(stored flotilla_provisioning::stored::placement::FulfilmentKindDefinition);
                assert_eq!(covered, registered, "every registered engine kind needs a transport fixture");
                Ok(snapshots)
            })
            .expect("engine snapshots")
    }
}

// Both live peer runtimes consume the real change stream. Every registered
// engine kind is read back at the other host with the same home, version and
// stored rows; the old replicator runs beside it and still copies old Convoys.
#[tokio::test(start_paused = true)]
async fn on_the_engine_two_daemons_replicate_every_kind_alongside_the_old_replicator() {
    use OldInventory::{HomeLocal, Replicated};
    let a = DummyDaemon::start_with_relay().host("engine-a").call().await;
    let b = DummyDaemon::start_with_relay().host("engine-b").call().await;
    a.deliver("replicated-a", CrewMix::CoderReviewer).await;
    b.deliver("replicated-b", CrewMix::CoderReviewer).await;
    let _peers = flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful(Arc::clone(&a.daemon), Arc::clone(&b.daemon))
        .await
        .expect("peer runtimes");
    for (source, destination, role) in [(&a, &b, "replicated-a"), (&b, &a, "replicated-b")] {
        let convoy = source.convoy(role).await.expect("source convoy");
        tokio::time::timeout(Duration::from_secs(60), async {
            while destination
                .daemon
                .message_primitives()
                .expect("engine")
                .convoys()
                .replica(&convoy.metadata.name, source.daemon.node_id().as_str())
                .expect("replica")
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("initial engine snapshot replicates");
    }
    // These admissions follow the initial snapshot, so the proof also needs
    // committed cursor continuation; an initial snapshot alone is insufficient.
    a.seed_transport_kinds();
    b.seed_transport_kinds();
    let expected_a = a.engine_snapshots();
    let expected_b = b.engine_snapshots();
    a.advance_transport_ensure();
    b.advance_transport_ensure();
    for (source, destination, expected) in [(&a, &b, &expected_a), (&b, &a, &expected_b)] {
        let mut mismatch = None;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                // The source and replica reads are not atomic. A replica newer
                // than this cut fails equality and is checked again next poll.
                let current = source.engine_snapshots();
                let complete = destination
                    .daemon
                    .decided_store()
                    .expect("store")
                    .with_store(|store| {
                        let mut read = store.read()?;
                        for minimum in expected {
                            let snapshot = current
                                .iter()
                                .find(|snapshot| {
                                    snapshot.kind == minimum.kind && snapshot.name == minimum.name && snapshot.home == minimum.home
                                })
                                .unwrap_or_else(|| panic!("source retains transport record {} {}", minimum.kind, minimum.name));
                            assert!(
                                snapshot.version >= minimum.version,
                                "source version regressed for {} {}: {} < {}",
                                minimum.kind,
                                minimum.name,
                                snapshot.version,
                                minimum.version
                            );
                            let Some(id) = read.resolve(snapshot.kind, &snapshot.name, &snapshot.home)? else {
                                mismatch = Some((snapshot.kind, snapshot.name.clone(), snapshot.version, None));
                                return Ok(false);
                            };
                            let actual = read.load_snapshot(id)?;
                            if actual.as_ref() != Some(snapshot) {
                                mismatch =
                                    Some((snapshot.kind, snapshot.name.clone(), snapshot.version, actual.map(|value| value.version)));
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    })
                    .expect("replicated engine rows");
                if complete {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("every engine kind replicates through the peer runtime: {error}; mismatch={mismatch:?}"));
        assert_old_inventory::<flotilla_resources::Convoy>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Vessel>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Environment>(source, destination, HomeLocal).await;
        assert_old_inventory::<flotilla_resources::Checkout>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::TerminalSession>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Clone>(source, destination, HomeLocal).await;
        assert_old_inventory::<flotilla_resources::ImageBuild>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::ConvoyEnsure>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Message>(source, destination, Replicated).await;
        assert!(
            destination
                .backend()
                .including_replicas::<flotilla_resources::Message>(NAMESPACE)
                .list()
                .await
                .expect("old messages")
                .items
                .is_empty(),
            "Messages live only on the engine"
        );
    }
}

// Stands in for the reconnecting command socket at the process boundary. The
// dummy terminal processes survive, while their daemon endpoint is replaced.
struct RestartRelay {
    commands: std::sync::Mutex<Option<Arc<flotilla_dummy_fleet::DaemonCommandPort>>>,
    ready: tokio::sync::Notify,
    backend: ResourceBackend,
}

impl RestartRelay {
    fn connect(&self, daemon: &Arc<InProcessDaemon>) {
        let handle = Arc::clone(daemon) as Arc<dyn DaemonHandle>;
        *self.commands.lock().expect("relay") = Some(flotilla_dummy_fleet::DaemonCommandPort::new(&handle));
        self.ready.notify_waiters();
    }
}

#[async_trait::async_trait]
impl flotilla_dummy_fleet::CrewCommandPort for RestartRelay {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        loop {
            let ready = self.ready.notified();
            let commands = self.commands.lock().expect("relay").clone();
            if let Some(commands) = commands {
                return commands.execute(action).await;
            }
            ready.await;
        }
    }

    async fn report_attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
        use flotilla_resources::{
            TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession, TerminalSessionStatusPatch,
        };
        let attention = TerminalAttention {
            state: if working { TerminalAttentionState::Working } else { TerminalAttentionState::Idle },
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        };
        flotilla_store::apply_status_patch(
            &self.backend.clone().using::<TerminalSession>(namespace),
            session,
            &TerminalSessionStatusPatch::ObserveAttention { attention },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

impl DummyDaemon {
    async fn restart(self) -> Self {
        let Self { daemon, fleet, _runtime, relay, _dir } = self;
        let relay = relay.expect("restartable fixture");
        *relay.commands.lock().expect("relay") = None;
        let backend = daemon.resource_backend();
        let host = daemon.host_name().clone();
        let node = daemon.node_id().clone();
        _runtime.shutdown();
        drop(daemon);
        tokio::task::yield_now().await;
        let config = Arc::new(ConfigStore::with_base(_dir.path().join("config")));
        let mut discovery = fake_discovery_with_host_directories(_dir.path());
        fleet.install(&mut discovery);
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&config),
            discovery,
            host,
            backend,
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        assert_eq!(daemon.node_id(), &node, "restart preserves the engine home identity");
        let runtime = DaemonRuntime::start_with_options(
            Arc::clone(&daemon),
            config,
            None,
            RuntimeOptions { namespace: NAMESPACE.into(), controller_resync_interval: Duration::from_millis(200), ..Default::default() },
        )
        .await
        .expect("restarted runtime");
        relay.connect(&daemon);
        Self { daemon, fleet, _runtime: runtime, relay: Some(relay), _dir }
    }
}

// A daemon restart restores its engine Convoy and provisioning records while
// the crew process survives. Its next submissions advance that restored record
// and deliver the convoy without re-admission or a duplicate process launch.
#[tokio::test(start_paused = true)]
async fn on_the_engine_a_daemon_restart_restores_an_active_convoy_and_lands() {
    let fleet = DummyDaemon::start_with_relay().agent("turn_delay_ms = 10000\n").host("restart-proof").restartable(true).call().await;
    fleet.dispatch("engine-restart", CrewMix::Coder).await;
    let convoy = fleet
        .wait_for("engine-restart", "a launched coder before its first submission", |_| fleet.fleet.snapshot().sessions_launched == 1)
        .await;
    let before =
        fleet.daemon.message_primitives().expect("engine").convoys().get(&convoy.metadata.name).expect("engine admission before restart");
    let persisted = fleet
        .daemon
        .decided_store()
        .expect("store")
        .with_store(|store| {
            let home = store.home().to_string();
            let mut read = store.read()?;
            let id = read
                .resolve(
                    <flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy> as flotilla_schema::Kind>::CODE,
                    &convoy.metadata.name,
                    &home,
                )?
                .expect("persisted convoy");
            Ok(read.load_snapshot(id)?.expect("snapshot"))
        })
        .expect("persisted engine record")
        .decode::<flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy>>()
        .expect("decode stored convoy");
    let fleet = fleet.restart().await;
    let restored = fleet.daemon.message_primitives().expect("engine").convoys().get(&convoy.metadata.name).expect("restored engine convoy");
    assert_eq!(restored.version(), before.version(), "the persisted engine version is restored");
    assert_eq!(restored.spec(), persisted.spec(), "engine promises and seats survive restart");
    fleet.wait_for_phase("engine-restart", ConvoyPhase::Landed).await;
    fleet.assert_engine_delivered(&convoy.metadata.name);
    assert_eq!(fleet.fleet.snapshot().sessions_launched, 1, "the surviving process is adopted");
}

// The old replicator copies old-store representations, not decided-store rows.
// Compare every old counterpart's actual inventory, including empty kinds.
#[derive(Clone, Copy, Debug)]
enum OldInventory {
    Replicated,
    HomeLocal,
}

async fn assert_old_inventory<T: flotilla_resources::Resource>(source: &DummyDaemon, destination: &DummyDaemon, inventory: OldInventory) {
    match inventory {
        OldInventory::HomeLocal => assert_eq!(T::REPLICATION_CLASS, flotilla_resources::ReplicationClass::None, "home-local old kind"),
        OldInventory::Replicated => assert_ne!(T::REPLICATION_CLASS, flotilla_resources::ReplicationClass::None, "replicated old kind"),
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let expected: std::collections::BTreeMap<_, _> = source.backend().using::<T>(NAMESPACE).list().await.expect("old source inventory").items.into_iter()
                .map(|row| (row.metadata.name, serde_json::to_value(row.spec).expect("old spec"))).collect();
            let replicated: std::collections::BTreeMap<_, _> = destination.backend().including_replicas::<T>(NAMESPACE).list().await.expect("old replica inventory").items.into_iter()
                .filter(|row| matches!(&row.provenance, flotilla_resources::ResourceProvenance::Replica { origin_root, .. } if origin_root == source.daemon.node_id()))
                .map(|row| (row.object.metadata.name, serde_json::to_value(row.object.spec).expect("replica spec"))).collect();
            let expected = match inventory { OldInventory::HomeLocal => std::collections::BTreeMap::new(), OldInventory::Replicated => expected };
            if expected == replicated { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap_or_else(|error| panic!("old {} inventory does not replicate: {error}", T::API_PATHS.kind));
}
