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
use flotilla_terminal::{ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionTag, TerminalSubmission};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    adapter::{DUMMY_SCRIPT_ENV, SCREEN_IDLE, SCREEN_WORKING},
    crew::{Crew, CrewIdentity, CrewScript},
    fleet::{Counter, DummyFleet},
    state::{Event, LaunchedCrew, Outcome},
};

/// Registry key of the dummy terminal pool; placement policies name it.
pub const DUMMY_TERMINAL_POOL: &str = "dummy";

pub(crate) struct DummySession {
    pub(crate) name: String,
    role: String,
    convoy: Option<String>,
    pub(crate) environment: String,
    command: String,
    cwd: ExecutionEnvironmentPath,
    working: AtomicBool,
    /// The crew it hosts, as the state file names it.
    launched: Option<LaunchedCrew>,
    turns: Option<mpsc::UnboundedSender<String>>,
    /// The crew's end of `turns`, until the crew runs.
    receiver: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    record: Mutex<CrewRecord>,
    crew: Mutex<Option<JoinHandle<()>>>,
}

/// What a crew's session has seen: every turn delivered to it and the outcome of every command
/// its crew sent, in order. A crew resumed in another process runs its script again over them.
#[derive(Default)]
pub(crate) struct CrewRecord {
    pub(crate) turns: Vec<DeliveredTurn>,
    pub(crate) outcomes: Vec<Outcome>,
}

/// A turn delivered to a session: its text, and the key it was sent with and when it was
/// accepted, as cleat records a keyed send.
#[derive(Clone)]
pub(crate) struct DeliveredTurn {
    pub(crate) text: String,
    pub(crate) key: Option<String>,
    pub(crate) at_unix_ms: u64,
}

impl DummySession {
    fn new(name: &str, environment: &str, command: String, cwd: ExecutionEnvironmentPath, launched: Option<LaunchedCrew>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            name: name.into(),
            role: launched.as_ref().map_or_else(|| "tool".to_string(), |crew| crew.role.clone()),
            convoy: launched.as_ref().map(|crew| crew.convoy.clone()),
            environment: environment.into(),
            command,
            cwd,
            // A launched crew works on its launch prompt before it first idles.
            working: AtomicBool::new(launched.is_some()),
            turns: launched.is_some().then_some(sender),
            receiver: Mutex::new(launched.is_some().then_some(receiver)),
            launched,
            record: Mutex::default(),
            crew: Mutex::new(None),
        }
    }

    pub(crate) fn record(&self) -> std::sync::MutexGuard<'_, CrewRecord> {
        self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue a turn a previous process delivered.
    pub(crate) fn restore_turn(&self, turn: DeliveredTurn) {
        if let Some(turns) = &self.turns {
            let _ = turns.send(turn.text.clone());
        }
        self.record().turns.push(turn);
    }

    /// When the session accepted a turn sent with `key`, if it did.
    fn accepted(&self, key: &str) -> Option<u64> {
        self.record().turns.iter().find(|turn| turn.key.as_deref() == Some(key)).map(|turn| turn.at_unix_ms)
    }

    /// Start the hosted crew, resuming its script over what the session has recorded.
    fn run_crew(self: &Arc<Self>, fleet: &Arc<DummyFleet>) -> Result<(), String> {
        let Some(launched) = &self.launched else { return Ok(()) };
        let Some(receiver) = self.receiver.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() else { return Ok(()) };
        let script = CrewScript::parse(&launched.script, fleet.config.agent.long_runner_turns)?;
        let replay = self.record().outcomes.iter().cloned().collect();
        let crew = Crew::new(Arc::clone(fleet), Arc::clone(self), CrewIdentity::from(launched), receiver, replay);
        *self.crew.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tokio::spawn(crew.run(script)));
        Ok(())
    }

    pub(crate) fn set_working(&self, working: bool) {
        self.working.store(working, Ordering::Release);
    }

    pub(crate) fn working(&self) -> bool {
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
    pub(crate) fn destroy_environment(&self, fleet: &DummyFleet, environment: &str) {
        fleet.record(&Event::SessionsDestroyed { environment: environment.into() });
        self.lock().retain(|_, session| {
            if session.environment == environment {
                session.stop();
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<DummySession>>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A session a previous process launched, its crew not yet running.
    pub(crate) fn restore(
        &self,
        name: String,
        environment: String,
        command: String,
        cwd: ExecutionEnvironmentPath,
        crew: Option<LaunchedCrew>,
    ) {
        let session = DummySession::new(&name, &environment, command, cwd, crew);
        self.lock().insert(name, Arc::new(session));
    }

    /// Start every crew not yet running: those a previous process hosted.
    pub(crate) fn resume(&self, fleet: &Arc<DummyFleet>) {
        let sessions = self.lock().values().cloned().collect::<Vec<_>>();
        for session in sessions {
            if let Err(error) = session.run_crew(fleet) {
                tracing::warn!(session = %session.name, %error, "dummy crew could not resume");
            }
        }
    }

    /// Stop every crew where it is, as its process's end does.
    pub(crate) fn stop_all(&self) {
        for session in self.lock().values() {
            session.stop();
        }
    }

    /// The events that restore every session: its launch, the turns delivered to it and its
    /// crew's outcomes.
    pub(crate) fn snapshot(&self) -> Vec<Event> {
        let sessions = self.lock();
        let mut names = sessions.keys().collect::<Vec<_>>();
        names.sort();
        let mut events = Vec::new();
        for name in names {
            let session = &sessions[name];
            events.push(Event::Launched {
                session: name.clone(),
                environment: session.environment.clone(),
                command: session.command.clone(),
                cwd: session.cwd.clone(),
                crew: session.launched.clone(),
            });
            let record = session.record();
            events.extend(record.turns.iter().map(|turn| Event::Turn {
                session: name.clone(),
                text: turn.text.clone(),
                key: turn.key.clone(),
                at_unix_ms: turn.at_unix_ms,
            }));
            events.extend(record.outcomes.iter().map(|outcome| Event::Acted { session: name.clone(), outcome: outcome.clone() }));
        }
        events
    }

    pub(crate) fn remaining_for(&self, convoy: &str) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, session)| session.convoy.as_deref() == Some(convoy))
            .map(|(name, _)| format!("live session {name}"))
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<DummySession>> {
        self.lock().get(name).cloned()
    }

    /// End and remove the session of `role`'s crew in convoy `convoy`, a convoy's record name or
    /// one of its launches, returning its name.
    pub(crate) fn end_crew(&self, convoy: &str, role: &str) -> Option<String> {
        let mut sessions = self.lock();
        let name = sessions
            .iter()
            .find(|(_, session)| {
                session.role == role
                    && session.convoy.as_deref().is_some_and(|crew| crew == convoy || flotilla_resources::convoy::launch_of(crew) == convoy)
            })
            .map(|(name, _)| name.clone())?;
        sessions.remove(&name)?.stop();
        Some(name)
    }
}

pub struct DummyTerminalPool {
    fleet: Arc<DummyFleet>,
    environment: String,
}

impl DummyTerminalPool {
    fn launch(&self, name: &str, command: &str, cwd: &ExecutionEnvironmentPath, environment: &TerminalEnvVars) -> Result<(), String> {
        let identity = CrewIdentity::from_environment(environment);
        let script = environment.iter().rev().find(|(key, _)| key == DUMMY_SCRIPT_ENV).map(|(_, script)| script.as_str());
        let launched = match (identity, script) {
            (Some(identity), Some(script)) => {
                // Refuse a script the crew could not run before anything starts.
                CrewScript::parse(script, self.fleet.config.agent.long_runner_turns)?;
                Some(identity.launched(script))
            }
            (Some(_), None) => return Err(format!("session {name} was not launched by the dummy agent adapter")),
            // Tool sessions have no crew; they stay idle until killed.
            (None, _) => None,
        };
        self.fleet.record(&Event::Launched {
            session: name.into(),
            environment: self.environment.clone(),
            command: command.into(),
            cwd: cwd.clone(),
            crew: launched.clone(),
        });
        let session = Arc::new(DummySession::new(name, &self.environment, command.into(), cwd.clone(), launched));
        self.fleet.sessions.lock().insert(name.into(), Arc::clone(&session));
        session.run_crew(&self.fleet)?;
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
        let killed = self.fleet.sessions.lock().remove(session_name);
        if let Some(session) = killed {
            self.fleet.record(&Event::Killed { session: session_name.into() });
            session.stop();
        }
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        Ok(self.fleet.sessions.get(session_name).map(|session| session.screen()))
    }

    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        self.accept(session_name, text, None)
    }

    /// A keyed send, as cleat's (cleat#411): a key the session already accepted writes nothing
    /// and answers when it was first accepted, across a restart of the daemon too.
    async fn guarded_deliver(&self, session_name: &str, text: &str, key: &str) -> Result<TerminalSubmission, String> {
        if let Some(at_unix_ms) = self.fleet.sessions.get(session_name).and_then(|session| session.accepted(key)) {
            return Ok(TerminalSubmission::AlreadySubmitted { at_unix_ms });
        }
        self.accept(session_name, text, Some(key))?;
        Ok(TerminalSubmission::Submitted { keyed: true })
    }
}

impl DummyTerminalPool {
    fn accept(&self, session_name: &str, text: &str, key: Option<&str>) -> Result<(), String> {
        let session = self.fleet.sessions.get(session_name).ok_or_else(|| format!("dummy session {session_name} is not running"))?;
        let turns = session.turns.as_ref().ok_or_else(|| format!("dummy session {session_name} hosts no crew"))?;
        if turns.is_closed() {
            return Err(format!("dummy crew in {session_name} has stopped"));
        }
        // The crew shows working from the moment its turn is accepted, as a
        // harness does once its composer submits.
        session.set_working(true);
        let turn = DeliveredTurn {
            text: text.into(),
            key: key.map(str::to_owned),
            at_unix_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default(),
        };
        self.fleet.record(&Event::Turn {
            session: session_name.into(),
            text: turn.text.clone(),
            key: turn.key.clone(),
            at_unix_ms: turn.at_unix_ms,
        });
        session.record().turns.push(turn);
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
        let environment = env
            .find_env_var("FLOTILLA_ENVIRONMENT_ID")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| vec![UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())])?;
        Ok(Arc::new(DummyTerminalPool { fleet: Arc::clone(&self.fleet), environment: environment.into() }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use flotilla_config::dummy_fleet::{DummyAgentConfig, DummyFleetConfig};
    use flotilla_protocol::{
        commands::{CommandValue, CrewPromiseOperation, CrewSupervisionAction, StallProposedDisposition},
        CommandAction,
    };
    use hegel::generators as gs;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{adapter::DummyAgentAdapter, CrewCommandPort, DummyFleetSnapshot, LAST_TURN_MARKER};
    use flotilla_agent::adapter::AgentAdapter;
    use flotilla_resources::TerminalAttentionState;

    const SESSION: &str = "terminal-convoy-a-implement-crew";

    // Stands in for the daemon's command boundary: records each crew command,
    // delays promise settlement, and reports handoffs to the test's peer.
    struct RecordingPort {
        /// Every submitted change-request reference starts with this.
        reference_prefix: String,
        handoff_refusals: std::sync::Mutex<usize>,
        commands: mpsc::UnboundedSender<String>,
        attention: std::sync::Mutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl CrewCommandPort for RecordingPort {
        async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
            // A crew reads its convoy's role to key its draws; the read is not a crew command.
            if let CommandAction::QueryResourceGet { kind, name, namespace } = &action {
                assert_eq!(kind, "Convoy");
                return Ok(CommandValue::ResourceObject(Box::new(flotilla_protocol::commands::ResourceJsonResponse {
                    kind: kind.clone(),
                    plural: "convoys".into(),
                    namespace: namespace.clone(),
                    value: serde_json::json!({ "metadata": { "name": name }, "spec": { "role": name } }),
                    replica_origin: None,
                })));
            }
            let (entry, value) = match action {
                CommandAction::CrewPromise { operation: CrewPromiseOperation::Promise { .. }, .. } => {
                    ("promise".to_string(), CommandValue::Ok)
                }
                CommandAction::CrewPromise { operation: CrewPromiseOperation::Submit { kind, .. }, .. } if kind == "decision-ledger" => {
                    ("submit-ledger".to_string(), CommandValue::Ok)
                }
                CommandAction::CrewPromise { operation: CrewPromiseOperation::Submit { reference, .. }, .. } => {
                    assert!(reference.starts_with(&self.reference_prefix), "{reference}");
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
                CommandAction::CrewSupervise { convoy, vessel, role, operation: CrewSupervisionAction::Resume, .. } => {
                    (format!("resume:{convoy}/{vessel}/{role}"), CommandValue::Ok)
                }
                CommandAction::ResourceApply { document, .. } => {
                    assert_eq!(document["kind"], "Artifact");
                    ("ledger".to_string(), CommandValue::Ok)
                }
                other => panic!("dummy crews send only crew commands, not {other:?}"),
            };
            self.commands.send(entry).expect("test listens");
            Ok(value)
        }

        async fn report_attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
            assert_eq!(namespace, "flotilla");
            self.attention.lock().expect("attention").push((session.to_string(), working));
            Ok(())
        }
    }

    // Behaviour: a supervisor resumes the stalled crew each supervision turn names, with the
    // command the turn gives, and goes on waiting for the next.
    #[tokio::test(start_paused = true)]
    async fn a_supervisor_resumes_each_stalled_crew_its_turns_name() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let (commands, mut recorded) = mpsc::unbounded_channel();
        let port = Arc::new(RecordingPort {
            reference_prefix: String::new(),
            handoff_refusals: std::sync::Mutex::new(0),
            commands,
            attention: Default::default(),
        });
        fleet.attach(Arc::clone(&port) as Arc<dyn CrewCommandPort>).expect("attach");
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        pool.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&CrewScript::Supervisor), &[])
            .await
            .expect("launch");
        for convoy in ["convoy-b", "convoy-c"] {
            let turn = format!(
                "[from flotilla (supervision)]\n\nSupervise stalled crew coder@work in convoy fleet/{convoy}.\n\
                 `flotilla crew supervise --convoy '{convoy}' --vessel 'work' --role 'coder' resume --message 'guidance'`\n\
                 `flotilla crew supervise --convoy '{convoy}' --vessel 'work' --role 'coder' escalate --message 'guidance'`"
            );
            pool.deliver(SESSION, &turn).await.expect("deliver");
            let acted = tokio::time::timeout(Duration::from_secs(600), recorded.recv()).await.expect("supervisor acts");
            assert_eq!(acted.expect("port open"), format!("resume:{convoy}/work/coder"));
        }
        let after = tokio::time::timeout(Duration::from_secs(600), recorded.recv()).await;
        assert!(after.is_err(), "a supervisor acts only on its turns, and does not end: {after:?}");
        pool.kill_session(SESSION).await.expect("kill");
    }

    // Container destruction removes in-process sessions only in that environment,
    // including idle tool sessions; killing another environment is not cleanup.
    #[tokio::test]
    async fn environment_destruction_removes_only_its_sessions() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let one = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "one".into() };
        let two = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "two".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        one.launch("one-session", "shell", &path, &vec![]).expect("one");
        two.launch("two-session", "shell", &path, &vec![]).expect("two");
        assert_eq!(fleet.snapshot().live_sessions, 2);
        fleet.sessions.destroy_environment(&fleet, "one");
        fleet.sessions.destroy_environment(&fleet, "one");
        assert_eq!(fleet.snapshot().live_sessions, 1);
        assert!(fleet.sessions.get("one-session").is_none());
        assert!(fleet.sessions.get("two-session").is_some());
    }

    fn launch_environment(script: &CrewScript) -> TerminalEnvVars {
        [
            ("FLOTILLA_CREW_ID", "crew-1"),
            ("FLOTILLA_NAMESPACE", "flotilla"),
            ("FLOTILLA_CONVOY", "convoy-a"),
            ("FLOTILLA_VESSEL", "implement"),
            ("FLOTILLA_CREW_ROLE", "crew"),
            ("FLOTILLA_TERMINAL_SESSION", SESSION),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .chain([(DUMMY_SCRIPT_ENV.to_string(), script.render())])
        .collect()
    }

    fn expected_commands(script: &CrewScript, handoff_refusals: usize, failed: bool, stalled: bool) -> Vec<String> {
        if failed {
            return vec!["stall:fail".into()];
        }
        let mut commands = if stalled { vec!["stall:resume".to_string()] } else { Vec::new() };
        let handoffs_start = commands.len() + if matches!(script, CrewScript::Coder { .. }) { 1 } else { 0 };
        match script {
            CrewScript::Coder { reviewer, .. } => {
                commands.push("submit".to_string());
                commands.extend(reviewer.iter().map(|reviewer| format!("handoff:{reviewer}!")));
            }
            CrewScript::Responder { reply_to } => commands.extend((0..RESPONDER_TURNS).map(|_| format!("handoff:{reply_to}"))),
            CrewScript::Governor { aide, turns } => {
                commands.extend((1..=*turns).map(|task| format!("handoff:{aide}{}", if task == *turns { "!" } else { "" })))
            }
            CrewScript::Supervisor => unreachable!("a supervisor never ends; the property draws scripts that do"),
        }
        // The first handoff is refused `handoff_refusals` times: the crew retries,
        // and stalls for guidance after every third refusal.
        if commands.iter().skip(handoffs_start).any(|command| command.starts_with("handoff:")) {
            let refused = (1..=handoff_refusals).flat_map(|refusal| {
                std::iter::once("handoff-refused".to_string()).chain((refusal % 3 == 0).then(|| "stall:resume".to_string()))
            });
            commands.splice(handoffs_start..handoffs_start, refused);
        }
        commands.extend(["submit-ledger".to_string(), "ledger".to_string()]);
        commands
    }

    const RESPONDER_TURNS: usize = 3;

    // Behaviour (#3206): a dummy crew acts only through crew commands, in its
    // script's order. It stalls proposing failure at its failure rate; at its
    // stall rate it stalls proposing resumption and continues after the next
    // turn; it publishes a ledger and waits for promise settlement; it shows as
    // working from a turn's delivery until it has acted, and idle afterwards.
    // The generator spans every script, peers present and absent, zero to two
    // handoff refusals, and each failure/stall/clean outcome.
    // Paused time keeps a case to milliseconds, so the property affords more
    // cases than the default profile to reach the rarer script/refusal pairs.
    #[test]
    fn crews_follow_their_script_through_crew_commands() {
        hegel::Hegel::new(crew_follows_its_script).settings(hegel::Settings::new().test_cases(200)).run();
    }

    fn crew_follows_its_script(tc: hegel::TestCase) {
        let script = match tc.draw(gs::integers::<usize>().min_value(0).max_value(3)) {
            0 => CrewScript::coder(None, "dummy/fleet".into()),
            1 => CrewScript::coder(Some("reviewer".into()), "dummy/fleet".into()),
            2 => CrewScript::Responder { reply_to: "coder".into() },
            _ => CrewScript::Governor { aide: "aide".into(), turns: tc.draw(gs::integers::<u32>().min_value(1).max_value(4)) },
        };
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
            let port = Arc::new(RecordingPort {
                reference_prefix: "https://forge.dummy.invalid/dummy/fleet/pulls/".into(),
                handoff_refusals: std::sync::Mutex::new(handoff_refusals),
                commands,
                attention: Default::default(),
            });
            fleet.attach(Arc::clone(&port) as Arc<dyn CrewCommandPort>).expect("attach");
            let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
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

            let expected = expected_commands(&script, handoff_refusals, failed, stalled);
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
            // Hook-style reports name the crew's session, start working on the
            // launch prompt and end idle, matching the screen.
            let reports = port.attention.lock().expect("attention").clone();
            assert!(reports.iter().all(|(session, _)| session == SESSION));
            assert_eq!((reports.first().map(|report| report.1), reports.last().map(|report| report.1)), (Some(true), Some(false)));
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

    // Behaviour (#3374): with a shared forge configured, a coder opens its
    // change request there over the Forgejo API and submits that pull
    // request's URL. When the forge refuses it (a wrong token), the crew
    // retries, stalls with the reason, and opens the pull request after
    // guidance once the token is right; the forge holds exactly one.
    #[tokio::test(start_paused = true)]
    async fn coders_open_change_requests_on_the_shared_forge() {
        use std::io::Write;

        use flotilla_config::dummy_fleet::DummySharedForgeConfig;
        use flotilla_fake_forge::{FakeForge, ForgeSettings, InProcessClient, RepoRef, SystemClock};

        let fake = Arc::new(FakeForge::new("https://fake-forge.test:3443", Arc::new(SystemClock)));
        fake.add_forge("github", ForgeSettings { token: "right".into(), login: "rehearsal-crew".into() }).expect("forge");
        let mut token = tempfile::NamedTempFile::new().expect("token file");
        write!(token, "wrong").expect("write token");
        let config = DummyFleetConfig {
            agent: DummyAgentConfig { turn_delay_ms: 1_500, fail_rate: 0.0, stall_rate: 0.0, long_runner_turns: 1 },
            shared_forge: Some(DummySharedForgeConfig {
                service_url: fake.service_url("github"),
                endpoint_url: None,
                kind: flotilla_resources::ForgeKind::Forgejo,
                token_path: token.path().to_string_lossy().into_owned(),
                base_branch: "main".into(),
            }),
            ..DummyFleetConfig::default()
        };
        let fleet = DummyFleet::with_http(config, Arc::new(InProcessClient::new(Arc::clone(&fake))));
        let (commands, mut recorded) = mpsc::unbounded_channel();
        let port = Arc::new(RecordingPort {
            reference_prefix: "https://fake-forge.test:3443/github/flotilla-org/flotilla/pulls/".into(),
            handoff_refusals: std::sync::Mutex::new(0),
            commands,
            attention: Default::default(),
        });
        fleet.attach(Arc::clone(&port) as Arc<dyn CrewCommandPort>).expect("attach");
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        let script = CrewScript::coder(None, "flotilla-org/flotilla".into());
        pool.ensure_session(
            SESSION,
            "flotilla-dummy-crew",
            &ExecutionEnvironmentPath::new("/workspace"),
            &launch_environment(&script),
            &[],
        )
        .await
        .expect("launch");
        async fn next(recorded: &mut mpsc::UnboundedReceiver<String>) -> String {
            tokio::time::timeout(Duration::from_secs(600), recorded.recv()).await.expect("crew acts").expect("port open")
        }
        assert_eq!(next(&mut recorded).await, "stall:resume", "a refused open stalls after its retries");
        assert_eq!(fake.refusals().1, 3, "each attempt reached the forge and was refused");
        let repo = RepoRef::new("github", "flotilla-org", "flotilla");
        assert!(fake.repository(&repo).expect("repository").items.is_empty());

        std::fs::write(token.path(), "right\n").expect("fix token");
        pool.deliver(SESSION, "[from the operator (supervisor)]\n\nToken fixed.").await.expect("deliver");
        assert_eq!(next(&mut recorded).await, "submit");
        assert_eq!(next(&mut recorded).await, "submit-ledger");
        assert_eq!(next(&mut recorded).await, "ledger");
        let pull = fake.pull(&repo, 1).expect("the crew's pull request");
        assert_eq!((pull.title.as_str(), pull.user.as_str()), ("Dummy work for convoy-a", "rehearsal-crew"));
        assert_eq!(pull.pull.expect("pull facts").head_branch, "dummy/convoy-a/crew");
        assert_eq!(fleet.snapshot().change_requests_opened, 1);
        pool.kill_session(SESSION).await.expect("kill");
    }

    // Behaviour (A): a fleet opened from the state file of a fleet whose process ended holds what
    // that fleet held: its session, the turns delivered to it, the forge's change requests and
    // its counts. Its crew resumes where it stopped: it sends none of the commands it had sent,
    // and acts on the next turn as it would have.
    #[tokio::test(start_paused = true)]
    async fn a_fleet_opened_from_its_state_file_resumes_where_it_stopped() {
        let state = tempfile::tempdir().expect("state directory");
        let config = DummyFleetConfig {
            agent: DummyAgentConfig { turn_delay_ms: 1_500, fail_rate: 0.0, stall_rate: 0.0, long_runner_turns: 1 },
            ..DummyFleetConfig::default()
        };
        let port = || {
            let (commands, recorded) = mpsc::unbounded_channel();
            let port = RecordingPort {
                reference_prefix: "https://forge.dummy.invalid/dummy/fleet/pulls/".into(),
                handoff_refusals: std::sync::Mutex::new(0),
                commands,
                attention: Default::default(),
            };
            (Arc::new(port), recorded)
        };
        async fn next(recorded: &mut mpsc::UnboundedReceiver<String>) -> String {
            tokio::time::timeout(Duration::from_secs(600), recorded.recv()).await.expect("crew acts").expect("port open")
        }
        let script = CrewScript::coder(Some("reviewer".into()), "dummy/fleet".into());

        let fleet = DummyFleet::open(config.clone(), state.path()).expect("open");
        let (first, mut recorded) = port();
        fleet.attach(first as Arc<dyn CrewCommandPort>).expect("attach");
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        pool.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&script), &[]).await.expect("launch");
        assert_eq!(next(&mut recorded).await, "submit");
        assert_eq!(next(&mut recorded).await, "handoff:reviewer!");
        pool.deliver(SESSION, "[from implement/peer (peer)]\n\nLooks fine.").await.expect("deliver");
        let before = fleet.snapshot();
        // The process ends with the reviewer's answer delivered but not yet acted on.
        fleet.stop();
        drop((pool, fleet));

        let fleet = DummyFleet::open(config, state.path()).expect("reopen");
        assert_eq!(fleet.snapshot(), DummyFleetSnapshot { live_sessions: 1, ..before }, "the counts and the session carry over");
        let (second, mut recorded) = port();
        fleet.attach(second as Arc<dyn CrewCommandPort>).expect("attach");
        assert_eq!(next(&mut recorded).await, "submit-ledger", "the crew acts on the delivered answer, sending nothing twice");
        assert_eq!(next(&mut recorded).await, "ledger");
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(recorded.try_recv().is_err(), "no command after the script ends");
        let (number, _, opened) = fleet.forge.open(&fleet, "dummy/fleet", "dummy/convoy-a/crew", "Convoy A");
        assert_eq!((number, opened), (1, false), "the forge holds the crew's change request");
        assert_eq!(fleet.forge.open(&fleet, "dummy/fleet", "dummy/convoy-b/crew", "Convoy B").0, 2, "and numbers on from it");
        let after = fleet.snapshot();
        assert_eq!((after.change_requests_opened, after.turns_received, after.crews_completed), (1, 1, 1), "{after:?}");
    }
}
