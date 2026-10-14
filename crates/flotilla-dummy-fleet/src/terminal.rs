//! Terminal sessions that host scripted crews in-process. Launching an agent
//! session starts its crew; delivered text becomes the crew's next turn; the
//! rendered screen reports whether the crew is working or idle, which the
//! dummy agent adapter classifies exactly as a real harness's screen would be.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use flotilla_discovery_api::{
    provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement,
};
use flotilla_environment::environment::dummy::DUMMY_FLEET_ENV;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{arg::Arg, TerminalStatus};
use flotilla_provider_runtime::CommandRunner;
use flotilla_terminal::{ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionTag};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    adapter::{DUMMY_SCRIPT_ENV, SCREEN_IDLE, SCREEN_WORKING},
    crew::{Crew, CrewIdentity, CrewScript},
    fleet::{Counter, DummyFleet},
};

/// Registry key of the dummy terminal pool; placement policies name it.
pub const DUMMY_TERMINAL_POOL: &str = "dummy";

pub(crate) struct DummySession {
    role: String,
    command: String,
    cwd: ExecutionEnvironmentPath,
    working: AtomicBool,
    turns: Option<mpsc::UnboundedSender<String>>,
    crew: Mutex<Option<JoinHandle<()>>>,
}

impl DummySession {
    pub(crate) fn set_working(&self, working: bool) {
        self.working.store(working, Ordering::Release);
    }

    fn working(&self) -> bool {
        self.working.load(Ordering::Acquire)
    }

    fn screen(&self) -> String {
        format!("flotilla dummy crew · {} · {}\n", self.role, if self.working() { SCREEN_WORKING } else { SCREEN_IDLE })
    }

    fn stop(&self) {
        if let Some(crew) = self.crew.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            crew.abort();
        }
    }
}

/// Every session the fleet's pools host, by session name. Pools are probed
/// per environment; sessions belong to the fleet so a re-probed pool keeps them.
#[derive(Default)]
pub(crate) struct DummySessions(Mutex<HashMap<String, Arc<DummySession>>>);

impl DummySessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<DummySession>>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    fn get(&self, name: &str) -> Option<Arc<DummySession>> {
        self.lock().get(name).cloned()
    }
}

pub struct DummyTerminalPool {
    fleet: Arc<DummyFleet>,
}

impl DummyTerminalPool {
    fn launch(&self, name: &str, command: &str, cwd: &ExecutionEnvironmentPath, environment: &TerminalEnvVars) -> Result<(), String> {
        let identity = CrewIdentity::from_environment(environment);
        let script = environment.iter().rev().find(|(key, _)| key == DUMMY_SCRIPT_ENV).map(|(_, script)| script.as_str());
        let script = match (&identity, script) {
            (Some(_), Some(script)) => Some(CrewScript::parse(script, self.fleet.config.agent.long_runner_turns)?),
            (Some(_), None) => return Err(format!("session {name} was not launched by the dummy agent adapter")),
            // Tool sessions have no crew; they stay idle until killed.
            (None, _) => None,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let session = Arc::new(DummySession {
            role: identity.as_ref().map_or_else(|| "tool".to_string(), |identity| identity.role.clone()),
            command: command.into(),
            cwd: cwd.clone(),
            // A launched crew works on its launch prompt before it first idles.
            working: AtomicBool::new(identity.is_some()),
            turns: identity.is_some().then_some(sender),
            crew: Mutex::new(None),
        });
        if let (Some(identity), Some(script)) = (identity, script) {
            let crew = Crew::new(Arc::clone(&self.fleet), Arc::clone(&session), identity, receiver);
            *session.crew.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tokio::spawn(crew.run(script)));
        }
        self.fleet.sessions.lock().insert(name.into(), session);
        self.fleet.count(Counter::SessionsLaunched);
        Ok(())
    }
}

#[async_trait]
impl TerminalPool for DummyTerminalPool {
    fn tracks_session_liveness(&self) -> bool {
        true
    }

    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(self
            .fleet
            .sessions
            .lock()
            .iter()
            .map(|(name, session)| {
                TerminalSession::builder()
                    .session_name(name.clone())
                    .status(TerminalStatus::Running)
                    .command(session.command.clone())
                    .working_directory(session.cwd.clone())
                    .screen_activity(if session.working() { ScreenActivity::Active } else { ScreenActivity::Stable })
                    .build()
            })
            .collect())
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        _tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        if self.fleet.sessions.get(session_name).is_some() {
            return Ok(());
        }
        self.launch(session_name, command, cwd, env_vars)
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Err(format!("dummy session {session_name} runs in the daemon and cannot be attached"))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        if let Some(session) = self.fleet.sessions.lock().remove(session_name) {
            session.stop();
        }
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        Ok(self.fleet.sessions.get(session_name).map(|session| session.screen()))
    }

    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        let session = self.fleet.sessions.get(session_name).ok_or_else(|| format!("dummy session {session_name} is not running"))?;
        let turns = session.turns.as_ref().ok_or_else(|| format!("dummy session {session_name} hosts no crew"))?;
        // The crew shows working from the moment its turn is accepted, as a
        // harness does once its composer submits.
        session.set_working(true);
        turns.send(text.to_string()).map_err(|_| format!("dummy crew in {session_name} has stopped"))?;
        self.fleet.count(Counter::TurnsReceived);
        Ok(())
    }
}

/// Provides the dummy pool inside dummy environments only.
pub struct DummyTerminalPoolFactory {
    pub(crate) fleet: Arc<DummyFleet>,
}

#[async_trait]
impl Factory for DummyTerminalPoolFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(
            ProviderCategory::TerminalPool,
            DUMMY_TERMINAL_POOL,
            DUMMY_TERMINAL_POOL,
            "Dummy crews",
            "DC",
            "Terminals",
            "terminal",
        )
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if env.find_env_var(DUMMY_FLEET_ENV).is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar(DUMMY_FLEET_ENV.into())]);
        }
        Ok(Arc::new(DummyTerminalPool { fleet: Arc::clone(&self.fleet) }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use flotilla_config::dummy_fleet::{DummyAgentConfig, DummyFleetConfig};
    use flotilla_protocol::{
        commands::{CommandValue, CrewPromiseOperation, StallProposedDisposition},
        CommandAction,
    };
    use hegel::generators as gs;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{adapter::DummyAgentAdapter, CrewCommandPort, LAST_TURN_MARKER};
    use flotilla_agent::adapter::AgentAdapter;
    use flotilla_resources::TerminalAttentionState;

    const SESSION: &str = "terminal-convoy-a-implement-crew";

    // Stands in for the daemon's command boundary: records each crew command,
    // refuses the first completions, and reports handoffs to the test's peer.
    struct RecordingPort {
        refusals: std::sync::Mutex<usize>,
        handoff_refusals: std::sync::Mutex<usize>,
        commands: mpsc::UnboundedSender<String>,
    }

    #[async_trait]
    impl CrewCommandPort for RecordingPort {
        async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
            let (entry, value) = match action {
                CommandAction::CrewPromise { operation: CrewPromiseOperation::Promise { .. }, .. } => {
                    ("promise".to_string(), CommandValue::Ok)
                }
                CommandAction::CrewPromise { operation: CrewPromiseOperation::Submit { reference, .. }, .. } => {
                    assert!(reference.starts_with("https://forge.dummy.invalid/dummy/fleet/pulls/"), "{reference}");
                    ("submit".to_string(), CommandValue::Ok)
                }
                CommandAction::CrewHandoff { target, message, .. } => {
                    let mut refusals = self.handoff_refusals.lock().expect("refusals");
                    if *refusals > 0 {
                        *refusals -= 1;
                        ("handoff-refused".to_string(), CommandValue::Error { message: "resource conflict".into() })
                    } else {
                        let last = if message.contains(LAST_TURN_MARKER) { "!" } else { "" };
                        (format!("handoff:{target}{last}"), CommandValue::Ok)
                    }
                }
                CommandAction::CrewStall { proposed_disposition, .. } => {
                    let proposal = if proposed_disposition == Some(StallProposedDisposition::Fail) { "fail" } else { "resume" };
                    (format!("stall:{proposal}"), CommandValue::Ok)
                }
                CommandAction::CrewComplete { .. } => {
                    let mut refusals = self.refusals.lock().expect("refusals");
                    if *refusals > 0 {
                        *refusals -= 1;
                        ("complete".to_string(), CommandValue::Error { message: "crew promises are not kept or retracted: pr".into() })
                    } else {
                        ("complete".to_string(), CommandValue::Ok)
                    }
                }
                other => panic!("dummy crews send only crew commands, not {other:?}"),
            };
            self.commands.send(entry).expect("test listens");
            Ok(value)
        }
    }

    fn launch_environment(script: &CrewScript) -> TerminalEnvVars {
        [
            ("FLOTILLA_CREW_ID", "crew-1"),
            ("FLOTILLA_NAMESPACE", "flotilla"),
            ("FLOTILLA_CONVOY", "convoy-a"),
            ("FLOTILLA_VESSEL", "implement"),
            ("FLOTILLA_CREW_ROLE", "crew"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .chain([(DUMMY_SCRIPT_ENV.to_string(), script.render())])
        .collect()
    }

    fn expected_commands(script: &CrewScript, refusals: usize, handoff_refusals: usize, failed: bool, stalled: bool) -> Vec<String> {
        if failed {
            return vec!["stall:fail".into()];
        }
        let mut commands = if stalled { vec!["stall:resume".to_string()] } else { Vec::new() };
        let handoffs_start = commands.len() + if matches!(script, CrewScript::Coder { .. }) { 2 } else { 0 };
        match script {
            CrewScript::Coder { reviewer, .. } => {
                commands.extend(["promise".to_string(), "submit".to_string()]);
                commands.extend(reviewer.iter().map(|reviewer| format!("handoff:{reviewer}!")));
            }
            CrewScript::Responder { reply_to } => commands.extend((0..RESPONDER_TURNS).map(|_| format!("handoff:{reply_to}"))),
            CrewScript::Governor { aide, turns } => {
                commands.extend((1..=*turns).map(|task| format!("handoff:{aide}{}", if task == *turns { "!" } else { "" })))
            }
        }
        // The first handoff is refused `handoff_refusals` times: the crew retries,
        // and stalls for guidance after every third refusal.
        if commands.iter().skip(handoffs_start).any(|command| command.starts_with("handoff:")) {
            let refused = (1..=handoff_refusals).flat_map(|refusal| {
                std::iter::once("handoff-refused".to_string()).chain((refusal % 3 == 0).then(|| "stall:resume".to_string()))
            });
            commands.splice(handoffs_start..handoffs_start, refused);
        }
        commands.extend(std::iter::repeat_n("complete".to_string(), refusals + 1));
        commands
    }

    const RESPONDER_TURNS: usize = 3;

    // Behaviour (#3206): a dummy crew acts only through crew commands, in its
    // script's order. It stalls proposing failure at its failure rate; at its
    // stall rate it stalls proposing resumption and continues after the next
    // turn; it retries completion until its daemon accepts it; it shows as
    // working from a turn's delivery until it has acted, and idle afterwards.
    // The generator spans every script, peers present and absent, zero to two
    // completion refusals, and each failure/stall/clean outcome.
    // Paused time keeps a case to milliseconds, so the property affords more
    // cases than the default profile to reach the rarer script/refusal pairs.
    #[test]
    fn crews_follow_their_script_through_crew_commands() {
        hegel::Hegel::new(crew_follows_its_script).settings(hegel::Settings::new().test_cases(200)).run();
    }

    fn crew_follows_its_script(tc: hegel::TestCase) {
        let script = match tc.draw(gs::integers::<usize>().min_value(0).max_value(3)) {
            0 => CrewScript::Coder { reviewer: None, repository: "dummy/fleet".into() },
            1 => CrewScript::Coder { reviewer: Some("reviewer".into()), repository: "dummy/fleet".into() },
            2 => CrewScript::Responder { reply_to: "coder".into() },
            _ => CrewScript::Governor { aide: "aide".into(), turns: tc.draw(gs::integers::<u32>().min_value(1).max_value(4)) },
        };
        let refusals = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        // Zero to four refusals: below, at and beyond the three attempts before a stall.
        let handoff_refusals = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let failed = tc.draw(gs::booleans());
        let stalled = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
        runtime.block_on(async {
            let agent = DummyAgentConfig {
                turn_delay_ms: 1_500,
                fail_rate: if failed { 1.0 } else { 0.0 },
                stall_rate: if stalled { 1.0 } else { 0.0 },
                long_runner_turns: 1,
            };
            let fleet = DummyFleet::new(DummyFleetConfig { agent, ..DummyFleetConfig::default() });
            let (commands, mut recorded) = mpsc::unbounded_channel();
            fleet
                .attach(Arc::new(RecordingPort {
                    refusals: std::sync::Mutex::new(refusals),
                    handoff_refusals: std::sync::Mutex::new(handoff_refusals),
                    commands,
                }))
                .expect("attach");
            let pool = DummyTerminalPool { fleet: Arc::clone(&fleet) };
            let adapter = DummyAgentAdapter::new(1);
            let screen =
                || async { adapter.classify_screen_attention(&pool.capture_screen(SESSION).await.expect("screen").expect("session")) };
            pool.ensure_session(
                SESSION,
                "flotilla-dummy-crew",
                &ExecutionEnvironmentPath::new("/workspace"),
                &launch_environment(&script),
                &[],
            )
            .await
            .expect("launch");
            assert_eq!(screen().await, Some(TerminalAttentionState::Working), "the launch prompt is the first turn");

            let expected = expected_commands(&script, refusals, handoff_refusals, failed, stalled);
            let mut seen = Vec::new();
            let mut delivered = 0u64;
            let responder = matches!(script, CrewScript::Responder { .. });
            // A responder's peer hands it tasks once it is working; the last one is marked final.
            let hand_tasks = || async {
                for task in 1..=RESPONDER_TURNS {
                    let marker = if task == RESPONDER_TURNS { LAST_TURN_MARKER } else { "" };
                    pool.deliver(SESSION, &format!("[from implement/coder (peer)]\n\nTask {task} {marker}")).await.expect("deliver");
                }
                RESPONDER_TURNS as u64
            };
            if responder && !failed && !stalled {
                delivered += hand_tasks().await;
            }
            while seen.len() < expected.len() {
                let command = tokio::time::timeout(Duration::from_secs(600), recorded.recv()).await.expect("crew acts").expect("port open");
                assert_eq!(command, expected[seen.len()], "command {} after {seen:?}", seen.len());
                let resumed = command == "stall:resume" && seen.is_empty() && stalled;
                let unblocked = command == "stall:resume" && !resumed;
                let peer_turn = resumed || unblocked || (command.starts_with("handoff:") && !responder);
                seen.push(command);
                if peer_turn {
                    // Guidance after a stall, or the peer's answer to a handoff.
                    let turn = if resumed || unblocked {
                        "[from the operator (supervisor)]\n\nCarry on."
                    } else {
                        "[from implement/peer (peer)]\n\nDone."
                    };
                    pool.deliver(SESSION, turn).await.expect("deliver");
                    assert_eq!(screen().await, Some(TerminalAttentionState::Working), "a delivered turn shows as work");
                    delivered += 1;
                }
                if resumed && responder {
                    delivered += hand_tasks().await;
                }
            }
            // Let the crew settle: nothing more is sent, and it shows as idle.
            tokio::time::sleep(Duration::from_secs(120)).await;
            assert!(recorded.try_recv().is_err(), "no command after the script ends");
            assert_eq!(screen().await, Some(TerminalAttentionState::Idle));
            let snapshot = fleet.snapshot();
            assert_eq!(snapshot.turns_received, delivered);
            assert_eq!((snapshot.crews_completed, snapshot.crews_failed), (u64::from(!failed), u64::from(failed)));
            let blocked = expected.iter().filter(|command| *command == "stall:resume").count() as u64;
            assert_eq!(snapshot.crews_stalled, blocked);
            pool.kill_session(SESSION).await.expect("kill");
            assert!(pool.list_sessions().await.expect("sessions").is_empty());
            assert!(pool.deliver(SESSION, "late").await.is_err(), "a killed session refuses turns");
        });
    }
}
