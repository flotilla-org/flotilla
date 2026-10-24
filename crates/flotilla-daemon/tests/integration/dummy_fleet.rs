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
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let switch = if primitives { "[primitives]\n" } else { "" };
        let daemon_toml = format!(
            "machine_id = \"dummy-load\"\n\n{switch}[dummy_fleet]\n[dummy_fleet.agent]\nlong_runner_turns = {LONG_RUNNER_TURNS}\n{agent}\n[dummy_fleet.environment]\n{environment}"
        );
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml).expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery(false);
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&config),
            discovery,
            HostName::new("dummy-load"),
            backend,
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
        let options = RuntimeOptions {
            namespace: NAMESPACE.into(),
            heartbeat_interval: Duration::from_secs(5),
            controller_resync_interval: resync,
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
        if self.daemon.message_primitives().is_some() {
            // The fleet roles live under the designated project; tests keep `project:fleet`.
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
            backend
                .clone()
                .definitions::<flotilla_resources::FleetDesignation>(NAMESPACE)
                .create(
                    &InputMeta::builder().name(flotilla_resources::FLEET_DESIGNATION_NAME.to_string()).build(),
                    &flotilla_resources::FleetDesignationSpec { project: "fleet".into(), image_cache: None },
                )
                .await
                .expect("fleet designation");
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
            .using::<PlacementPolicy>(NAMESPACE)
            .create(
                &InputMeta::builder().name(DUMMY_PLACEMENT_POLICY.to_string()).labels(catalog::placement_policy_labels()).build(),
                &catalog::placement_policy(&host.metadata.name),
            )
            .await
            .expect("dummy placement policy");
        backend
            .clone()
            .using::<flotilla_resources::FulfilmentKind>(NAMESPACE)
            .create(
                &InputMeta::builder().name(DUMMY_PLACEMENT_POLICY.into()).build(),
                &flotilla_resources::FulfilmentKindSpec::from_policy(&catalog::placement_policy(&host.metadata.name), "linux")
                    .expect("dummy fulfilment"),
            )
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
    let fleet = DummyDaemon::start(SCENARIO_AGENT, "").await;
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

impl DummyDaemon {
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

// Behaviour (#3317): an operator sends as `principal:<name>`, which has no
// declared role until the principals model (#3457); on the engine path the
// ruling is refused with that reason, never sent as a guessed role.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_an_operator_ruling_is_refused_until_principals() {
    let fleet = DummyDaemon::start_on_primitives(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "").await;
    fleet.dispatch("engine-stalled", CrewMix::Coder).await;
    fleet.wait_for("engine-stalled", "stall accounting", |_| fleet.fleet.snapshot().crews_stalled > 0).await;
    let convoy = fleet.wait_for_stall("engine-stalled").await;
    let resumed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Resume)).await;
    assert!(matches!(&resumed, CommandValue::Error { message } if message.contains("#3457")), "{resumed:?}");
}

// A supervisor must be able to resume a stalled engine convoy, after which its
// change-request promise is kept and the engine Convoy is delivered.
#[tokio::test(start_paused = true)]
#[ignore = "operator resume is refused: principal:implicit needs the principals model (#3457); awaiting owner ruling"]
async fn on_the_engine_a_stalled_coder_resumes_and_delivers() {
    use flotilla_state_machines::machines::convoy::ConvoyState;

    let fleet = DummyDaemon::start_on_primitives("stall_rate = 1.0\n", "").await;
    fleet.dispatch("engine-resume-proof", CrewMix::Coder).await;
    fleet.wait_for("engine-resume-proof", "stall accounting", |_| fleet.fleet.snapshot().crews_stalled > 0).await;
    let convoy = fleet.wait_for_stall("engine-resume-proof").await;
    let primitives = fleet.daemon.message_primitives().expect("engine path");
    let engine = primitives.convoys().get(&convoy.metadata.name).expect("admitted engine Convoy");
    assert!(matches!(engine.state(), ConvoyState::Open { .. }), "the stalled convoy remains open: {engine:?}");

    let resumed = fleet.execute_as_operator(supervise(&convoy, CrewSupervisionAction::Resume)).await;
    assert_eq!(resumed, CommandValue::Ok, "supervision must resume the engine crew");
    fleet.wait_for_phase("engine-resume-proof", ConvoyPhase::Landed).await;
    let engine = primitives.convoys().get(&convoy.metadata.name).expect("engine Convoy after resume");
    assert!(matches!(engine.state(), ConvoyState::Delivered { .. }), "the engine convoy delivers: {engine:?}");
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
                            matches!(record.state(), LifecycleState::Realised { .. } | LifecycleState::Gone),
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
// must let the same Ensure bind a fresh generation without losing its history.
#[tokio::test(start_paused = true)]
#[ignore = "engine Convoy admission explicitly skips standing convoys pending seat growth/import; awaiting owner ruling"]
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
                .placement_policy(DUMMY_PLACEMENT_POLICY.into())
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
    let abandoned = fleet
        .execute_as_operator(CommandAction::ConvoyAbandon {
            namespace: Some(NAMESPACE.into()),
            name: name.into(),
            reason: "exercise standing re-admission".into(),
        })
        .await;
    assert!(!matches!(abandoned, CommandValue::Error { .. }), "{abandoned:?}");
    let second = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(next) = bound().filter(|next| next != &first) {
                break next;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("engine Ensure binds a fresh convoy after abandon");
    assert_ne!(first, second);
}
