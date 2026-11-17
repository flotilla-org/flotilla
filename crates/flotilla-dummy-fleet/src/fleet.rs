//! The fleet hub shared by every dummy provider a daemon installs.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_config::dummy_fleet::DummyFleetConfig;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_environment::{
    environment::dummy::{
        DummyEnvironmentBehaviour, DummyEnvironmentCleanup, DummyEnvironmentCounts, DummyVendorJournal, DUMMY_CLOUD_ENVIRONMENT_INSTANCE,
        DUMMY_ENVIRONMENT_INSTANCE,
    },
    factories::dummy::DummyEnvironmentFactory,
};
use flotilla_protocol::{commands::CommandValue, Command, CommandAction, DaemonEvent};
use flotilla_provider_runtime::{HttpClient, ReqwestHttpClient};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast::error::RecvError, oneshot};

use crate::{
    catalog::CrewMix,
    forge::DummyForge,
    shared_forge::SharedForge,
    state::{self, Event, Journal},
    terminal::DummySessions,
    vcs::DummyCheckouts,
};

/// How a dummy crew reaches its daemon: the same commands a real crew sends
/// through `flotilla crew …`, without a socket or a process.
#[async_trait]
pub trait CrewCommandPort: Send + Sync {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String>;

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
    pub(crate) vendor: DummyVendor,
    pub(crate) checkouts: DummyCheckouts,
    /// The state file this fleet appends its changes to, once restored from it.
    journal: Journal,
    port: OnceLock<Arc<dyn CrewCommandPort>>,
    counters: Counters,
    vessel_names: BTreeSet<String>,
    command_error_samples: Mutex<Vec<String>>,
}

impl DummyFleet {
    /// A fleet whose shared forge, if configured, is reached over HTTPS. Nothing it holds
    /// outlives it.
    pub fn new(config: DummyFleetConfig) -> Arc<Self> {
        let http = config.shared_forge.is_some().then(|| Arc::new(ReqwestHttpClient::new()) as Arc<dyn HttpClient>);
        Self::build(config, http)
    }

    /// The fleet whose state file is in `state_dir`: it holds what the fleet there held when
    /// its process ended, and records its changes there. Its crews resume once it is attached.
    pub fn open(config: DummyFleetConfig, state_dir: &Path) -> Result<Arc<Self>, String> {
        let fleet = Self::new(config);
        std::fs::create_dir_all(state_dir).map_err(|error| format!("create {}: {error}", state_dir.display()))?;
        let path = state_dir.join(state::STATE_FILE);
        for event in state::load(&path)? {
            fleet.restore(event);
        }
        fleet.journal.open(&path, &fleet.events())?;
        Ok(fleet)
    }

    /// A fleet whose shared forge, if configured, is reached through `http`.
    pub fn with_http(config: DummyFleetConfig, http: Arc<dyn HttpClient>) -> Arc<Self> {
        Self::build(config, Some(http))
    }

    fn build(config: DummyFleetConfig, http: Option<Arc<dyn HttpClient>>) -> Arc<Self> {
        let shared_forge = config.shared_forge.clone().zip(http).map(|(shared, http)| Arc::new(SharedForge::new(shared, http)));
        // Allocation names each vessel after a role it holds.
        let vessel_names =
            CrewMix::ALL.into_iter().flat_map(|mix| mix.workflow(config.agent.long_runner_turns).roles).map(|crew| crew.role).collect();
        Arc::new_cyclic(|fleet: &Weak<Self>| {
            let behaviour = DummyEnvironmentBehaviour {
                seed: config.seed,
                realise_delay: Duration::from_millis(config.environment.realise_delay_ms),
                failure_rate: config.environment.failure_rate,
            };
            // A backing's destruction ends the sessions and checkouts inside it.
            let cleanup: DummyEnvironmentCleanup = {
                let fleet = Weak::clone(fleet);
                Arc::new(move |id| {
                    if let Some(fleet) = fleet.upgrade() {
                        fleet.sessions.destroy_environment(&fleet, id);
                        fleet.checkouts.destroy_environment(&fleet, id);
                    }
                })
            };
            let journal = |instance: &'static str| -> DummyVendorJournal {
                let fleet = Weak::clone(fleet);
                Arc::new(move |change| {
                    if let Some(fleet) = fleet.upgrade() {
                        fleet.record(&Event::Vendor { instance: instance.into(), change });
                    }
                })
            };
            let factories = [
                DummyEnvironmentFactory::new(behaviour.clone(), Arc::clone(&cleanup), journal(DUMMY_ENVIRONMENT_INSTANCE)),
                DummyEnvironmentFactory::cloud(behaviour, cleanup, journal(DUMMY_CLOUD_ENVIRONMENT_INSTANCE)),
            ];
            Self {
                vessel_names,
                forge: DummyForge::new(&config),
                shared_forge,
                sessions: DummySessions::default(),
                vendor: DummyVendor { factories },
                checkouts: DummyCheckouts::default(),
                journal: Journal::default(),
                config,
                port: OnceLock::new(),
                counters: Counters::default(),
                command_error_samples: Mutex::default(),
            }
        })
    }

    pub fn config(&self) -> &DummyFleetConfig {
        &self.config
    }

    /// Connect crews to their daemon, and resume those a previous process's fleet hosted. The
    /// composition root calls this once, after constructing the daemon whose discovery
    /// installed this fleet.
    pub fn attach(self: &Arc<Self>, port: Arc<dyn CrewCommandPort>) -> Result<(), String> {
        self.port.set(port).map_err(|_| "dummy fleet is already attached to a daemon".to_string())?;
        self.sessions.resume(self);
        Ok(())
    }

    /// End this fleet as its process's end does: its crews stop where they are, and nothing
    /// more is recorded. The fleet [opened](Self::open) from its state file next takes over.
    pub fn stop(&self) {
        self.journal.close();
        self.sessions.stop_all();
    }

    pub(crate) fn record(&self, event: &Event) {
        self.journal.record(event);
    }

    /// Apply an event a previous process's fleet recorded.
    fn restore(&self, event: Event) {
        match event {
            Event::Vendor { instance, change } => match self.vendor.factories.iter().find(|vendor| vendor.instance() == instance) {
                Some(vendor) => vendor.restore(change),
                None => tracing::warn!(%instance, "dummy fleet state names an unknown vendor"),
            },
            Event::ChangeRequest { repository, number, branch, title, opened_at, head_sha } => {
                self.forge.restore(repository, number, branch, title, opened_at, head_sha)
            }
            Event::Checkout { environment, path, branch } => {
                self.checkouts.lock().insert((environment, path), branch);
            }
            Event::CheckoutRemoved { environment, path } => {
                self.checkouts.lock().remove(&(environment, path));
            }
            Event::CheckoutsDestroyed { environment } => self.checkouts.lock().retain(|(owner, _), _| *owner != environment),
            Event::Launched { session, environment, command, cwd, crew } => self.sessions.restore(session, environment, command, cwd, crew),
            Event::Killed { session } => {
                self.sessions.lock().remove(&session);
            }
            Event::SessionsDestroyed { environment } => self.sessions.lock().retain(|_, session| session.environment != environment),
            Event::Turn { session, text, key, at_unix_ms } => match self.sessions.get(&session) {
                Some(session) => session.restore_turn(crate::terminal::DeliveredTurn { text, key, at_unix_ms }),
                None => tracing::warn!(%session, "dummy fleet state delivers a turn to an unknown session"),
            },
            Event::Acted { session, outcome } => {
                if let Some(session) = self.sessions.get(&session) {
                    session.record().outcomes.push(outcome);
                }
            }
            Event::Counted { counter } => {
                self.counters.get(counter).fetch_add(1, Ordering::Relaxed);
            }
            Event::Counts { counts } => {
                for (counter, count) in counts {
                    self.counters.get(counter).fetch_add(count, Ordering::Relaxed);
                }
            }
        }
    }

    /// The fewest events that restore what this fleet holds now.
    fn events(&self) -> Vec<Event> {
        let counts = Counter::ALL.into_iter().map(|counter| (counter, self.counters.get(counter).load(Ordering::Relaxed))).collect();
        let mut events = vec![Event::Counts { counts }];
        for vendor in &self.vendor.factories {
            events.extend(vendor.snapshot().into_iter().map(|change| Event::Vendor { instance: vendor.instance().into(), change }));
        }
        events.extend(self.forge.snapshot());
        let checkouts = self
            .checkouts
            .lock()
            .iter()
            .map(|((environment, path), branch)| (environment.clone(), path.clone(), branch.clone()))
            .collect::<BTreeSet<(String, PathBuf, String)>>();
        events.extend(checkouts.into_iter().map(|(environment, path, branch)| Event::Checkout { environment, path, branch }));
        events.extend(self.sessions.snapshot());
        events
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

    pub(crate) async fn report_attention(&self, namespace: &str, session: &str, working: bool) {
        let Some(port) = self.port.get() else { return };
        if let Err(error) = port.report_attention(namespace, session, working).await {
            tracing::debug!(%session, %error, "dummy crew attention report failed");
        }
    }

    pub(crate) fn count(&self, counter: Counter) {
        self.counters.get(counter).fetch_add(1, Ordering::Relaxed);
        self.record(&Event::Counted { counter });
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

    /// The environment vendors, as a test acts as them.
    /// End the process of `role`'s crew in convoy `convoy`, as a harness that exits or crashes
    /// does: its session is gone from the pool. False when no such crew runs.
    pub fn end_crew(&self, convoy: &str, role: &str) -> bool {
        let Some(name) = self.sessions.end_crew(convoy, role) else { return false };
        self.record(&Event::Killed { session: name });
        true
    }

    pub fn vendor(&self) -> &DummyVendor {
        &self.vendor
    }

    pub fn snapshot(&self) -> DummyFleetSnapshot {
        let read = |counter| self.counters.get(counter).load(Ordering::Relaxed);
        let environments = self.vendor.counts();
        DummyFleetSnapshot {
            environments_provisioned: environments.provisioned,
            environments_destroyed: environments.destroyed,
            environments_vanished: environments.vanished,
            live_environments: environments.live,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Counter {
    SessionsLaunched,
    TurnsReceived,
    CrewsCompleted,
    CrewsFailed,
    CrewsStalled,
    ChangeRequestsOpened,
    CommandErrors,
}

impl Counter {
    const ALL: [Self; 7] = [
        Self::SessionsLaunched,
        Self::TurnsReceived,
        Self::CrewsCompleted,
        Self::CrewsFailed,
        Self::CrewsStalled,
        Self::ChangeRequestsOpened,
        Self::CommandErrors,
    ];
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
    /// Backings the environment vendors realised.
    pub environments_provisioned: u64,
    /// Backings destroyed on the daemon's request.
    pub environments_destroyed: u64,
    /// Backings a vendor deleted of its own accord.
    pub environments_vanished: u64,
    /// Backings the vendors hold now.
    pub live_environments: u64,
}

/// The container and cloud-sandbox environment vendors. Every install of the
/// fleet installs them, and the state file carries their backings to the next
/// process's fleet, as a vendor's backings outlive a daemon. Acting as the
/// vendor changes only what its provider reports.
pub struct DummyVendor {
    pub(crate) factories: [DummyEnvironmentFactory; 2],
}

impl DummyVendor {
    fn each(&self) -> impl Iterator<Item = &DummyEnvironmentFactory> {
        self.factories.iter()
    }

    /// Delete `environment`'s backing, by Environment name or backing ID; its
    /// crews' processes end with it. False when no vendor holds it.
    pub fn vanish(&self, environment: &str) -> bool {
        self.each().any(|vendor| vendor.vanish(environment))
    }

    /// Stop `environment`'s backing. False when no vendor holds it running.
    pub fn stop(&self, environment: &str) -> bool {
        self.each().any(|vendor| vendor.stop(environment))
    }

    /// While set, refuse every new backing.
    pub fn refuse_provisioning(&self, refuse: bool) {
        self.each().for_each(|vendor| vendor.refuse_provisioning(refuse));
    }

    fn counts(&self) -> DummyEnvironmentCounts {
        self.each().map(DummyEnvironmentFactory::counts).fold(DummyEnvironmentCounts::default(), |sum, counts| DummyEnvironmentCounts {
            provisioned: sum.provisioned + counts.provisioned,
            destroyed: sum.destroyed + counts.destroyed,
            vanished: sum.vanished + counts.vanished,
            live: sum.live + counts.live,
        })
    }
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
                    // The daemon stopped: its commands' results are lost now, as a closed socket
                    // tells a client at once, rather than when their wait times out.
                    Err(RecvError::Closed) => {
                        lock(&slots).clear();
                        return;
                    }
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
        // A query answers at once, as a client's read does; only commands run asynchronously.
        if action.is_query() {
            return daemon.execute_query(Command::builder().action(action).build(), uuid::Uuid::new_v4()).await;
        }
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
