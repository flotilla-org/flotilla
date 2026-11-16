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
//! change touched what its cards read; every card when it delivered every environment, a
//! fulfilment kind's grants changed, or an image build became built or stopped being built. A
//! card's endpoints change only at its session's launch, which renders the card itself.
//!
//! A failed delivery is retried at a backoff a finite number of times, then waits for an event
//! naming its environment. Failures are traced and, for refreshes, told to a person as an
//! attention; nothing about them is stored on the vessel or the environment. A restart rebuilds
//! everything from the start pass, which prepares the running crews' credentials again.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use flotilla_credentials::{CredentialStore, GithubAppRefreshWake, GithubAppScope};
use flotilla_crew::crew_capabilities::CardScope;
use flotilla_resources::{
    account_access::AccountChange,
    environment_records::EnvironmentChange,
    image_build_records::ImageBuildChange,
    vessel_records::{environment_vessels, EnvironmentVessel, EnvironmentVessels, VesselWithoutConvoy},
    CredentialGrant, CredentialSpec, FulfilmentGrant, FulfilmentKind, Project, ReadWatchEvent, Repository, RepositoryKey, RepositoryTrust,
    TerminalSession, TerminalSessionPhase, WatchEvent,
};
use flotilla_store::load_metrics::ControllerLoadMetrics;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{debug, info, warn};

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

/// A request to deliver one environment now, answered once it has been tried.
struct Request {
    environment: String,
    reply: oneshot::Sender<Result<(), String>>,
}

/// The controller's inbox: the events its sources record, and the requests crew operations and
/// people send. Held by the runtime state so either can reach the controller.
pub(super) struct CredentialEvents {
    pending: StdMutex<Pending>,
    wake: Notify,
    requests: mpsc::UnboundedSender<Request>,
    receiver: StdMutex<Option<mpsc::UnboundedReceiver<Request>>>,
}

impl Default for CredentialEvents {
    fn default() -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        Self { pending: StdMutex::default(), wake: Notify::new(), requests, receiver: StdMutex::new(Some(receiver)) }
    }
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

    /// Delivers `environment` now, whatever its backoff, and answers how that went. Fails when no
    /// controller is running to answer.
    pub(super) async fn deliver(&self, environment: &str) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request { environment: environment.to_owned(), reply })
            .map_err(|_| "the credential delivery controller is not running".to_string())?;
        answer.await.map_err(|_| "the credential delivery controller stopped before answering".to_string())?
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

pub(super) struct CredentialDelivery {
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    store: Arc<CredentialStore>,
    events: Arc<CredentialEvents>,
    failures: BTreeMap<String, Failure>,
    /// The Docker environments' Codex login copies failing to follow a rotation.
    codex_failure: Option<Failure>,
    /// When the earliest failed push of the Codex login into a crew's home is retried.
    login_retry_at: Option<tokio::time::Instant>,
    /// The refresh failures last told to the attention reconciler.
    told: Vec<(String, Option<String>, bool)>,
    /// The loop's metrics, which every pass, the start pass too, counts into.
    metrics: Arc<ControllerLoadMetrics>,
}

impl CredentialDelivery {
    /// The controller over `state`'s credential store; none without one.
    pub(super) fn new(state: Arc<ControllerRuntimeState>) -> Option<Self> {
        let store = state.credential_store.clone()?;
        Some(Self {
            metrics: state.controller_metrics.for_kind(METRICS_KIND),
            namespace: state.namespace.clone(),
            events: Arc::clone(&state.credential_events),
            state,
            store,
            failures: BTreeMap::new(),
            codex_failure: None,
            login_retry_at: None,
            told: Vec::new(),
        })
    }

    /// The start pass: delivers every environment some vessel stands on or this daemon delivered
    /// to, adopting running crews' GitHub App tokens again, and tells refresh failures.
    pub(super) async fn start(&mut self) -> Result<(), String> {
        let result = self.deliver(None, true).await;
        self.tell_refresh_failures().await;
        result
    }

    /// Runs on events and the next deadline, after [`CredentialDelivery::start`].
    pub(super) async fn run(mut self, metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>) {
        let Some(mut requests) = self.events.receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() else {
            warn!("credential delivery is already running");
            return;
        };
        let mut sources = tokio::task::JoinSet::new();
        let backend = self.state.daemon.resource_backend();
        sources.spawn(watch_sessions(backend.clone(), self.namespace.clone(), Arc::clone(&self.events)));
        sources.spawn(watch_environments(Arc::clone(&self.state), Arc::clone(&self.events)));
        sources.spawn(watch_named::<Project>(backend.clone(), self.namespace.clone(), Arc::clone(&self.events), |pending, name| {
            pending.projects.insert(name.to_owned());
        }));
        sources.spawn(watch_named::<CredentialSpec>(backend.clone(), self.namespace.clone(), Arc::clone(&self.events), |pending, _| {
            pending.definitions = true;
        }));
        sources.spawn(watch_named::<CredentialGrant>(backend, self.namespace.clone(), Arc::clone(&self.events), |pending, _| {
            pending.definitions = true;
        }));
        sources.spawn(watch_bindings(Arc::clone(&self.state), self.namespace.clone(), Arc::clone(&self.events)));
        sources.spawn(watch_fulfilment_kinds(self.state.daemon.resource_backend(), self.namespace.clone(), Arc::clone(&self.events)));
        sources.spawn(watch_image_builds(Arc::clone(&self.state), Arc::clone(&self.events)));
        loop {
            let deadline = self.next_deadline().await;
            tokio::select! {
                () = self.events.wake.notified() => {}
                Some(request) = requests.recv() => {
                    metrics.wakes.increment();
                    let _busy = metrics.busy();
                    self.failures.remove(&request.environment);
                    let result = self.deliver(Some(&BTreeSet::from([request.environment.clone()])), false).await;
                    // The environment's cards follow what was delivered at the next pass.
                    self.events.record(|pending| {
                        pending.cards.environments.insert(request.environment.clone());
                    });
                    let _ = request.reply.send(result);
                    continue;
                }
                () = sleep_until(deadline) => { metrics.requeues.increment(); }
                Some(_) = sources.join_next(), if !sources.is_empty() => { continue; }
            }
            metrics.wakes.increment();
            let started = tokio::time::Instant::now();
            {
                let _busy = metrics.busy();
                if let Err(error) = self.pass().await {
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
            self.failures.iter().filter(|(_, failure)| failure.next_at.is_some_and(|next| next <= now)).map(|(name, _)| name.clone()),
        );
        // The cards of every environment an event named or a retry reached, delivered or still in
        // backoff; every card when every environment is delivered or the Codex login rotated.
        let mut cards = std::mem::take(&mut pending.cards);
        cards.every |= pending.all || pending.definitions || pending.codex_rotated;
        cards.environments.extend(environments.iter().cloned());
        let result = if pending.all || pending.definitions {
            if pending.definitions {
                self.failures.clear();
            }
            self.deliver(None, false).await
        } else if !environments.is_empty() {
            // An event retries an environment that gave up; one in backoff waits for it.
            let due: BTreeSet<String> = environments
                .into_iter()
                .filter(|environment| self.failures.get(environment).is_none_or(|failure| failure.next_at.is_none_or(|next| next <= now)))
                .collect();
            if due.is_empty() {
                Ok(())
            } else {
                self.deliver(Some(&due), false).await
            }
        } else {
            Ok(())
        };
        self.follow_codex_login(pending.codex_rotated, now).await;
        let wake = GithubAppRefreshWake { projects: pending.projects, retry_given_up: pending.definitions };
        let refreshed = self.store.refresh_github_app_tokens(&wake).await;
        for error in &refreshed.errors {
            warn!(error = %error.message, environment = %error.environment_ref, "failed to refresh GitHub App credential delivery");
        }
        // A re-mint, failed or not, may change what its environment reports delivered.
        cards.environments.extend(refreshed.environments);
        // Cards read committed delivery observations, never the preparation index.
        if let Err(error) = self.state.daemon.refresh_capability_cards(&self.namespace, &cards).await {
            warn!(%error, "failed to refresh session capability cards");
        }
        self.tell_refresh_failures().await;
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
            let wait = (at - chrono::Utc::now()).to_std().unwrap_or_default();
            tokio::time::Instant::now() + wait
        });
        self.failures
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

    /// Delivers `environments`, or every environment some vessel stands on or this daemon
    /// delivered to. `adopt` prepares the running crews' GitHub App tokens again, as the start
    /// pass does after a restart.
    async fn deliver(&mut self, environments: Option<&BTreeSet<String>>, adopt: bool) -> Result<(), String> {
        let wanted = match self.wanted(environments).await {
            Ok(wanted) => wanted,
            Err(error) => {
                // The records could not be read: each environment named is retried at its backoff.
                for environment in environments.into_iter().flatten() {
                    self.fail(environment, &error, true);
                }
                return Err(error);
            }
        };
        let mut errors = Vec::new();
        for (environment, wanted) in wanted {
            match self.deliver_environment(&environment, wanted, adopt).await {
                Ok(()) => {
                    if self.failures.remove(&environment).is_some() {
                        info!(%environment, "credential delivery recovered");
                    }
                }
                Err((error, retryable)) => {
                    self.fail(&environment, &error, retryable);
                    errors.push(error);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn fail(&mut self, environment: &str, error: &str, retryable: bool) {
        let attempts = self.failures.get(environment).map_or(0, |failure| failure.attempts).saturating_add(1);
        let failure = Failure::after(attempts, retryable);
        warn!(%environment, %error, attempts = failure.attempts, retry = failure.next_at.is_some(), "credential delivery failed");
        self.failures.insert(environment.to_owned(), failure);
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
            tokio::task::spawn_blocking(move || environment_vessels(reads.as_ref(), named.as_ref()))
                .await
                .map_err(|error| format!("read credential vessels: {error}"))?
                .map_err(|error| format!("read credential vessels: {error}"))?;
        // The other vessels' credentials are still delivered.
        for VesselWithoutConvoy { vessel, convoy, convoy_home } in &without_convoy {
            warn!(%vessel, %convoy, %convoy_home, "credential delivery: the vessel's convoy record is missing; its credentials are left out");
            self.metrics.skipped.increment();
        }
        let needs_grants = vessels.iter().any(|vessel| !vessel.requirement.credential_scopes.is_empty());
        let (grants, repository_trust) = if needs_grants {
            let grants = backend
                .including_replicas::<CredentialGrant>(namespace)
                .list()
                .await
                .map_err(|error| format!("list credential grants: {error}"))?
                .items
                .into_iter()
                .map(|grant| grant.object.spec)
                .collect::<Vec<_>>();
            let repository_trust = backend
                .including_replicas::<Repository>(namespace)
                .list()
                .await
                .map_err(|error| format!("list repositories for credential grants: {error}"))?
                .items
                .into_iter()
                .map(|source| {
                    let trust = if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own };
                    (RepositoryKey(source.object.metadata.name), trust)
                })
                .collect::<BTreeMap<_, _>>();
            (grants, repository_trust)
        } else {
            (Vec::new(), BTreeMap::new())
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
        for (environment, delivered) in self.store.tracked_work_deliveries().await {
            if environments.is_none_or(|set| set.contains(&environment)) {
                wanted.entry(environment).or_default().granted.extend(delivered);
            }
        }
        // An environment named but wanting nothing still answers, as delivered.
        for environment in environments.into_iter().flatten() {
            wanted.entry(environment.clone()).or_default();
        }
        Ok(wanted)
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

/// Refreshes every card when a fulfilment kind's grants change, at any home: a card lists the
/// grants of its work's placement. Kinds change rarely, so no card is singled out.
async fn watch_fulfilment_kinds(backend: flotilla_store::ResourceBackend, namespace: String, events: Arc<CredentialEvents>) {
    // Each kind's grants as last seen; a kind not seen since the watch opened counts as changed.
    let mut grants = BTreeMap::<String, BTreeSet<FulfilmentGrant>>::new();
    let mut failures = 0_u32;
    loop {
        match backend.including_replicas::<FulfilmentKind>(&namespace).watch().await {
            Ok(mut changes) => {
                // Changes may have been missed while the watch was closed.
                grants.clear();
                events.record(|pending| pending.cards.every = true);
                while let Some(change) = changes.next().await {
                    let changed = match change {
                        Ok(ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read)) => {
                            let now = read.object.spec.grants;
                            grants.insert(read.object.metadata.name, now.clone()).is_none_or(|before| before != now)
                        }
                        Ok(ReadWatchEvent::Deleted(read)) => {
                            grants.remove(&read.object.metadata.name);
                            true
                        }
                        Ok(ReadWatchEvent::DeletedByName { tombstone, .. }) => {
                            grants.remove(&tombstone.name);
                            true
                        }
                        Err(error) => {
                            debug!(%error, "credential delivery fulfilment kind watch failed");
                            break;
                        }
                    };
                    failures = 0;
                    if changed {
                        events.record(|pending| pending.cards.every = true);
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

/// Refreshes every card when a build becomes built or stops being built: a card lists a built
/// image's verified tools. No index finds the environments on one image, so no card is singled
/// out; a launch's other steps refresh none.
async fn watch_image_builds(state: Arc<ControllerRuntimeState>, events: Arc<CredentialEvents>) {
    let reads = state.daemon.lifecycle_reads();
    let mut changes = match reads.image_builds() {
        Ok(builds) => builds.changes(),
        Err(error) => {
            warn!(%error, "credential delivery cannot follow image builds; other events still wake it");
            return;
        }
    };
    // Whether each build was built when last read; a build not read yet counts as changed.
    let mut built = BTreeMap::<String, bool>::new();
    loop {
        match changes.recv().await {
            Ok(ImageBuildChange::Changed(name)) => {
                let (reads, read) = (Arc::clone(&reads), name.clone());
                let read = tokio::task::spawn_blocking(move || reads.image_builds()?.get(&read))
                    .await
                    .unwrap_or_else(|error| Err(format!("image build read: {error}")));
                let now = match read {
                    Ok(build) => build.is_some_and(|build| build.digest().is_some()),
                    Err(error) => {
                        debug!(%error, build = %name, "credential delivery could not read an image build; refreshing every card");
                        built.remove(&name);
                        events.record(|pending| pending.cards.every = true);
                        continue;
                    }
                };
                if built.insert(name, now) != Some(now) {
                    events.record(|pending| pending.cards.every = true);
                }
            }
            Ok(ImageBuildChange::Rescanned) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                built.clear();
                events.record(|pending| pending.cards.every = true);
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
