//! Work-credential delivery as one controller per daemon (#3108).
//!
//! The controller keeps each environment's credential files in step with the crews running
//! there, keeps GitHub App tokens fresh, and pushes the central Codex login into crews' homes
//! after it rotates. It runs on events, never on a timer: a crew session's phase changing, an
//! environment's record changing, a project's membership or a credential definition changing,
//! the central Codex login rotating, or a request from crew operations or a person. Time enters
//! only through one deadline: the earliest GitHub App refresh or retry, the earliest retry of a
//! failed environment, or the earliest retry of a Codex login push.
//!
//! Each pass also refreshes the capability cards its events touch: those of the environments it
//! delivered or re-minted for, of a convoy whose account binding changed, and of a convoy whose
//! change touched what its cards read, and of the sessions whose cards read a fulfilment kind whose
//! grants changed, or that read an image build which became built or stopped being built; every
//! card when it delivered every environment. A
//! card's endpoints change only at its session's launch, which renders the card itself.
//!
//! A failed delivery is retried at a backoff a finite number of times, then waits for an event
//! naming its environment. Failures are traced and, for refreshes, told to a person as an
//! attention; nothing about them is stored on the vessel or the environment. A restart rebuilds
//! everything from the start pass, which prepares the running crews' credentials again.
//!
//! Delivery is keyed by environment. A request to deliver one environment (a crew handoff, a
//! person) runs in the requester's task under that environment's lock, never queued behind other
//! environments or a pass; different environments deliver concurrently, the loop's too. Delivering
//! one environment reads only what it needs: its vessels by key and index, the repositories its
//! crews' scopes name by key, and the credential grants held from their watch.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, MutexGuard, Weak,
    },
    time::Duration,
};

use flotilla_credentials::{CredentialStore, GithubAppRefreshWake, GithubAppScope};
use flotilla_crew::crew_capabilities::CardScope;
use flotilla_resources::{
    account_access::AccountChange,
    environment_records::EnvironmentChange,
    image_build_records::ImageBuildChange,
    is_prepared_snapshot,
    vessel_records::{environment_vessels, EnvironmentVessel, EnvironmentVessels, VesselWithoutConvoy},
    CredentialGrant, CredentialGrantSpec, CredentialSpec, FulfilmentGrant, FulfilmentKind, Project, ReadWatchEvent, Repository,
    RepositoryKey, RepositoryTrust, ResourceError, TerminalSession, TerminalSessionPhase, WatchEvent, PLACEMENT_SNAPSHOT_KIND,
};
use flotilla_store::load_metrics::{attributed, inherit, ControllerLoadMetrics};
use futures::{stream, StreamExt};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tracing::{debug, info, info_span, warn, Instrument};

use super::access_layer::redeliver_central_logins;
use super::credentials::{github_app_scope_from_grants, reconcile_credential_refresh_attention_in, redeliver_codex_credentials};
use super::state::ControllerRuntimeState;

/// The kind the loop's metrics are registered under, as the runtime starts it.
const METRICS_KIND: &str = "CredentialDelivery";

/// The backoff after an environment's delivery fails, doubling to the cap.
const RETRY_INITIAL: Duration = Duration::from_secs(30);
const RETRY_MAXIMUM: Duration = Duration::from_secs(120);
/// Consecutive failed deliveries retried at the backoff; past it, the next event naming the
/// environment retries it.
const RETRY_LIMIT: u32 = 5;
/// Consecutive failures reopening a watch before it stops; other events still wake the controller.
const WATCH_RETRY_LIMIT: u32 = 5;
/// Environments one pass delivers at once; each environment's own deliveries stay one at a time.
const CONCURRENT_DELIVERIES: usize = 16;

/// What woke the controller since its last pass.
#[derive(Debug, Default)]
struct Pending {
    /// Every environment: the start, or a watch that reopened and may have missed changes.
    all: bool,
    /// The environments an event named.
    environments: BTreeSet<String>,
    /// The projects whose membership may have changed.
    projects: BTreeSet<String>,
    /// A credential definition changed: every environment, and refreshes that gave up, again.
    definitions: bool,
    /// The central Codex login rotated: every crew's copy follows it.
    codex_rotated: bool,
    /// Capability cards to refresh besides those of the environments delivered.
    cards: CardScope,
}

/// The controller's inbox: the events its sources record, and the way in for the requests crew
/// operations and people send. Held by the runtime state so either can reach the controller.
#[derive(Default)]
pub(super) struct CredentialEvents {
    pending: StdMutex<Pending>,
    wake: Notify,
    /// The deliveries of the controller built over this state; none before it is built or after
    /// it is dropped.
    deliveries: StdMutex<Weak<Deliveries>>,
}

impl CredentialEvents {
    fn record(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        self.wake.notify_one();
    }

    fn take(&self) -> Pending {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// The central Codex login changed on disk; its refresher calls this.
    pub(super) fn codex_rotated(&self) {
        self.record(|pending| pending.codex_rotated = true);
    }

    /// Delivers `environment` now, whatever its backoff, and answers how that went. It runs here,
    /// in the caller's task, waiting only for an earlier delivery to the same environment. Fails
    /// when no controller is built to deliver.
    pub(super) async fn deliver(&self, environment: &str) -> Result<(), String> {
        let deliveries = self
            .deliveries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upgrade()
            .ok_or_else(|| "the credential delivery controller is not running".to_string())?;
        deliveries.request(environment).instrument(info_span!("credential_delivery.request_wait", environment)).await
    }
}

/// An environment's failing delivery: how many attempts in a row, and when the next timed one
/// falls due, if any.
#[derive(Clone, Debug)]
struct Failure {
    attempts: u32,
    next_at: Option<tokio::time::Instant>,
}

impl Failure {
    /// The state after `attempts` failures in a row: retried at the backoff within the limit.
    fn after(attempts: u32, retryable: bool) -> Self {
        let next_at = (retryable && attempts <= RETRY_LIMIT).then(|| {
            let delay = RETRY_INITIAL.saturating_mul(1 << attempts.saturating_sub(1).min(8)).min(RETRY_MAXIMUM);
            tokio::time::Instant::now() + delay
        });
        Self { attempts, next_at }
    }
}

/// One environment's wanted delivery, from the crews standing on it.
#[derive(Debug, Default)]
struct Wanted {
    /// Every credential granted to a crew on it, running or settled: what may need withdrawing.
    granted: BTreeSet<String>,
    /// The credentials the running crews hold.
    running: BTreeSet<String>,
    scopes: BTreeMap<String, BTreeSet<RepositoryKey>>,
    live_scopes: BTreeMap<String, GithubAppScope>,
    permissions: BTreeMap<String, Option<BTreeMap<String, String>>>,
    /// Running crews needing different minted permissions for one credential: a misconfiguration
    /// no retry fixes.
    conflict: Option<String>,
}

/// What delivering an environment needs, shared by the controller's loop and the requests run in
/// requesters' tasks. One environment's deliveries run one at a time under its lock; different
/// environments' run concurrently.
pub(super) struct Deliveries {
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    store: Arc<CredentialStore>,
    events: Arc<CredentialEvents>,
    /// The loop's metrics, which every pass and request counts into.
    metrics: Arc<ControllerLoadMetrics>,
    failures: StdMutex<BTreeMap<String, Failure>>,
    /// Each environment's delivery lock, while some delivery holds or waits for it.
    locks: StdMutex<BTreeMap<String, Weak<AsyncMutex<()>>>>,
    /// The number of each environment's latest delivery, from `sequence`, while a fleet-wide pass
    /// runs: one whose reading is older reads the environment again under its lock.
    delivered: StdMutex<BTreeMap<String, u64>>,
    sequence: AtomicU64,
    every_in_flight: std::sync::atomic::AtomicUsize,
    /// The credential grants by name, as the start's reading or their watch last left them; none
    /// while neither holds them current, as when the watch is closed.
    grants: AsyncMutex<Option<BTreeMap<String, CredentialGrantSpec>>>,
}

fn locked<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) struct CredentialDelivery {
    deliveries: Arc<Deliveries>,
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    store: Arc<CredentialStore>,
    events: Arc<CredentialEvents>,
    /// The Docker environments' Codex login copies failing to follow a rotation.
    codex_failure: Option<Failure>,
    /// When the earliest failed push of the Codex login into a crew's home is retried.
    login_retry_at: Option<tokio::time::Instant>,
    /// The refresh failures last told to the attention reconciler.
    told: Vec<(String, Option<String>, bool)>,
}

impl CredentialDelivery {
    /// The controller over `state`'s credential store; none without one. Requests reach its
    /// deliveries from when it is built until it is dropped.
    pub(super) fn new(state: Arc<ControllerRuntimeState>) -> Option<Self> {
        let store = state.credential_store.clone()?;
        let events = Arc::clone(&state.credential_events);
        let deliveries = Arc::new(Deliveries {
            state: Arc::clone(&state),
            namespace: state.namespace.clone(),
            store: Arc::clone(&store),
            events: Arc::clone(&events),
            metrics: state.controller_metrics.for_kind(METRICS_KIND),
            failures: StdMutex::default(),
            locks: StdMutex::default(),
            delivered: StdMutex::default(),
            sequence: AtomicU64::new(0),
            every_in_flight: std::sync::atomic::AtomicUsize::new(0),
            grants: AsyncMutex::new(None),
        });
        *locked(&events.deliveries) = Arc::downgrade(&deliveries);
        Some(Self {
            deliveries,
            namespace: state.namespace.clone(),
            events,
            state,
            store,
            codex_failure: None,
            login_retry_at: None,
            told: Vec::new(),
        })
    }

    /// The start pass: delivers every environment some vessel stands on or this daemon delivered
    /// to, adopting running crews' GitHub App tokens again, and tells refresh failures.
    pub(super) async fn start(&mut self) -> Result<(), String> {
        // The start's one reading of the grants; their watch keeps them current from `run`.
        match self.deliveries.list_grants().await {
            Ok(listed) => *self.deliveries.grants.lock().await = Some(listed),
            Err(error) => warn!(%error, "credential delivery could not read the credential grants; their watch reads them when it opens"),
        }
        let result = self.deliveries.deliver_every(true).instrument(info_span!("credential_delivery.start")).await;
        self.tell_refresh_failures().await;
        result
    }

    /// Runs on events and the next deadline, after [`CredentialDelivery::start`]. Requests do not
    /// come through here: they deliver in their own tasks and record their cards for the next
    /// pass, which coalesces everything recorded since the last.
    pub(super) async fn run(mut self, metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>) {
        let mut sources = tokio::task::JoinSet::new();
        let backend = self.state.daemon.resource_backend();
        // Each source's reads count for delivery.
        sources.spawn(inherit(watch_sessions(backend.clone(), self.namespace.clone(), Arc::clone(&self.events))));
        sources.spawn(inherit(watch_environments(Arc::clone(&self.state), Arc::clone(&self.events))));
        sources.spawn(inherit(watch_named::<Project>(
            backend.clone(),
            self.namespace.clone(),
            Arc::clone(&self.events),
            |pending, name| {
                pending.projects.insert(name.to_owned());
            },
        )));
        sources.spawn(inherit(watch_named::<CredentialSpec>(
            backend.clone(),
            self.namespace.clone(),
            Arc::clone(&self.events),
            |pending, _| {
                pending.definitions = true;
            },
        )));
        sources.spawn(inherit(watch_grants(backend, Arc::clone(&self.deliveries))));
        sources.spawn(inherit(watch_bindings(Arc::clone(&self.state), self.namespace.clone(), Arc::clone(&self.events))));
        sources.spawn(inherit(watch_fulfilment_kinds(
            self.state.daemon.resource_backend(),
            self.namespace.clone(),
            Arc::clone(&self.events),
        )));
        sources.spawn(inherit(watch_image_builds(Arc::clone(&self.state), Arc::clone(&self.events))));
        loop {
            let deadline = self.next_deadline().await;
            tokio::select! {
                () = self.events.wake.notified() => {}
                () = sleep_until(deadline) => { metrics.requeues.increment(); }
                Some(_) = sources.join_next(), if !sources.is_empty() => { continue; }
            }
            metrics.wakes.increment();
            metrics.woke("pass");
            let started = tokio::time::Instant::now();
            {
                let _busy = metrics.busy();
                if let Err(error) = self.pass().instrument(info_span!("credential_delivery.pass")).await {
                    metrics.failures.increment();
                    debug!(%error, "credential delivery pass left failures");
                }
            }
            metrics.reconcile.record(started.elapsed());
        }
    }

    /// One pass over what woke it and what fell due.
    async fn pass(&mut self) -> Result<(), String> {
        let mut pending = self.events.take();
        let now = tokio::time::Instant::now();
        let mut environments = pending.environments.clone();
        // Environments whose timed retry fell due.
        environments.extend(
            locked(&self.deliveries.failures)
                .iter()
                .filter(|(_, failure)| failure.next_at.is_some_and(|next| next <= now))
                .map(|(name, _)| name.clone()),
        );
        // The cards of every environment an event named or a retry reached, delivered or still in
        // backoff; every card when every environment is delivered or the Codex login rotated.
        let mut cards = std::mem::take(&mut pending.cards);
        cards.every |= pending.all || pending.definitions || pending.codex_rotated;
        cards.environments.extend(environments.iter().cloned());
        // Every environment only for a fleet-wide trigger: the start of a watch that may have
        // missed changes, or a definition changing.
        let result = if pending.all || pending.definitions {
            if pending.definitions {
                locked(&self.deliveries.failures).clear();
            }
            self.deliveries.deliver_every(false).await
        } else if !environments.is_empty() {
            // An event retries an environment that gave up; one in backoff waits for it.
            let due: BTreeSet<String> = {
                let failures = locked(&self.deliveries.failures);
                environments
                    .into_iter()
                    .filter(|environment| failures.get(environment).is_none_or(|failure| failure.next_at.is_none_or(|next| next <= now)))
                    .collect()
            };
            if due.is_empty() {
                Ok(())
            } else {
                self.deliveries.deliver_named(&due).await
            }
        } else {
            Ok(())
        };
        self.follow_codex_login(pending.codex_rotated, now).instrument(info_span!("credential_delivery.codex_login")).await;
        let wake = GithubAppRefreshWake { projects: pending.projects, retry_given_up: pending.definitions };
        let refreshed = self.store.refresh_github_app_tokens(&wake).instrument(info_span!("credential_delivery.github_app_refresh")).await;
        for error in &refreshed.errors {
            warn!(error = %error.message, environment = %error.environment_ref, "failed to refresh GitHub App credential delivery");
        }
        // A re-mint, failed or not, may change what its environment reports delivered.
        cards.environments.extend(refreshed.environments);
        // Cards read committed delivery observations, never the preparation index.
        let span = info_span!("credential_delivery.cards", every = cards.every, environments = cards.environments.len());
        if let Err(error) = self.state.daemon.refresh_capability_cards(&self.namespace, &cards).instrument(span).await {
            warn!(%error, "failed to refresh session capability cards");
        }
        self.tell_refresh_failures().instrument(info_span!("credential_delivery.refresh_failures")).await;
        result
    }

    /// Copies a rotated central Codex login into the Docker environments that read it and
    /// pushes it into crews' homes, or retries the copies and pushes that fell due.
    async fn follow_codex_login(&mut self, rotated: bool, now: tokio::time::Instant) {
        let due = |at: Option<tokio::time::Instant>| at.is_some_and(|at| at <= now);
        if rotated || due(self.codex_failure.as_ref().and_then(|failure| failure.next_at)) {
            match redeliver_codex_credentials(&self.state).await {
                Ok(()) => self.codex_failure = None,
                Err(error) => {
                    // A rotation starts the count again.
                    let attempts = if rotated { 0 } else { self.codex_failure.as_ref().map_or(0, |failure| failure.attempts) };
                    self.codex_failure = Some(Failure::after(attempts + 1, true));
                    warn!(%error, attempts = attempts + 1, "failed to copy the central Codex credential into environments");
                }
            }
        }
        if rotated || due(self.login_retry_at) {
            self.login_retry_at = match redeliver_central_logins(&self.state, &self.namespace, rotated).await {
                Ok(next) => next.and_then(|at| at.to_datetime()).map(|at| {
                    let wait = (at - self.state.daemon.clock().now()).to_std().unwrap_or_default();
                    tokio::time::Instant::now() + wait
                }),
                Err(error) => {
                    // The records could not be read: try once more at the first backoff.
                    warn!(%error, "failed to push the central Codex login into crew homes");
                    rotated.then(|| tokio::time::Instant::now() + RETRY_INITIAL)
                }
            };
        }
    }

    /// The earliest of the next GitHub App refresh or retry, the next environment retry and the
    /// next Codex login retry.
    async fn next_deadline(&self) -> Option<tokio::time::Instant> {
        let refresh = self.store.next_github_app_refresh_at().await.map(|at| {
            let wait = (at - flotilla_resources::clock::now()).to_std().unwrap_or_default();
            tokio::time::Instant::now() + wait
        });
        // Failing environments only: the failures map holds no other.
        locked(&self.deliveries.failures)
            .values()
            .chain(&self.codex_failure)
            .filter_map(|failure| failure.next_at)
            .chain(refresh)
            .chain(self.login_retry_at)
            .min()
    }

    /// Tells the attention reconciler the refresh failures, when they changed.
    async fn tell_refresh_failures(&mut self) {
        let failing = self.store.github_app_refresh_failures().await;
        let told: Vec<_> =
            failing.iter().map(|error| (error.environment_ref.clone(), error.credential_name.clone(), error.should_surface)).collect();
        if told == self.told {
            return;
        }
        match reconcile_credential_refresh_attention_in(
            &*self.state.daemon.attentions(),
            &self.state.daemon.resource_backend(),
            &self.namespace,
            &failing,
        )
        .await
        {
            Ok(()) => self.told = told,
            Err(error) => warn!(%error, "failed to reconcile credential refresh attention"),
        }
    }
}

impl Deliveries {
    /// A request to deliver `environment` now, whatever its backoff, run in the requester's task.
    /// Its cards follow what was delivered at the loop's next pass.
    async fn request(&self, environment: &str) -> Result<(), String> {
        let started = tokio::time::Instant::now();
        self.metrics.wakes.increment();
        self.metrics.woke(environment);
        locked(&self.failures).remove(environment);
        let result = attributed(Arc::clone(&self.metrics.reads), self.deliver_held(environment, None, false)).await;
        self.metrics.reconcile.record(started.elapsed());
        if result.is_err() {
            self.metrics.failures.increment();
        }
        self.events.record(|pending| {
            pending.cards.environments.insert(environment.to_owned());
        });
        result
    }

    /// Delivers each of `environments`, several at once, each read by its key.
    async fn deliver_named(&self, environments: &BTreeSet<String>) -> Result<(), String> {
        let results = stream::iter(environments.iter().cloned())
            .map(|environment| async move { self.deliver_held(&environment, None, false).await })
            .buffer_unordered(CONCURRENT_DELIVERIES)
            .collect::<Vec<_>>()
            .await;
        joined(results)
    }

    /// Delivers every environment some vessel stands on or this daemon delivered to, from one
    /// fleet-wide reading. Only a fleet-wide trigger runs it. `adopt` prepares the running crews'
    /// GitHub App tokens again, as the start pass does after a restart.
    async fn deliver_every(&self, adopt: bool) -> Result<(), String> {
        self.every_in_flight.fetch_add(1, Ordering::SeqCst);
        let read_at = self.sequence.load(Ordering::SeqCst);
        let result = async {
            let wanted = self.wanted(None).instrument(info_span!("credential_delivery.wanted", every = true)).await?;
            let results = stream::iter(wanted)
                .map(|(environment, wanted)| async move { self.deliver_held(&environment, Some((wanted, read_at)), adopt).await })
                .buffer_unordered(CONCURRENT_DELIVERIES)
                .collect::<Vec<_>>()
                .await;
            joined(results)
        }
        .await;
        // The numbers matter only while a fleet-wide reading is in use.
        let mut delivered = locked(&self.delivered);
        if self.every_in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            delivered.clear();
        }
        result
    }

    /// Delivers `environment` under its lock: what `read` holds, when no delivery to it has run
    /// since that reading was taken, else what reading it by its key finds.
    async fn deliver_held(&self, environment: &str, read: Option<(Wanted, u64)>, adopt: bool) -> Result<(), String> {
        let lock = {
            let mut locks = locked(&self.locks);
            match locks.get(environment).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    locks.insert(environment.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        let result = async {
            let _held = lock.lock().instrument(info_span!("credential_delivery.environment_wait")).await;
            let current = read.filter(|(_, read_at)| locked(&self.delivered).get(environment).is_none_or(|at| at <= read_at));
            let wanted = match current {
                Some((wanted, _)) => wanted,
                None => {
                    let named = BTreeSet::from([environment.to_owned()]);
                    match self.wanted(Some(&named)).instrument(info_span!("credential_delivery.wanted", every = false)).await {
                        Ok(mut wanted) => wanted.remove(environment).unwrap_or_default(),
                        Err(error) => {
                            // The records could not be read: the environment is retried at its backoff.
                            self.fail(environment, &error, true);
                            return Err(error);
                        }
                    }
                }
            };
            let delivered = self.deliver_environment(environment, wanted, adopt).await;
            let number = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
            let mut numbers = locked(&self.delivered);
            if self.every_in_flight.load(Ordering::SeqCst) > 0 {
                numbers.insert(environment.to_owned(), number);
            }
            drop(numbers);
            match delivered {
                Ok(()) => {
                    if locked(&self.failures).remove(environment).is_some() {
                        info!(%environment, "credential delivery recovered");
                    }
                    Ok(())
                }
                Err((error, retryable)) => {
                    self.fail(environment, &error, retryable);
                    Err(error)
                }
            }
        }
        .instrument(info_span!("credential_delivery.environment", environment))
        .await;
        // The last holder drops the environment's lock entry.
        let mut locks = locked(&self.locks);
        if Arc::strong_count(&lock) == 1 {
            locks.remove(environment);
        }
        result
    }

    fn fail(&self, environment: &str, error: &str, retryable: bool) {
        let mut failures = locked(&self.failures);
        let attempts = failures.get(environment).map_or(0, |failure| failure.attempts).saturating_add(1);
        let failure = Failure::after(attempts, retryable);
        warn!(%environment, %error, attempts = failure.attempts, retry = failure.next_at.is_some(), "credential delivery failed");
        failures.insert(environment.to_owned(), failure);
    }

    async fn deliver_environment(&self, environment: &str, wanted: Wanted, adopt: bool) -> Result<(), (String, bool)> {
        if let Some(conflict) = wanted.conflict {
            return Err((conflict, false));
        }
        let Wanted { granted, running, scopes, live_scopes, permissions, .. } = wanted;
        let permissions = permissions.into_iter().filter_map(|(name, value)| value.map(|value| (name, value))).collect();
        if granted.is_empty() {
            return Ok(());
        }
        let runner = match self.state.daemon.command_runner_for_environment_ref(environment) {
            Some(runner) => runner,
            None if running.is_empty() => {
                // The environment and its filesystem are gone: forget its registrations and material.
                return self.store.forget_environment(environment).await.map_err(|error| (error, true));
            }
            None => return Err((format!("command runner unavailable for credential delivery to environment {environment}"), true)),
        };
        if !running.is_empty() {
            if adopt {
                self.store
                    .adopt_github_app_deliveries_with_permissions(environment, &running, &scopes, &permissions, runner.clone())
                    .await
                    .map_err(|error| (format!("mint work credentials for environment {environment}: {}", error.message), true))?;
            }
            self.store.set_github_app_scopes(environment, &live_scopes).await;
        }
        self.store
            .reconcile_work_delivery_with_permissions(environment, &granted, &running, &scopes, &permissions, runner)
            .await
            .map_err(|error| (format!("reconcile work credentials for environment {environment}: {error}"), true))
    }

    /// What each environment in `environments` (or every one) wants: the grants of the vessels on
    /// it, and of the running crews among them, from their convoys' frozen requirements.
    async fn wanted(&self, environments: Option<&BTreeSet<String>>) -> Result<BTreeMap<String, Wanted>, String> {
        let namespace = self.namespace.as_str();
        let backend = self.state.daemon.resource_backend();
        let reads = self.state.daemon.lifecycle_reads();
        let named = environments.cloned();
        let EnvironmentVessels { vessels, without_convoy } =
            tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || environment_vessels(reads.as_ref(), named.as_ref())))
                .await
                .map_err(|error| format!("read credential vessels: {error}"))?
                .map_err(|error| format!("read credential vessels: {error}"))?;
        // The other vessels' credentials are still delivered.
        for VesselWithoutConvoy { vessel, convoy, convoy_home } in &without_convoy {
            warn!(%vessel, %convoy, %convoy_home, "credential delivery: the vessel's convoy record is missing; its credentials are left out");
            self.metrics.skipped.increment();
        }
        // The repositories the crews' scopes name, each read by its key, and the grants as held.
        let scoped =
            vessels.iter().flat_map(|vessel| vessel.requirement.credential_scopes.values().flatten()).collect::<BTreeSet<&RepositoryKey>>();
        let (grants, repository_trust) = if vessels.iter().all(|vessel| vessel.requirement.credential_scopes.is_empty()) {
            (Vec::new(), BTreeMap::new())
        } else {
            let grants = self.grants().await?;
            let repositories = backend.including_replicas::<Repository>(namespace);
            let mut repository_trust = BTreeMap::new();
            for key in scoped {
                match repositories.get(&key.0).await {
                    Ok(source) => {
                        let trust = if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own };
                        repository_trust.insert(key.clone(), trust);
                    }
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(format!("read repository `{}` for credential grants: {error}", key.0)),
                }
            }
            (grants, repository_trust)
        };
        let mut wanted = BTreeMap::<String, Wanted>::new();
        for EnvironmentVessel { environment, requirement, project, live, .. } in &vessels {
            let entry = wanted.entry(environment.clone()).or_default();
            entry.granted.extend(requirement.credential_refs.iter().cloned());
            if !*live {
                continue;
            }
            entry.running.extend(requirement.credential_refs.iter().cloned());
            for name in &requirement.credential_refs {
                let incoming = requirement.credential_permissions.get(name).cloned();
                match entry.permissions.get(name) {
                    Some(existing) if existing != &incoming => {
                        entry.conflict =
                            Some(format!("crews sharing environment `{environment}` require different minted permissions for `{name}`"));
                    }
                    Some(_) => {}
                    None => {
                        entry.permissions.insert(name.clone(), incoming);
                    }
                }
            }
            for (name, repositories) in &requirement.credential_scopes {
                entry.scopes.entry(name.clone()).or_default().extend(repositories.iter().cloned());
                let mut scope = github_app_scope_from_grants(
                    name,
                    repositories,
                    project.as_deref(),
                    &requirement.crew.iter().map(|crew| crew.role.clone()).collect(),
                    &repository_trust,
                    requirement.repository_refs.is_some(),
                    &grants,
                );
                scope.permissions = requirement.credential_permissions.get(name).cloned();
                let live_scope = entry.live_scopes.entry(name.clone()).or_default();
                live_scope.fixed_repositories.extend(scope.fixed_repositories);
                live_scope.projects.extend(scope.projects);
                live_scope.permissions = scope.permissions;
            }
        }
        // What this daemon delivered before, so a settled crew's credentials are withdrawn.
        match environments {
            None => {
                for (environment, delivered) in self.store.tracked_work_deliveries().await {
                    wanted.entry(environment).or_default().granted.extend(delivered);
                }
            }
            Some(environments) => {
                for environment in environments {
                    let delivered = self.store.tracked_work_delivery(environment).await;
                    if !delivered.is_empty() {
                        wanted.entry(environment.clone()).or_default().granted.extend(delivered);
                    }
                }
            }
        }
        // An environment named but wanting nothing still answers, as delivered.
        for environment in environments.into_iter().flatten() {
            wanted.entry(environment.clone()).or_default();
        }
        Ok(wanted)
    }

    /// The credential grants as held. None held (the watch not open, and no start reading) fails
    /// the delivery, which its environment's backoff retries: no stale grants, no scan.
    async fn grants(&self) -> Result<Vec<CredentialGrantSpec>, String> {
        let held = self.grants.lock().await;
        let held = held
            .as_ref()
            .ok_or_else(|| "credential grants are not held: the credential grant watch is not open; delivery retries".to_string())?;
        Ok(held.values().cloned().collect())
    }

    async fn list_grants(&self) -> Result<BTreeMap<String, CredentialGrantSpec>, String> {
        Ok(self
            .state
            .daemon
            .resource_backend()
            .including_replicas::<CredentialGrant>(&self.namespace)
            .list()
            .await
            .map_err(|error| format!("list credential grants: {error}"))?
            .items
            .into_iter()
            .map(|grant| (grant.object.metadata.name, grant.object.spec))
            .collect())
    }
}

/// The deliveries' outcome: their failures joined.
fn joined(results: Vec<Result<(), String>>) -> Result<(), String> {
    let errors = results.into_iter().filter_map(Result::err).collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Keeps the held credential grants in step with their changes, local or replicated, and wakes a
/// redelivery of every environment for each. The grants are read whole once each time the watch
/// opens, as it may have missed changes; each change after is applied by its name. While the
/// watch is closed none are held, and it is reopened at a capped backoff for as long as it runs.
async fn watch_grants(backend: flotilla_store::ResourceBackend, deliveries: Arc<Deliveries>) {
    let mut failures = 0_u32;
    // The grants as the watch last saw them, so reopening wakes deliveries only for a change.
    let mut seen = deliveries.grants.lock().await.clone();
    loop {
        match backend.including_replicas::<CredentialGrant>(&deliveries.namespace).watch().await {
            Ok(mut changes) => {
                // Opened after the reading below could miss nothing: the watch holds what follows.
                match deliveries.list_grants().await {
                    Ok(listed) => {
                        let changed = seen.as_ref() != Some(&listed);
                        seen = Some(listed.clone());
                        *deliveries.grants.lock().await = Some(listed);
                        if changed {
                            deliveries.events.record(|pending| pending.definitions = true);
                        }
                    }
                    Err(error) => {
                        // Without a reading the watch cannot keep grants current: reopen it.
                        warn!(%error, "credential delivery could not read the credential grants; reopening their watch");
                        failures = failures.saturating_add(1);
                        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
                        continue;
                    }
                }
                while let Some(change) = changes.next().await {
                    let change = match change {
                        Ok(change) => change,
                        Err(error) => {
                            debug!(%error, "credential delivery grant watch failed");
                            break;
                        }
                    };
                    failures = 0;
                    let held = seen.get_or_insert_default();
                    match change {
                        ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) => {
                            held.insert(read.object.metadata.name, read.object.spec);
                        }
                        ReadWatchEvent::Deleted(read) => {
                            held.remove(&read.object.metadata.name);
                        }
                        ReadWatchEvent::DeletedByName { tombstone, .. } => {
                            held.remove(&tombstone.name);
                        }
                    }
                    *deliveries.grants.lock().await = Some(held.clone());
                    deliveries.events.record(|pending| pending.definitions = true);
                }
            }
            Err(error) => debug!(%error, "credential delivery grant watch did not open"),
        }
        // Closed, changes go unseen: hold no grants until the watch reopens and reads them.
        *deliveries.grants.lock().await = None;
        failures = failures.saturating_add(1);
        if failures == WATCH_RETRY_LIMIT {
            warn!(failures, "credential delivery keeps failing to watch credential grants; deliveries needing them fail until it opens");
        }
        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Names the environment of each crew session at this home whose environment or phase changed.
async fn watch_sessions(backend: flotilla_store::ResourceBackend, namespace: String, events: Arc<CredentialEvents>) {
    let mut seen = BTreeMap::<String, (String, Option<TerminalSessionPhase>)>::new();
    let mut failures = 0_u32;
    loop {
        match backend.using::<TerminalSession>(&namespace).watch(flotilla_resources::WatchStart::Now).await {
            Ok(mut changes) => {
                // A reopened watch may have missed changes: deliver everything again.
                events.record(|pending| pending.all = true);
                while let Some(change) = changes.next().await {
                    let change = match change {
                        Ok(change) => change,
                        Err(error) => {
                            debug!(%error, "credential delivery session watch failed");
                            break;
                        }
                    };
                    failures = 0;
                    let (name, now) = match &change {
                        WatchEvent::Added(object) | WatchEvent::Modified(object) => (
                            object.metadata.name.clone(),
                            Some((object.spec.env_ref.clone(), object.status.as_ref().map(|status| status.phase))),
                        ),
                        WatchEvent::Deleted(object) => (object.metadata.name.clone(), None),
                        WatchEvent::DeletedByName(tombstone) => (tombstone.name.clone(), None),
                    };
                    let before = match &now {
                        Some(now) => seen.insert(name, now.clone()),
                        None => seen.remove(&name),
                    };
                    if before != now {
                        let named: BTreeSet<String> = before.into_iter().chain(now).map(|(environment, _)| environment).collect();
                        events.record(|pending| pending.environments.extend(named));
                    }
                }
            }
            Err(error) => debug!(%error, "credential delivery session watch did not open"),
        }
        failures = failures.saturating_add(1);
        if failures > WATCH_RETRY_LIMIT {
            warn!(failures, "credential delivery stops watching crew sessions; other events still wake it");
            return;
        }
        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
    }
}

/// Names each environment whose record changed, at any home.
async fn watch_environments(state: Arc<ControllerRuntimeState>, events: Arc<CredentialEvents>) {
    let mut changes = match state.daemon.lifecycle_reads().environments() {
        Ok(environments) => environments.changes(),
        Err(error) => {
            warn!(%error, "credential delivery cannot follow environments; other events still wake it");
            return;
        }
    };
    loop {
        match changes.recv().await {
            Ok(EnvironmentChange::Changed(name)) => events.record(|pending| {
                pending.environments.insert(name);
            }),
            Ok(EnvironmentChange::Rescanned) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                events.record(|pending| pending.all = true)
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Names the convoy of each changed account binding: its sessions' cards list their bindings. A
/// watch that reopened may have missed changes, so every card is refreshed.
async fn watch_bindings(state: Arc<ControllerRuntimeState>, namespace: String, events: Arc<CredentialEvents>) {
    let accounts = state.daemon.account_records();
    let mut failures = 0_u32;
    loop {
        match accounts.changes(&namespace).await {
            Ok(mut changes) => {
                events.record(|pending| pending.cards.every = true);
                while let Some(change) = changes.next().await {
                    match change {
                        Ok(AccountChange::Binding { convoy: Some(convoy), .. }) => {
                            failures = 0;
                            events.record(|pending| {
                                pending.cards.convoys.insert(convoy);
                            });
                        }
                        // A binding is ended, which is a change, before it is removed; no card
                        // reads a provision.
                        Ok(_) => failures = 0,
                        Err(error) => {
                            debug!(%error, "credential delivery account binding watch failed");
                            break;
                        }
                    }
                }
            }
            Err(error) => debug!(%error, "credential delivery account binding watch did not open"),
        }
        failures = failures.saturating_add(1);
        if failures > WATCH_RETRY_LIMIT {
            warn!(failures, "credential delivery stops watching account bindings; other events still wake it");
            return;
        }
        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
    }
}

/// Refreshes the cards that read a fulfilment kind whose grants changed, at any home: a card lists
/// the grants of the kind its work's placement decision chose. Admission's placement snapshots
/// are never chosen, so no card reads one: a convoy's admission and teardown, which write and
/// delete its snapshot, wake nothing here.
async fn watch_fulfilment_kinds(backend: flotilla_store::ResourceBackend, namespace: String, events: Arc<CredentialEvents>) {
    // Each live kind's grants as last seen; none until the watch first opens and reads them.
    let mut seen: Option<BTreeMap<String, BTreeSet<FulfilmentGrant>>> = None;
    let mut failures = 0_u32;
    loop {
        match backend.including_replicas::<FulfilmentKind>(&namespace).watch().await {
            Ok(mut changes) => {
                // Opened before the reading, so the watch holds every change after it. A change
                // missed while the watch was closed is a kind whose grants differ from those seen.
                // At the first opening nothing was seen: a card read before it is refreshed by
                // its kind, which names only the cards already read.
                match backend.including_replicas::<FulfilmentKind>(&namespace).list().await {
                    Ok(listed) => {
                        let listed: BTreeMap<String, BTreeSet<FulfilmentGrant>> = listed
                            .items
                            .into_iter()
                            .filter(|kind| !is_prepared_snapshot(&kind.object.metadata.labels, PLACEMENT_SNAPSHOT_KIND))
                            .map(|kind| (kind.object.metadata.name, kind.object.spec.grants))
                            .collect();
                        let changed: BTreeSet<String> = match &seen {
                            Some(before) => {
                                before.keys().chain(listed.keys()).filter(|name| before.get(*name) != listed.get(*name)).cloned().collect()
                            }
                            None => listed.keys().cloned().collect(),
                        };
                        if !changed.is_empty() {
                            events.record(|pending| pending.cards.kinds.extend(changed));
                        }
                        seen = Some(listed);
                    }
                    Err(error) => {
                        debug!(%error, "credential delivery could not read the fulfilment kinds; reopening their watch");
                        failures = failures.saturating_add(1);
                        if failures > WATCH_RETRY_LIMIT {
                            warn!(failures, "credential delivery stops watching fulfilment kinds; other events still wake it");
                            return;
                        }
                        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
                        continue;
                    }
                }
                let held = seen.get_or_insert_default();
                while let Some(change) = changes.next().await {
                    let changed = match change {
                        // A snapshot is never a card's kind.
                        Ok(ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read))
                            if is_prepared_snapshot(&read.object.metadata.labels, PLACEMENT_SNAPSHOT_KIND) =>
                        {
                            None
                        }
                        Ok(ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read)) => {
                            let now = read.object.spec.grants;
                            let name = read.object.metadata.name;
                            held.insert(name.clone(), now.clone()).is_none_or(|before| before != now).then_some(name)
                        }
                        Ok(ReadWatchEvent::Deleted(read)) => {
                            let name = read.object.metadata.name;
                            held.remove(&name).map(|_| name)
                        }
                        // Only a kind held is a live kind: an unheld name is a snapshot's.
                        Ok(ReadWatchEvent::DeletedByName { tombstone, .. }) => held.remove(&tombstone.name).map(|_| tombstone.name),
                        Err(error) => {
                            debug!(%error, "credential delivery fulfilment kind watch failed");
                            break;
                        }
                    };
                    failures = 0;
                    if let Some(kind) = changed {
                        events.record(|pending| {
                            pending.cards.kinds.insert(kind);
                        });
                    }
                }
            }
            Err(error) => debug!(%error, "credential delivery fulfilment kind watch did not open"),
        }
        failures = failures.saturating_add(1);
        if failures > WATCH_RETRY_LIMIT {
            warn!(failures, "credential delivery stops watching fulfilment kinds; other events still wake it");
            return;
        }
        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
    }
}

/// Names each image build whose launches changed: a card lists a built image's verified tools.
/// The refresh reads only the cards that read that build, and refreshes those that found it built
/// when it no longer is, or the reverse; a build no card read is not read at all. A rescan, or
/// changes missed, judge every build some card read.
async fn watch_image_builds(state: Arc<ControllerRuntimeState>, events: Arc<CredentialEvents>) {
    let mut changes = match state.daemon.lifecycle_reads().image_builds() {
        Ok(builds) => builds.changes(),
        Err(error) => {
            warn!(%error, "credential delivery cannot follow image builds; other events still wake it");
            return;
        }
    };
    loop {
        match changes.recv().await {
            Ok(ImageBuildChange::Changed(name)) => events.record(|pending| {
                pending.cards.image_builds.insert(name);
            }),
            Ok(ImageBuildChange::Rescanned) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                events.record(|pending| pending.cards.image_builds_rescanned = true)
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Records each change to an `R`, local or replicated, by its name.
async fn watch_named<R: flotilla_resources::Resource>(
    backend: flotilla_store::ResourceBackend,
    namespace: String,
    events: Arc<CredentialEvents>,
    record: fn(&mut Pending, &str),
) {
    let mut failures = 0_u32;
    loop {
        match backend.including_replicas::<R>(&namespace).watch().await {
            Ok(mut changes) => {
                while let Some(change) = changes.next().await {
                    match change {
                        Ok(change) => {
                            failures = 0;
                            let name = match &change {
                                ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read) => {
                                    read.object.metadata.name.as_str()
                                }
                                ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name.as_str(),
                            };
                            events.record(|pending| record(pending, name));
                        }
                        Err(error) => {
                            debug!(kind = R::API_PATHS.kind, %error, "credential delivery watch failed");
                            break;
                        }
                    }
                }
            }
            Err(error) => debug!(kind = R::API_PATHS.kind, %error, "credential delivery watch did not open"),
        }
        failures = failures.saturating_add(1);
        if failures > WATCH_RETRY_LIMIT {
            warn!(kind = R::API_PATHS.kind, failures, "credential delivery stops watching this kind; other events still wake it");
            return;
        }
        tokio::time::sleep(RETRY_INITIAL.min(Duration::from_secs(u64::from(failures)))).await;
    }
}
