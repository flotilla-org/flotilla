//! The fleet hub shared by every dummy provider a daemon installs.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_config::dummy_fleet::DummyFleetConfig;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{commands::CommandValue, query::CrewCommandContext, Command, CommandAction, DaemonEvent};
use flotilla_provider_runtime::{HttpClient, ReqwestHttpClient};
use tokio::sync::{broadcast::error::RecvError, oneshot};

use crate::{catalog::CrewMix, forge::DummyForge, shared_forge::SharedForge, terminal::DummySessions, vcs::DummyCheckouts};

/// How a dummy crew reaches its daemon: the same commands a real crew sends
/// through `flotilla crew …`, without a socket or a process.
#[async_trait]
pub trait CrewCommandPort: Send + Sync {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String>;

    /// Observe settlement before attempting completion, as a real crew checks
    /// its submitted promises. Boundary doubles without a store are ready.
    async fn completion_ready(&self, _context: &CrewCommandContext) -> Result<bool, String> {
        Ok(true)
    }

    /// Report the crew working or idle in its terminal session, as a real
    /// harness's hooks do. Ports without a hook path ignore it; the session's
    /// screen still shows the same state.
    async fn report_attention(&self, _namespace: &str, _session: &str, _working: bool) -> Result<(), String> {
        Ok(())
    }
}

/// One daemon's dummy fleet: its settings, the forge its crews open change
/// requests on, the sessions its terminal pool hosts and its counters.
pub struct DummyFleet {
    pub(crate) config: DummyFleetConfig,
    pub(crate) forge: DummyForge,
    /// Where coders open change requests instead of `forge`, when configured.
    pub(crate) shared_forge: Option<Arc<SharedForge>>,
    pub(crate) sessions: DummySessions,
    pub(crate) checkouts: DummyCheckouts,
    port: OnceLock<Arc<dyn CrewCommandPort>>,
    counters: Counters,
    vessel_names: BTreeSet<String>,
    command_error_samples: Mutex<Vec<String>>,
}

impl DummyFleet {
    /// A fleet whose shared forge, if configured, is reached over HTTPS.
    pub fn new(config: DummyFleetConfig) -> Arc<Self> {
        let http = config.shared_forge.is_some().then(|| Arc::new(ReqwestHttpClient::new()) as Arc<dyn HttpClient>);
        Self::build(config, http)
    }

    /// A fleet whose shared forge, if configured, is reached through `http`.
    pub fn with_http(config: DummyFleetConfig, http: Arc<dyn HttpClient>) -> Arc<Self> {
        Self::build(config, Some(http))
    }

    fn build(config: DummyFleetConfig, http: Option<Arc<dyn HttpClient>>) -> Arc<Self> {
        let shared_forge = config.shared_forge.clone().zip(http).map(|(shared, http)| Arc::new(SharedForge::new(shared, http)));
        let vessel_names = CrewMix::ALL
            .into_iter()
            .flat_map(|mix| mix.workflow(config.agent.long_runner_turns).vessels)
            .map(|vessel| vessel.name)
            .collect();
        Arc::new(Self {
            vessel_names,
            forge: DummyForge::new(&config),
            shared_forge,
            sessions: DummySessions::default(),
            checkouts: DummyCheckouts::default(),
            config,
            port: OnceLock::new(),
            counters: Counters::default(),
            command_error_samples: Mutex::default(),
        })
    }

    pub fn config(&self) -> &DummyFleetConfig {
        &self.config
    }

    /// Connect crews to their daemon. The composition root calls this once,
    /// after constructing the daemon whose discovery installed this fleet.
    pub fn attach(&self, port: Arc<dyn CrewCommandPort>) -> Result<(), String> {
        self.port.set(port).map_err(|_| "dummy fleet is already attached to a daemon".to_string())
    }

    pub(crate) async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        let port = self.port.get().ok_or("dummy fleet is not attached to a daemon")?;
        // Keep only a static command kind across the ownership boundary. Format
        // diagnostics only for retained failures; successful commands allocate no diagnostic strings.
        let command = match &action {
            CommandAction::CrewPromise { .. } => "CrewPromise",
            CommandAction::CrewComplete { .. } => "CrewComplete",
            CommandAction::CrewHandoff { .. } => "CrewHandoff",
            CommandAction::CrewStall { .. } => "CrewStall",
            CommandAction::CrewFail { .. } => "CrewFail",
            _ => "dummy crew command",
        };
        let result = port.execute(action).await;
        match &result {
            Ok(CommandValue::Error { .. }) | Err(_) => {
                self.count(Counter::CommandErrors);
                let mut samples = self.command_error_samples.lock().expect("command errors");
                if samples.len() < 5 {
                    samples.push(format!("{command}: {result:?}"));
                }
            }
            Ok(_) => {}
        }
        match result {
            Ok(CommandValue::Error { message }) => Err(message),
            result => result,
        }
    }

    pub(crate) async fn completion_ready(&self, context: &CrewCommandContext) -> Result<bool, String> {
        self.port.get().ok_or("dummy fleet is not attached to a daemon")?.completion_ready(context).await
    }

    pub(crate) async fn report_attention(&self, namespace: &str, session: &str, working: bool) {
        let Some(port) = self.port.get() else { return };
        if let Err(error) = port.report_attention(namespace, session, working).await {
            tracing::debug!(%session, %error, "dummy crew attention report failed");
        }
    }

    pub(crate) fn count(&self, counter: Counter) {
        self.counters.get(counter).fetch_add(1, Ordering::Relaxed);
    }

    /// Provider-side artifacts survive resource deletion if teardown is broken.
    pub fn remaining_for(&self, convoy: &str) -> Vec<String> {
        let mut remaining = self.sessions.remaining_for(convoy);
        // The catalogue defines complete environment identities. Prefix matching
        // cannot separate a convoy from another whose name extends it with '-'.
        let environments = self.vessel_names.iter().map(|vessel| format!("env-{convoy}-{vessel}")).collect::<HashSet<_>>();
        remaining.extend(self.checkouts.remaining_for(convoy, &environments));
        remaining
    }

    /// Bounded diagnostic evidence for command errors, retained for load reports.
    pub fn command_error_samples(&self) -> Vec<String> {
        self.command_error_samples.lock().expect("command errors").clone()
    }

    pub fn snapshot(&self) -> DummyFleetSnapshot {
        let read = |counter| self.counters.get(counter).load(Ordering::Relaxed);
        DummyFleetSnapshot {
            sessions_launched: read(Counter::SessionsLaunched),
            turns_received: read(Counter::TurnsReceived),
            crews_completed: read(Counter::CrewsCompleted),
            crews_failed: read(Counter::CrewsFailed),
            crews_stalled: read(Counter::CrewsStalled),
            change_requests_opened: read(Counter::ChangeRequestsOpened),
            command_errors: read(Counter::CommandErrors),
            live_sessions: self.sessions.len() as u64,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Counter {
    SessionsLaunched,
    TurnsReceived,
    CrewsCompleted,
    CrewsFailed,
    CrewsStalled,
    ChangeRequestsOpened,
    CommandErrors,
}

#[derive(Default)]
struct Counters([AtomicU64; 7]);

impl Counters {
    fn get(&self, counter: Counter) -> &AtomicU64 {
        &self.0[counter as usize]
    }
}

/// Monotonic counters since the fleet started, plus live sessions now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DummyFleetSnapshot {
    pub sessions_launched: u64,
    pub turns_received: u64,
    pub crews_completed: u64,
    pub crews_failed: u64,
    pub crews_stalled: u64,
    pub change_requests_opened: u64,
    pub command_errors: u64,
    pub live_sessions: u64,
}

/// Results unclaimed for this long belong to other callers and are dropped.
const UNCLAIMED_RESULT_TTL: Duration = Duration::from_secs(60);

/// A command that finishes later than this is reported to the crew as lost.
const COMMAND_RESULT_TIMEOUT: Duration = Duration::from_secs(120);

enum Slot {
    Waiting(oneshot::Sender<CommandValue>),
    Finished(Box<CommandValue>, tokio::time::Instant),
}

/// [`CrewCommandPort`] over a daemon handle. One listener routes every
/// `CommandFinished` event to its waiting crew, so a thousand crews share one
/// event subscription. It holds the daemon weakly: the daemon owns this port
/// through its discovery factories.
pub struct DaemonCommandPort {
    daemon: Weak<dyn DaemonHandle>,
    slots: Arc<Mutex<HashMap<u64, Slot>>>,
}

impl DaemonCommandPort {
    pub fn new(daemon: &Arc<dyn DaemonHandle>) -> Arc<Self> {
        let slots = Arc::new(Mutex::new(HashMap::new()));
        let mut events = daemon.subscribe();
        let routed = Arc::downgrade(&slots);
        tokio::spawn(async move {
            loop {
                let event = events.recv().await;
                let Some(slots) = routed.upgrade() else { return };
                match event {
                    Ok(DaemonEvent::CommandFinished { command_id, result, .. }) => {
                        let mut slots = lock(&slots);
                        let now = tokio::time::Instant::now();
                        slots.retain(|_, slot| !matches!(slot, Slot::Finished(_, at) if now.duration_since(*at) > UNCLAIMED_RESULT_TTL));
                        match slots.remove(&command_id) {
                            Some(Slot::Waiting(waiter)) => {
                                let _ = waiter.send(result);
                            }
                            _ => {
                                slots.insert(command_id, Slot::Finished(Box::new(result), now));
                            }
                        }
                    }
                    Ok(_) => {}
                    // A lagged listener loses results; their crews time out
                    // and report the command as lost rather than hanging.
                    Err(RecvError::Lagged(skipped)) => tracing::warn!(skipped, "dummy fleet command listener lagged"),
                    Err(RecvError::Closed) => return,
                }
            }
        });
        Arc::new(Self { daemon: Arc::downgrade(daemon), slots })
    }
}

fn lock(slots: &Mutex<HashMap<u64, Slot>>) -> std::sync::MutexGuard<'_, HashMap<u64, Slot>> {
    slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl CrewCommandPort for DaemonCommandPort {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        let daemon = self.daemon.upgrade().ok_or("daemon has stopped")?;
        let command_id = daemon.execute(Command::builder().action(action).build()).await?;
        drop(daemon);
        let (sender, receiver) = oneshot::channel();
        {
            let mut slots = lock(&self.slots);
            match slots.remove(&command_id) {
                Some(Slot::Finished(result, _)) => return Ok(*result),
                _ => {
                    slots.insert(command_id, Slot::Waiting(sender));
                }
            }
        }
        match tokio::time::timeout(COMMAND_RESULT_TIMEOUT, receiver).await {
            Ok(Ok(result)) => Ok(result),
            _ => {
                lock(&self.slots).remove(&command_id);
                Err(format!("result of command {command_id} was lost"))
            }
        }
    }
}
