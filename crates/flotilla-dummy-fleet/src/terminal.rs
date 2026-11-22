//! Terminal sessions that host scripted crews in-process. Launching an agent
//! session starts its crew; delivered text becomes the crew's next turn; the
//! rendered screen reports whether the crew is working or idle, which the
//! dummy agent adapter classifies exactly as a real harness's screen would be.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use flotilla_discovery_api::{
    provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement,
};
use flotilla_environment::environment::dummy::{dummy_draw, DUMMY_FLEET_ENV};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{arg::Arg, TerminalStatus};
use flotilla_provider_runtime::CommandRunner;
use flotilla_terminal::{
    ScreenActivity, TerminalEnvVars, TerminalPool, TerminalSession, TerminalSessionLiveness, TerminalSessionTag, TerminalSubmission,
};
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

    /// Where the session's command runs.
    pub(crate) fn cwd(&self) -> &ExecutionEnvironmentPath {
        &self.cwd
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

    /// Abort the crew; the handle it returns ends once the crew has.
    fn stop(&self) -> Option<JoinHandle<()>> {
        let crew = self.crew.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
        crew.abort();
        Some(crew)
    }
}

/// Every session the fleet's pools host, by session name. Pools are probed
/// per environment; sessions belong to the fleet so a re-probed pool keeps them.
#[derive(Default)]
pub(crate) struct DummySessions {
    live: Mutex<HashMap<String, Arc<DummySession>>>,
    /// The environments whose backings were destroyed: cleat cannot start a session inside a
    /// container that is gone, so a launch there is refused.
    destroyed: Mutex<HashSet<String>>,
    /// The sessions their pool lost: cleat's daemon reports each absent, not exited.
    lost: Mutex<HashSet<String>>,
}

impl DummySessions {
    pub(crate) fn destroy_environment(&self, fleet: &DummyFleet, environment: &str) {
        fleet.record(&Event::SessionsDestroyed { environment: environment.into() });
        self.destroyed(environment);
        self.lock().retain(|_, session| {
            if session.environment == environment {
                session.stop();
                false
            } else {
                true
            }
        });
    }

    /// `environment`'s backing is gone, as a previous process's fleet recorded: its sessions
    /// went with it, and none starts there again.
    pub(crate) fn restore_destroyed(&self, environment: String) {
        self.lock().retain(|_, session| session.environment != environment);
        self.destroyed(&environment);
    }

    fn destroyed(&self, environment: &str) {
        self.destroyed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(environment.to_owned());
    }

    /// `name` was lost from its pool, as a previous process's fleet recorded.
    pub(crate) fn restore_lost(&self, name: String) {
        self.lock().remove(&name);
        self.lost.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(name);
    }

    /// Whether the pool lost `name`.
    fn is_lost(&self, name: &str) -> bool {
        self.lost.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(name)
    }

    /// Whether `environment`'s backing was destroyed.
    fn is_destroyed(&self, environment: &str) -> bool {
        self.destroyed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(environment)
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<DummySession>>> {
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Stop every crew where it is, as its process's end does. Each returned handle ends once its
    /// crew has stopped.
    pub(crate) fn stop_all(&self) -> Vec<JoinHandle<()>> {
        self.lock().values().filter_map(|session| session.stop()).collect()
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
        let mut destroyed: Vec<String> = self.destroyed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect();
        destroyed.sort();
        events.extend(destroyed.into_iter().map(|environment| Event::SessionsDestroyed { environment }));
        let mut lost: Vec<String> = self.lost.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect();
        lost.sort();
        events.extend(lost.into_iter().map(|session| Event::Lost { session }));
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

    /// Lose the session of `role`'s crew in convoy `convoy` from its pool, as a cleat daemon that
    /// no longer has it does, returning its name.
    pub(crate) fn lose_crew(&self, convoy: &str, role: &str) -> Option<String> {
        let name = self.end_crew(convoy, role)?;
        self.lost.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(name.clone());
        Some(name)
    }
}

pub struct DummyTerminalPool {
    fleet: Arc<DummyFleet>,
    environment: String,
}

impl DummyTerminalPool {
    /// The session `name` as this pool's environment sees it: cleat inside one container
    /// reaches only that container's sessions, so another environment's is absent here.
    fn own(&self, name: &str) -> Option<Arc<DummySession>> {
        self.fleet.sessions.get(name).filter(|session| session.environment == self.environment)
    }

    fn launch(&self, name: &str, command: &str, cwd: &ExecutionEnvironmentPath, environment: &TerminalEnvVars) -> Result<(), String> {
        if self.fleet.sessions.is_destroyed(&self.environment) {
            return Err(format!("session {name} cannot start: environment {} is gone", self.environment));
        }
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

    /// A session that ended reads as stopped, as an exited process does; one its pool lost reads
    /// as absent, as cleat reports a session its daemon no longer has.
    async fn session_liveness(&self, session_id: &str) -> Result<TerminalSessionLiveness, String> {
        Ok(if self.own(session_id).is_some() {
            TerminalSessionLiveness::Running
        } else if self.fleet.sessions.is_lost(session_id) {
            TerminalSessionLiveness::Absent
        } else {
            TerminalSessionLiveness::Stopped
        })
    }

    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(self
            .fleet
            .sessions
            .lock()
            .iter()
            .filter(|(_, session)| session.environment == self.environment)
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
        if self.own(session_name).is_some() {
            return Ok(());
        }
        self.launch(session_name, command, cwd, env_vars)
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: Option<&str>,
        _cwd: &ExecutionEnvironmentPath,
        _env: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Err(format!("dummy session {session_name} runs in the daemon and cannot be attached"))
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        let killed = {
            let mut sessions = self.fleet.sessions.lock();
            match sessions.get(session_name) {
                Some(session) if session.environment == self.environment => sessions.remove(session_name),
                _ => None,
            }
        };
        if let Some(session) = killed {
            self.fleet.record(&Event::Killed { session: session_name.into() });
            session.stop();
        }
        Ok(())
    }

    async fn capture_screen(&self, session_name: &str) -> Result<Option<String>, String> {
        Ok(self.own(session_name).map(|session| session.screen()))
    }

    /// An unkeyed send: cleat's pool answers its refusal as an error.
    async fn deliver(&self, session_name: &str, text: &str) -> Result<(), String> {
        if let Some(reason) = self.refusal() {
            return Err(reason);
        }
        self.accept(session_name, text, None)
    }

    /// A keyed send, as cleat's (cleat#411): a key the session already accepted writes nothing
    /// and answers when it was first accepted, across a restart of the daemon too; a refused
    /// send writes nothing.
    async fn guarded_deliver(&self, session_name: &str, text: &str, key: &str) -> Result<TerminalSubmission, String> {
        if let Some(at_unix_ms) = self.own(session_name).and_then(|session| session.accepted(key)) {
            return Ok(TerminalSubmission::AlreadySubmitted { at_unix_ms });
        }
        if let Some(reason) = self.refusal() {
            return Ok(TerminalSubmission::Refused(reason));
        }
        self.accept(session_name, text, Some(key))?;
        Ok(TerminalSubmission::Submitted { keyed: true })
    }
}

impl DummyTerminalPool {
    /// Cleat's pre-write refusal of this submission, at the configured rate: the composer held
    /// text when the send looked. Every attempt draws anew.
    fn refusal(&self) -> Option<String> {
        let rate = self.fleet.config.terminal.refusal_rate;
        if rate <= 0.0 {
            return None;
        }
        let attempt = self.fleet.submissions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dummy_draw(self.fleet.config.seed, "delivery-refusal", &attempt.to_string()) >= rate {
            return None;
        }
        self.fleet.count(Counter::SubmissionsRefused);
        Some("not-matched: composer[value=''] did not match\n".into())
    }

    fn accept(&self, session_name: &str, text: &str, key: Option<&str>) -> Result<(), String> {
        let session = self.own(session_name).ok_or_else(|| format!("dummy session {session_name} is not running"))?;
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
            at_unix_ms: u64::try_from(flotilla_resources::clock::now().timestamp_millis()).unwrap_or_default(),
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

    use flotilla_config::dummy_fleet::{DummyAgentConfig, DummyFleetConfig};
    use hegel::generators as gs;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        fake_daemon::{FakeDaemon, HOOK_TOKEN, HOOK_URL},
        DummyFleetSnapshot, LAST_TURN_MARKER,
    };

    const SESSION: &str = "terminal-convoy-a-implement-crew";

    // Behaviour: a supervisor resumes the stalled crew each supervision turn names, with the
    // command the turn gives, and goes on waiting for the next.
    #[tokio::test]
    async fn a_supervisor_resumes_each_stalled_crew_its_turns_name() {
        let fleet = DummyFleet::new(DummyFleetConfig { agent: quick_crews(), ..DummyFleetConfig::default() });
        let (_daemon, mut recorded) = FakeDaemon::serve(&fleet, "env-a", "", 0).await;
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
            assert_eq!(next(&mut recorded).await, format!("resume:{convoy}/work/coder"));
        }
        settle(&fleet).await;
        let after = recorded.try_recv();
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

    // As cleat in a container that is gone: once an environment's backing is destroyed, no
    // session starts in it again, while another environment's pool still launches.
    #[tokio::test]
    async fn a_destroyed_environment_starts_no_session() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let one = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "one".into() };
        let two = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "two".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        one.ensure_session("before", "shell", &path, &vec![], &[]).await.expect("launched before");
        fleet.sessions.destroy_environment(&fleet, "one");
        let refused = one.ensure_session("after", "shell", &path, &vec![], &[]).await;
        assert!(refused.is_err(), "{refused:?}");
        assert!(fleet.sessions.get("after").is_none());
        two.ensure_session("elsewhere", "shell", &path, &vec![], &[]).await.expect("another environment launches");
        assert_eq!(fleet.snapshot().sessions_launched, 2);
    }

    // As cleat inside one container: a pool lists, captures, sends to and kills only its own
    // environment's sessions; another environment's session is absent to it.
    #[tokio::test]
    async fn a_pool_reaches_only_its_own_environments_sessions() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let one = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "one".into() };
        let two = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "two".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        one.ensure_session("one-session", "shell", &path, &vec![], &[]).await.expect("one");
        two.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&CrewScript::Supervisor), &[]).await.expect("two");

        let listed = |sessions: Vec<TerminalSession>| sessions.into_iter().map(|session| session.session_name).collect::<Vec<_>>();
        assert_eq!(listed(one.list_sessions().await.expect("list one")), vec!["one-session".to_string()]);
        assert_eq!(listed(two.list_sessions().await.expect("list two")), vec![SESSION.to_string()]);
        assert_eq!(one.capture_screen(SESSION).await.expect("capture"), None);
        assert!(one.deliver(SESSION, "turn").await.is_err(), "another environment's session is not running here");
        assert!(one.guarded_deliver(SESSION, "turn", "key").await.is_err());
        one.kill_session(SESSION).await.expect("kill finds nothing here");
        assert!(two.capture_screen(SESSION).await.expect("capture").is_some(), "the other environment's session lives on");
        assert_eq!(fleet.snapshot().turns_received, 0);
        two.kill_session(SESSION).await.expect("kill");
    }

    // At the configured rate the pool refuses a submission as cleat does, before writing: a keyed
    // send answers `Refused` and an unkeyed one an error, the crew receives nothing and the key is
    // not taken, so the same key's retry is submitted once.
    #[tokio::test]
    async fn the_pool_refuses_submissions_at_the_configured_rate_without_writing() {
        let mut config = DummyFleetConfig::default();
        config.terminal.refusal_rate = 1.0;
        let fleet = DummyFleet::new(config);
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        pool.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&CrewScript::Supervisor), &[])
            .await
            .expect("launch");

        let refused = pool.guarded_deliver(SESSION, "turn", "key-1").await.expect("answered");
        assert!(matches!(&refused, TerminalSubmission::Refused(reason) if reason.starts_with("not-matched:")), "{refused:?}");
        assert!(pool.deliver(SESSION, "turn").await.is_err(), "an unkeyed refusal is an error, as cleat's pool reports it");
        assert_eq!((fleet.snapshot().turns_received, fleet.snapshot().submissions_refused), (0, 2));
        pool.kill_session(SESSION).await.expect("kill");

        // At a partial rate each attempt draws anew: retrying one key is refused some times,
        // then submitted once, and is already submitted after that.
        let mut config = DummyFleetConfig::default();
        config.terminal.refusal_rate = 0.5;
        let fleet = DummyFleet::new(config);
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        pool.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&CrewScript::Supervisor), &[])
            .await
            .expect("launch");
        let mut attempts = 0;
        loop {
            attempts += 1;
            match pool.guarded_deliver(SESSION, "turn", "key-1").await.expect("answered") {
                TerminalSubmission::Refused(_) => assert!(attempts < 64, "a retry goes through"),
                TerminalSubmission::Submitted { keyed: true } => break,
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(pool.guarded_deliver(SESSION, "turn", "key-1").await, Ok(TerminalSubmission::AlreadySubmitted { .. })));
        let snapshot = fleet.snapshot();
        assert_eq!((snapshot.turns_received, snapshot.submissions_refused), (1, attempts - 1));
        pool.kill_session(SESSION).await.expect("kill");
    }

    // The crews reach the stand-in over real sockets, so these tests run in real time: paused
    // time would advance to the next timer while a reply is still in the socket. Short turns
    // and retries keep them quick.
    fn quick_crews() -> DummyAgentConfig {
        DummyAgentConfig { turn_delay_ms: 10, retry_backoff_ms: 1, long_runner_turns: 1, ..DummyAgentConfig::default() }
    }

    /// Long enough for a crew to act on a turn; only reached when it does not.
    const ACT_WITHIN: Duration = Duration::from_secs(60);

    /// The next command the stand-in records.
    async fn next(recorded: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(ACT_WITHIN, recorded.recv()).await.expect("crew acts").expect("port open")
    }

    /// Wait until the session shows idle, then a few turns' length more, after which a crew
    /// with nothing to act on has sent everything it will.
    async fn settle(fleet: &DummyFleet) {
        let idle = async {
            while fleet.sessions.get(SESSION).is_some_and(|session| session.working()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ACT_WITHIN, idle).await.expect("the crew goes idle");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fn launch_environment(script: &CrewScript) -> TerminalEnvVars {
        [
            ("FLOTILLA_CREW_ID", "crew-1"),
            ("FLOTILLA_NAMESPACE", "flotilla"),
            ("FLOTILLA_CONVOY", "convoy-a"),
            ("FLOTILLA_VESSEL", "implement"),
            ("FLOTILLA_CREW_ROLE", "crew"),
            ("FLOTILLA_TERMINAL_SESSION", SESSION),
            (flotilla_agent::CLAUDE_CODE_HOOK_URL_ENV, HOOK_URL),
            (flotilla_agent::CLAUDE_CODE_HOOK_TOKEN_ENV, HOOK_TOKEN),
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
    // Short turns keep a case to a fraction of a second, so the property affords more
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
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let agent = DummyAgentConfig {
                fail_rate: if failed { 1.0 } else { 0.0 },
                stall_rate: if stalled { 1.0 } else { 0.0 },
                ..quick_crews()
            };
            let fleet = DummyFleet::new(DummyFleetConfig { agent, ..DummyFleetConfig::default() });
            let (daemon, mut recorded) =
                FakeDaemon::serve(&fleet, "env-a", "https://forge.dummy.invalid/dummy/fleet/pulls/", handoff_refusals).await;
            let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
            let working = || fleet.sessions.get(SESSION).expect("session").working();
            pool.ensure_session(
                SESSION,
                "flotilla-dummy-crew",
                &ExecutionEnvironmentPath::new("/workspace"),
                &launch_environment(&script),
                &[],
            )
            .await
            .expect("launch");
            assert!(working(), "the launch prompt is the first turn");

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
                let command = next(&mut recorded).await;
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
                    assert!(working(), "a delivered turn shows as work");
                    delivered += 1;
                }
                if resumed && responder {
                    delivered += hand_tasks().await;
                }
            }
            // Let the crew settle: nothing more is sent, and it shows as idle.
            settle(&fleet).await;
            assert!(recorded.try_recv().is_err(), "no command after the script ends");
            assert!(!working());
            // Claude Code's hooks: the session starts as a command hook, then the launch prompt is
            // submitted, tool calls pair pre and post within turns, and the crew ends stopped.
            let hooks = daemon.hooks.lock().expect("hooks").clone();
            assert_eq!(hooks.first().map(String::as_str), Some("command:SessionStart"), "{hooks:?}");
            assert_eq!(hooks.get(1).map(String::as_str), Some("user-prompt-submit"), "{hooks:?}");
            assert_eq!(hooks.last().map(String::as_str), Some("stop"), "{hooks:?}");
            let prompts = hooks.iter().filter(|hook| *hook == "user-prompt-submit").count();
            let stops = hooks.iter().filter(|hook| *hook == "stop").count();
            assert_eq!(prompts, stops, "each prompt's turn stops: {hooks:?}");
            assert!(hooks.iter().skip(1).all(|hook| !hook.starts_with("command:")), "HTTP hooks: {hooks:?}");
            // Each connection claimed the crew its launch environment names.
            let claims = daemon.claims.lock().expect("claims").clone();
            assert!(!claims.is_empty());
            assert!(claims.iter().all(|claims| claims.get("FLOTILLA_CREW_ID").map(String::as_str) == Some("crew-1")
                && claims.get("FLOTILLA_TERMINAL_SESSION").map(String::as_str) == Some(SESSION)));
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
    #[tokio::test]
    async fn coders_open_change_requests_on_the_shared_forge() {
        use std::io::Write;

        use flotilla_config::dummy_fleet::DummySharedForgeConfig;
        use flotilla_fake_forge::{FakeForge, ForgeSettings, InProcessClient, RepoRef, SystemClock};

        let fake = Arc::new(FakeForge::new("https://fake-forge.test:3443", Arc::new(SystemClock)));
        fake.add_forge("github", ForgeSettings { token: "right".into(), login: "rehearsal-crew".into() }).expect("forge");
        let mut token = tempfile::NamedTempFile::new().expect("token file");
        write!(token, "wrong").expect("write token");
        let config = DummyFleetConfig {
            agent: quick_crews(),
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
        let (_daemon, mut recorded) =
            FakeDaemon::serve(&fleet, "env-a", "https://fake-forge.test:3443/github/flotilla-org/flotilla/pulls/", 0).await;
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
    #[tokio::test]
    async fn a_fleet_opened_from_its_state_file_resumes_where_it_stopped() {
        let state = tempfile::tempdir().expect("state directory");
        let config = DummyFleetConfig { agent: quick_crews(), ..DummyFleetConfig::default() };
        const PULLS: &str = "https://forge.dummy.invalid/dummy/fleet/pulls/";
        let script = CrewScript::coder(Some("reviewer".into()), "dummy/fleet".into());

        let fleet = DummyFleet::open(config.clone(), state.path()).expect("open");
        let (_first, mut recorded) = FakeDaemon::serve(&fleet, "env-a", PULLS, 0).await;
        let pool = DummyTerminalPool { fleet: Arc::clone(&fleet), environment: "env-a".into() };
        let path = ExecutionEnvironmentPath::new("/workspace");
        pool.ensure_session(SESSION, "flotilla-dummy-crew", &path, &launch_environment(&script), &[]).await.expect("launch");
        assert_eq!(next(&mut recorded).await, "submit");
        assert_eq!(next(&mut recorded).await, "handoff:reviewer!");
        // The crew has the handoff's result and waits for the reviewer.
        settle(&pool.fleet).await;
        pool.deliver(SESSION, "[from implement/peer (peer)]\n\nLooks fine.").await.expect("deliver");
        let before = fleet.snapshot();
        // The process ends with the reviewer's answer delivered but not yet acted on.
        fleet.stop();
        drop((pool, fleet));

        let fleet = DummyFleet::open(config, state.path()).expect("reopen");
        assert_eq!(fleet.snapshot(), DummyFleetSnapshot { live_sessions: 1, ..before }, "the counts and the session carry over");
        let (_second, mut recorded) = FakeDaemon::serve(&fleet, "env-a", PULLS, 0).await;
        fleet.resume();
        assert_eq!(next(&mut recorded).await, "submit-ledger", "the crew acts on the delivered answer, sending nothing twice");
        assert_eq!(next(&mut recorded).await, "ledger");
        settle(&fleet).await;
        assert!(recorded.try_recv().is_err(), "no command after the script ends");
        let (number, _, opened) = fleet.forge.open(&fleet, "dummy/fleet", "dummy/convoy-a/crew", "Convoy A");
        assert_eq!((number, opened), (1, false), "the forge holds the crew's change request");
        assert_eq!(fleet.forge.open(&fleet, "dummy/fleet", "dummy/convoy-b/crew", "Convoy B").0, 2, "and numbers on from it");
        let after = fleet.snapshot();
        assert_eq!((after.change_requests_opened, after.turns_received, after.crews_completed), (1, 1, 1), "{after:?}");
    }
}
