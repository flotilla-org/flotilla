//! Background task ownership, supervision, retention, and watch loops.

/// Shared by supervision and the forge-observation health classification.
pub(super) const AGGREGATOR_CONTROLLER: &str = "aggregator";

use flotilla_daemon_api::daemon::DaemonHandle;
use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, PoisonError},
    time::Duration,
};

use chrono::Utc;
use flotilla_admission::reconcilers::vessel_requests::VesselRequests;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_projections::aggregator_projection::AggregatorProjectionState;
use flotilla_provisioning::checkout_runtime::sweep_host_empty_convoy_directories;

use flotilla_credentials::CredentialStore;
use flotilla_discovery::registry::{command_provider_registry, ProviderRegistry};
use flotilla_environment::environment::EnvironmentInput;
use flotilla_resources::{
    Artifact, Convoy, CredentialSpec, FulfilmentFacts, Host, HostStatusPatch, ModelProbeState, Resource, ResourceError, RetryBackoff,
    TerminalSession, Vessel,
};
use flotilla_store::{
    load_metrics::{attributed, caller, inherit},
    watch_resource_kind_including_replicas, ResourceBackend,
};
use flotilla_vcs::vcs::REMOTE_CHECKOUT_ARCHIVE_SWEEP_TIMEOUT;
use futures::stream::{BoxStream, SelectAll};
use futures::FutureExt;
use futures::StreamExt;
use serde_json::Value;
use tokio::{
    sync::{broadcast, oneshot, watch, Mutex},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

use super::discovery::{
    observe_fulfilment_facts, BagEnvVars, FulfilmentProbeContext, LocalProvisioningProfile, ProbeListings, SshHostProfile,
};
use super::docker::DockerControllerRuntime;
use super::environments::reconcile_provisioned_environments;
use super::health::{apply_host_heartbeat_with_credentials, DaemonHealthIdentity, HostObservations, RuntimeHealth};
use super::state::ControllerRuntimeState;
use super::StartupRestoration;
use crate::{
    artifact::ArtifactService,
    blob_store::TieredBlobStore,
    dispatch_reconciler::{DaemonDispatchIssueSource, DispatchIssueSource, DispatchReconciler},
    resource_manifest::{materialize_bound_manifest_root, ResourceManifestReconciler},
    sleep_inhibitor,
    supervisor::{supervise, ControllerSupervision},
};

pub(super) const MANIFEST_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

pub(super) const CONVOY_ENSURE_CONDITION_TYPE: &str = "Controller/standing_convoy_ensure";

pub(crate) fn manifest_reconciler_enabled(declared_root: &str, local_root: &str) -> bool {
    declared_root == local_root
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize)]
pub(super) struct CheckoutArchiveRoot {
    pub(super) env_ref: String,
    pub(super) path: PathBuf,
}

pub(super) struct CheckoutArchiveSweep {
    pub(super) daemon: Arc<InProcessDaemon>,
    pub(super) catalog_path: PathBuf,
    pub(super) roots: Vec<CheckoutArchiveRoot>,
    pub(super) retention_days: u64,
}

pub(super) async fn load_checkout_archive_roots(path: &Path) -> Result<BTreeSet<CheckoutArchiveRoot>, String> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| format!("decode {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// How long an unreferenced blob stands before it is collected.
pub(super) const BLOB_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
/// The wait before retrying a failed collection.
const GC_RETRY: RetryBackoff = RetryBackoff { initial: Duration::from_secs(60), maximum: Duration::from_secs(60 * 60) };
/// Consecutive failed collections retried at [`GC_RETRY`]; past it, the next change or expiry
/// retries.
pub(super) const GC_RETRY_LIMIT: u32 = 10;
/// The host condition artifact retention reports while its failures are past [`GC_RETRY_LIMIT`].
pub(super) const RETENTION_CONDITION_TYPE: &str = "Artifact/Retention";

/// Artifact retention's consecutive failures. Each is retried at [`GC_RETRY`] up to
/// [`GC_RETRY_LIMIT`]; the first past it is warned once with its reason and reported in the host's
/// health, which holds it until a pass succeeds.
pub(super) struct RetentionFailures {
    count: u32,
    health: RuntimeHealth,
}

impl RetentionFailures {
    pub(super) fn new(health: RuntimeHealth) -> Self {
        Self { count: 0, health }
    }

    /// A pass succeeded: the count and any reported failure clear.
    pub(super) fn succeeded(&mut self) {
        if self.count > GC_RETRY_LIMIT {
            self.health.report_retention(None);
        }
        self.count = 0;
    }

    /// A pass failed with `error`: the wait before it is retried, or `None` past the limit, when
    /// `then` says what happens instead.
    pub(super) fn failed(&mut self, error: &str, then: &str) -> Option<Duration> {
        self.count = self.count.saturating_add(1);
        if self.count <= GC_RETRY_LIMIT {
            warn!(%error, failures = self.count, "artifact retention failed; retrying");
            return Some(GC_RETRY.delay(self.count));
        }
        if self.count == GC_RETRY_LIMIT + 1 {
            warn!(%error, failures = self.count, "artifact retention keeps failing; {then}");
            self.health.report_retention(Some(format!("artifact retention failed {} times in a row; {then}: {error}", self.count)));
        } else {
            debug!(%error, failures = self.count, "artifact retention failed again");
        }
        None
    }
}

/// Artifact retention: reaps this home's expired artifacts and collects the blobs no artifact
/// references any more ([`Retention`]). A convoy's retirement reaps none of its artifacts: they go
/// at their own expiry or by an explicit delete. It lists the artifacts once, then acts at each
/// change to one and when the next artifact expires or a blob's grace passes: nothing is listed
/// again and no timer runs. A failure retries at a backoff a finite number of times; past it, a
/// failing pass waits for the next change or expiry, and a failing listing stops retention until
/// the daemon restarts.
pub(super) async fn run_artifact_gc(
    backend: ResourceBackend,
    namespace: String,
    blobs: Arc<TieredBlobStore>,
    grace: Duration,
    health: RuntimeHealth,
) {
    let mut failures = RetentionFailures::new(health);
    loop {
        let stopped = retain(&backend, &namespace, &blobs, grace, &mut failures).await;
        match failures.failed(&format!("{stopped}; reading the artifacts again"), "it stops until the daemon restarts") {
            Some(wait) => tokio::time::sleep(wait).await,
            None => return,
        }
    }
}

/// Retention over one listing and the changes after it, until the artifact watch ends: why it
/// stopped.
async fn retain(
    backend: &ResourceBackend,
    namespace: &str,
    blobs: &Arc<TieredBlobStore>,
    grace: Duration,
    failures: &mut RetentionFailures,
) -> String {
    let mut unwritten = blobs.follow_unwritten();
    // Watched before the listing, so no change between the two is missed.
    let mut changes = match backend.including_replicas::<Artifact>(namespace).watch().await {
        Ok(changes) => changes.ready_chunks(64),
        Err(error) => return format!("watch artifacts: {error}"),
    };
    let service = ArtifactService { backend, blobs: blobs.as_ref(), namespace };
    let mut retention = match service.retention(grace).await {
        Ok(retention) => retention,
        Err(error) => return format!("list artifacts: {error}"),
    };
    // This home's artifacts to delete: the expired ones, and those whose delete failed.
    let mut due: BTreeSet<String> = BTreeSet::new();
    // One look at the stores at the start collects what no artifact referenced then: among it, the
    // body of a put whose artifact never landed before a restart. Later, a blob whose last
    // reference goes, or whose artifact's write failed.
    let mut leftovers = Some(tokio::time::Instant::now());
    loop {
        let now = Utc::now();
        due.extend(retention.expired(now));
        let mut failed = service.delete(&mut retention, due.iter().cloned()).await.err();
        if failed.is_none() {
            due.clear();
        }
        for digest in retention.collectable(now) {
            match blobs.collect(&digest, grace).await {
                Ok(None) => retention.collected(&digest),
                Ok(Some(left)) => retention.defer(&digest, now + chrono::Duration::from_std(left).unwrap_or(chrono::Duration::MAX)),
                Err(error) => drop(failed.get_or_insert(error)),
            }
        }
        if leftovers.is_some_and(|at| at <= tokio::time::Instant::now()) {
            match blobs.gc_unreferenced(&retention.referenced(), grace).await {
                Ok(left) => leftovers = left.map(|left| tokio::time::Instant::now() + left),
                Err(error) => drop(failed.get_or_insert(error)),
            }
        }
        let wake = |at: chrono::DateTime<Utc>| tokio::time::Instant::now() + (at - Utc::now()).to_std().unwrap_or_default();
        // A failure waits for its retry, not for what is already due; past its retries, for the
        // next change, or the next expiry or grace still ahead.
        let deadline = match failed {
            None => {
                failures.succeeded();
                retention.next_deadline().map(wake).into_iter().chain(leftovers).min()
            }
            Some(error) => match failures.failed(&error, "the next change or expiry retries it") {
                Some(wait) => Some(tokio::time::Instant::now() + wait),
                None => {
                    let ahead = leftovers.filter(|at| *at > tokio::time::Instant::now());
                    retention.next_deadline_after(Utc::now()).map(wake).into_iter().chain(ahead).min()
                }
            },
        };
        tokio::select! {
            batch = changes.next() => {
                let Some(events) = batch else { return "the artifact watch ended".into() };
                let now = Utc::now();
                for event in events {
                    let event = match event {
                        Ok(event) => event,
                        Err(error) => return format!("artifact watch: {error}"),
                    };
                    retention.apply(event, now);
                }
            }
            digest = unwritten.recv() => match digest {
                Ok(digest) => retention.unwritten(&digest, Utc::now()),
                // Fell behind: the stores are looked at again.
                Err(broadcast::error::RecvError::Lagged(_)) => leftovers = Some(tokio::time::Instant::now()),
                Err(broadcast::error::RecvError::Closed) => return "the blob store closed".into(),
            },
            () = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}

pub(super) async fn run_checkout_archive_gc(archive_sweep: CheckoutArchiveSweep) {
    let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        {
            if let Some(host) = archive_sweep.daemon.local_host_id() {
                let reads = archive_sweep.daemon.lifecycle_reads();
                let swept = match (reads.convoys(), reads.environments()) {
                    (Ok(convoys), Ok(environments)) => sweep_host_empty_convoy_directories(convoys, environments, host.as_str()).await,
                    (Err(error), _) | (_, Err(error)) => Err(error),
                };
                if let Err(error) = swept {
                    warn!(%error, "empty convoy directory sweep failed");
                }
            } else {
                debug!("empty convoy directory sweep skipped: local host identity unavailable");
            }
            let mut roots = archive_sweep.roots.iter().cloned().collect::<BTreeSet<_>>();
            match load_checkout_archive_roots(&archive_sweep.catalog_path).await {
                Ok(recorded) => roots.extend(recorded),
                Err(error) => warn!(%error, "checkout archive sweep could not load archive roots"),
            }
            let reads = archive_sweep.daemon.lifecycle_reads();
            match tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || reads.clones()?.here()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|here| here)
            {
                Ok(clones) => {
                    for clone in clones {
                        if let Some(parent) = Path::new(&clone.spec.path).parent() {
                            roots.insert(CheckoutArchiveRoot { env_ref: clone.spec.env_ref, path: parent.join(".flotilla-archives") });
                        }
                    }
                }
                Err(error) => warn!(%error, "checkout archive sweep could not list clones"),
            }
            let reads = archive_sweep.daemon.lifecycle_reads();
            match tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || reads.checkouts()?.here()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|here| here)
            {
                Ok(checkouts) => {
                    for checkout in checkouts {
                        if let Some(parent) = checkout.spec.target_path().and_then(|path| Path::new(path).parent()) {
                            if let Some(env_ref) = checkout.spec.env_ref() {
                                roots.insert(CheckoutArchiveRoot { env_ref: env_ref.to_string(), path: parent.join(".flotilla-archives") });
                            }
                        }
                    }
                }
                Err(error) => warn!(%error, "checkout archive sweep could not list checkouts"),
            }
            for root in roots {
                if let Some(environment) = archive_sweep.daemon.resolve_environment_ref(&root.env_ref) {
                    let runner = environment.runner;
                    let result = if environment.id == *archive_sweep.daemon.local_environment_id() {
                        flotilla_vcs::vcs::prune_checkout_archives(&*runner, &root.path, archive_sweep.retention_days).await
                    } else {
                        flotilla_vcs::vcs::prune_remote_checkout_archives(
                            &*runner,
                            &root.path,
                            archive_sweep.retention_days,
                            REMOTE_CHECKOUT_ARCHIVE_SWEEP_TIMEOUT,
                        )
                        .await
                    };
                    if let Err(error) = result {
                        warn!(archive_root = %root.path.display(), env_ref = %root.env_ref, %error, "checkout archive retention sweep failed");
                    }
                }
            }
        }
    }
}

pub(super) fn spawn_manifest_reconciler_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    manifests: flotilla_config::ResourceManifestsConfig,
    interval: Duration,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        supervise_controller("manifest", supervision, runtime_health, move || {
            let daemon = Arc::clone(&daemon);
            let namespace = namespace.clone();
            let manifests = manifests.clone();
            async move {
                let roots = daemon.manifest_roots();
                materialize_bound_manifest_root(&*roots, &namespace, &manifests).await.map_err(ResourceError::other)?;
                let vcs = if manifests.binding.is_some() {
                    daemon.local_charter_vcs()
                } else {
                    daemon.local_vcs_for_checkout(&manifests.dir).await
                }
                .map_err(ResourceError::other)?;
                ResourceManifestReconciler::new(daemon.resource_backend(), roots, namespace, manifests.dir)
                    .with_declared_source(manifests.source, manifests.reconciler_root)
                    .with_binding(manifests.binding)
                    .with_vcs(vcs)
                    .run(interval)
                    .await
            }
        })
        .await;
    })
}

pub(super) async fn supervise_controller<F, Fut>(
    name: &'static str,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
    mut make_run: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), ResourceError>>,
{
    loop {
        let result = supervise(name, supervision.clone(), || {
            // Its engine reads count for it, unless it names a caller of its own.
            let run = attributed(caller(name), make_run());
            let health = runtime_health.clone();
            // Use the supervisor's same healthy duration for alarm recovery.
            // A run failing before this window leaves the exhaustion alarm set.
            let healthy_after = supervision.success_reset_after;
            async move {
                tokio::pin!(run);
                tokio::select! {
                    result = &mut run => result,
                    () = tokio::time::sleep(healthy_after) => {
                        health.failures.lock().expect("runtime health lock poisoned").remove(&format!("Controller/{name}"));
                        run.await
                    }
                }
            }
        })
        .await;
        match result {
            Ok(()) => {
                runtime_health.failures.lock().expect("runtime health lock poisoned").remove(&format!("Controller/{name}"));
                return;
            }
            Err(exhausted) => {
                runtime_health.report_restart_budget_exhausted(exhausted);
                tokio::time::sleep(supervision.recovery_backoff).await;
            }
        }
    }
}

/// A controller supervised under `name`: a second controller of one primary kind (the account
/// bindings' over Convoy) keeps its own supervision and health.
pub(super) fn spawn_named_controller<F, Fut>(
    name: &'static str,
    backend: ResourceBackend,
    namespace: String,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
    mut make_run: F,
) -> JoinHandle<()>
where
    F: FnMut(ResourceBackend, String) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), ResourceError>> + Send + 'static,
{
    tokio::spawn(async move {
        supervise_controller(name, supervision, runtime_health, move || make_run(backend.clone(), namespace.clone())).await;
    })
}

pub(super) fn spawn_vessel_requests(
    requests: VesselRequests,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // The requester and the Vessel reconciler need distinct health keys, while both remain tied to the resource they manage.
        supervise_controller(Vessel::API_PATHS.plural, supervision, runtime_health, move || {
            let requests = requests.clone();
            attributed(caller("VesselRequests"), async move { requests.run().await })
        })
        .await;
    })
}

pub(super) fn spawn_sleep_inhibitor_task(
    backend: ResourceBackend,
    namespace: String,
    host_id: String,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        supervise_controller("sleep_inhibitor", supervision, runtime_health, move || {
            let convoys = backend.clone().including_replicas::<Convoy>(&namespace);
            let vessels = backend.clone().using::<Vessel>(&namespace);
            let hosts = backend.clone().using::<Host>(&namespace);
            let host_id = host_id.clone();
            async move { sleep_inhibitor::run(convoys, vessels, hosts, host_id).await }
        })
        .await;
    })
}

pub(super) const FULFILMENT_CHANGE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Probe local fulfilment facts. Changed facts are published at once; a probe
/// that only refreshes `observed_at` is kept in `observations` and writes nothing.
pub(super) fn spawn_local_fulfilment_probe_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    providers: Arc<ProviderRegistry>,
    scratch: PathBuf,
    observations: HostObservations,
    interval: Duration,
) -> JoinHandle<()> {
    let probe = Arc::new(LocalFulfilmentProbe::new(daemon, namespace, profile, providers, scratch, observations));
    spawn_periodic_task("local fulfilment probe", interval, PeriodicTaskStart::Immediate, move || {
        let probe = Arc::clone(&probe);
        async move { probe.tick().await }
    })
}

/// One local fulfilment probe, and what its ticks keep between them.
pub(super) struct LocalFulfilmentProbe {
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    providers: Arc<ProviderRegistry>,
    scratch: PathBuf,
    observations: HostObservations,
    local_observation: StdMutex<super::discovery::LocalObservation>,
    listings: Mutex<ProbeListings>,
}

impl LocalFulfilmentProbe {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        namespace: String,
        profile: LocalProvisioningProfile,
        providers: Arc<ProviderRegistry>,
        scratch: PathBuf,
        observations: HostObservations,
    ) -> Self {
        Self {
            daemon,
            namespace,
            profile,
            providers,
            scratch,
            observations,
            local_observation: StdMutex::default(),
            listings: Mutex::default(),
        }
    }

    pub(super) async fn tick(&self) {
        let Self { daemon, namespace, profile, providers, scratch, observations, local_observation, listings } = self;
        observations.record_provider_advertisements(super::discovery::observe_provider_advertisements(providers).await).await;
        // Image probes run with the scratch directory as their working directory.
        if let Err(error) = tokio::fs::create_dir_all(scratch).await {
            warn!(%error, scratch = %scratch.display(), "cannot create the fulfilment probe directory");
        }
        let discovery = daemon.discovery_runtime();
        let hosts = daemon.resource_backend().using::<Host>(namespace);
        // Probe ages come from this process's latest observation: the store keeps no
        // observation time, so a fact this process has not observed is probed afresh.
        let published = observations.fulfilment_facts().await;
        let previous_published = published.is_some();
        let previous = published.unwrap_or_default();
        let mut model_probes = ModelProbeState::default();
        let harnesses = discovery.factories.harnesses(discovery.env.as_ref());
        let observation = match local_observation.lock().unwrap_or_else(PoisonError::into_inner).current(discovery) {
            Ok(observation) => observation,
            Err(error) => {
                warn!(%error, "local host is not ready for fulfilment probes");
                return;
            }
        };
        match observe_fulfilment_facts(
            &daemon.resource_backend(),
            namespace,
            &profile.host_id,
            &profile.available_pools,
            &previous,
            FulfilmentProbeContext {
                providers,
                runner: discovery.runner.as_ref(),
                env: discovery.env.as_ref(),
                scratch,
                observation: &observation,
                harnesses: &harnesses,
                pinned_harnesses: &Default::default(),
                listings: &mut *listings.lock().await,
            },
            &mut model_probes,
        )
        .await
        {
            Ok(facts) => {
                // Facts this process published, observed again, write nothing; model probes are
                // not kept. Remember facts only once the store holds them, so a failed publish of
                // changed facts is retried.
                let published = if previous_published && FulfilmentFacts::same_observations(&facts, &previous) {
                    true
                } else {
                    match flotilla_store::observe_status(
                        &hosts,
                        &profile.host_id,
                        &HostStatusPatch::FulfilmentFacts { facts: facts.clone(), model_probes },
                    )
                    .await
                    {
                        Ok(()) => true,
                        Err(error) => {
                            warn!(%error, "failed to publish observed fulfilment facts");
                            false
                        }
                    }
                };
                if published {
                    observations.record_fulfilment_facts(facts).await;
                }
            }
            Err(error) => warn!(%error, "failed to observe local fulfilment facts"),
        }
    }
}

pub(super) fn spawn_ssh_fulfilment_probe_task(daemon: Arc<InProcessDaemon>, namespace: String, ssh: SshHostProfile) -> JoinHandle<()> {
    // Compose the remote detection endpoint once, then retain that exact
    // provider instance across observation passes.
    let providers = Arc::new(command_provider_registry(&[EnvironmentInput::HostDirect], Arc::clone(&ssh.runner)));
    let listings = Arc::new(Mutex::new(ProbeListings::default()));
    spawn_periodic_task("ssh fulfilment probe", FULFILMENT_CHANGE_CHECK_INTERVAL, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let ssh = ssh.clone();
        let providers = Arc::clone(&providers);
        let listings = Arc::clone(&listings);
        async move {
            let hosts = daemon.resource_backend().using::<Host>(&namespace);
            // As for the local host, probe ages come from this process's latest observation.
            let previous = ssh.fulfilment_facts.read().await.clone();
            let mut model_probes = ModelProbeState::default();
            let scratch = match ssh.runner.writable_config_base(None, Path::new("/tmp")).await {
                Ok(base) => base.join("probe-cwd"),
                Err(error) => {
                    warn!(%error, "cannot select SSH probe directory");
                    return;
                }
            };
            if let Err(error) = ssh
                .runner
                .run("mkdir", &["-p", &scratch.to_string_lossy()], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default)
                .await
            {
                warn!(%error, scratch = %scratch.display(), "cannot create the SSH fulfilment probe directory");
            }
            let harnesses = daemon.discovery_runtime().factories.harnesses(&BagEnvVars(&ssh.env_bag));
            // #4093: the host's crews run the pinned harness delivered to them, so its
            // facts report the pin, whatever the host has installed.
            let pinned_harnesses = flotilla_resources::stdlib::stdlib_harness_pins();
            let observation = match ssh.preparer(Vec::new()).observation() {
                Ok(observation) => observation,
                Err(error) => {
                    let error = error.for_environment(ssh.environment_id.as_str());
                    warn!(%error, "SSH host cannot be probed");
                    return;
                }
            };
            match observe_fulfilment_facts(
                &daemon.resource_backend(),
                &namespace,
                &ssh.provisioning.host_id,
                &ssh.provisioning.available_pools,
                &previous,
                FulfilmentProbeContext {
                    providers: &providers,
                    runner: ssh.runner.as_ref(),
                    env: &BagEnvVars(&ssh.env_bag),
                    scratch: &scratch,
                    observation: &observation,
                    harnesses: &harnesses,
                    pinned_harnesses,
                    listings: &mut *listings.lock().await,
                },
                &mut model_probes,
            )
            .await
            {
                Ok(facts) => {
                    // As for the local host: facts published before write nothing, and facts are
                    // remembered only once the store holds them.
                    let published = if !previous.is_empty() && FulfilmentFacts::same_observations(&facts, &previous) {
                        true
                    } else {
                        match flotilla_store::observe_status(
                            &hosts,
                            &ssh.provisioning.host_id,
                            &HostStatusPatch::FulfilmentFacts { facts: facts.clone(), model_probes },
                        )
                        .await
                        {
                            Ok(()) => true,
                            Err(error) => {
                                warn!(host = %ssh.provisioning.host_id, %error, "failed to publish observed SSH fulfilment facts");
                                false
                            }
                        }
                    };
                    if published {
                        *ssh.fulfilment_facts.write().await = facts;
                    }
                }
                Err(error) => warn!(host = %ssh.provisioning.host_id, %error, "failed to observe SSH fulfilment facts"),
            }
        }
    })
}

pub(super) fn spawn_heartbeat_task_with_credentials(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    credential_store: Arc<Option<Arc<CredentialStore>>>,
    health: DaemonHealthIdentity,
    runtime_health: RuntimeHealth,
    interval: Duration,
) -> JoinHandle<()> {
    // Each tick publishes, and so does a condition that must not wait for one (a remote a clone
    // could not reach), and so does a change to the credential declarations: the held set judges
    // only the declarations the host can list, so one applied after a publish is judged at once.
    tokio::spawn(attributed(caller("host heartbeat"), async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut declarations: Option<BoxStream<'static, Result<flotilla_resources::ReadWatchEvent<CredentialSpec>, ResourceError>>> = None;
        loop {
            let declaration = async {
                match declarations.as_mut() {
                    Some(watch) => watch.next().await,
                    None => std::future::pending().await,
                }
            };
            let (ticked, watching) = tokio::select! {
                _ = ticker.tick() => (true, true),
                () = runtime_health.heartbeat_now.notified() => (false, true),
                event = declaration => (false, matches!(event, Some(Ok(_)))),
            };
            if !watching {
                declarations = None;
            }
            // Made at a tick, before the publish, so a declaration applied after the publish wakes
            // the next; one that fails or closes is made again an interval later.
            if ticked && declarations.is_none() && credential_store.is_some() {
                match daemon.resource_backend().including_replicas::<CredentialSpec>(&namespace).watch().await {
                    Ok(watch) => declarations = Some(watch),
                    Err(error) => warn!(%error, "credential declarations cannot be watched; held credentials are judged each heartbeat"),
                }
            }
            if let Err(err) =
                apply_host_heartbeat_with_credentials(&daemon, &namespace, &profile, credential_store.as_deref(), &health, &runtime_health)
                    .await
            {
                warn!(%err, "failed to publish host heartbeat");
            }
        }
    }))
}

#[cfg(test)]
pub(super) async fn apply_host_heartbeat(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    health: &DaemonHealthIdentity,
) -> Result<(), String> {
    apply_host_heartbeat_with_credentials(daemon, namespace, profile, None, health, &RuntimeHealth::default()).await
}

pub(super) fn spawn_host_description_projection_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(attributed(caller("host description projection"), async move {
        let mut resync = tokio::time::interval(interval);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let mut watch = match daemon.resource_backend().including_replicas::<Host>(&namespace).watch().await {
                // Drain ready bursts together so replicated heartbeat fan-in
                // causes one projection pass per batch, not one scan per event.
                Ok(watch) => watch.ready_chunks(64),
                Err(error) => {
                    warn!(%error, "watch host descriptions failed");
                    resync.tick().await;
                    continue;
                }
            };
            // Subscribe before listing so an update during projection cannot be lost.
            loop {
                if let Err(error) = daemon.refresh_resource_host_summaries().await {
                    warn!(%error, "project host descriptions failed");
                }
                tokio::select! {
                    event = watch.next() => {
                        match event {
                            Some(events) => {
                                if let Some(error) = events.into_iter().find_map(Result::err) {
                                    warn!(%error, "host description watch failed; resubscribing");
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = resync.tick() => {}
                }
            }
            resync.tick().await;
        }
    }))
}

/// Reconcile durable supervisor turns on resource changes, with periodic recovery.
/// The caller owns the returned task and must abort it when the daemon stops.
/// Readiness resolves after both watches subscribe and the startup scan completes.
#[doc(hidden)]
pub fn spawn_pending_supervisor_turn_task(daemon: Arc<InProcessDaemon>, namespace: String) -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let backend = daemon.resource_backend();
    let watch_namespace = namespace.clone();
    let message_path = daemon.crew_message_path();
    spawn_pending_supervisor_turn_task_with_watches(daemon, namespace, move || {
        let backend = backend.clone();
        let namespace = watch_namespace.clone();
        let message_path = Arc::clone(&message_path);
        async move {
            let convoys = backend.including_replicas::<Convoy>(&namespace).watch().await?;
            let sessions = backend.including_replicas::<TerminalSession>(&namespace).watch().await?;
            // The Message path's commits, which list no message to hear one.
            let messages = message_path.get().ok_or_else(|| ResourceError::other("the Message path is not installed"))?.changes()?.map(Ok);
            Ok((
                futures::stream::select(convoys.map(|event| event.map(|_| ())), messages.map(|event| event.map(|_| ()))).boxed(),
                sessions.map(|event| event.map(|_| ())).boxed(),
            ))
        }
    })
}

pub(super) type SupervisorTurnWatches = (BoxStream<'static, Result<(), ResourceError>>, BoxStream<'static, Result<(), ResourceError>>);

/// Failed passes and subscriptions back off from one second to a minute; a pass gives up after
/// this many failures until the next change.
const SUPERVISOR_TURN_RETRY_LIMIT: u32 = 10;
const SUPERVISOR_TURN_RETRY: RetryBackoff = RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(60) };

// Watch subscription is the notification boundary. Tests can close one input
// without changing the resource stores or the reconciliation they exercise.
#[visibility::make(pub)]
#[doc(hidden)]
pub(super) fn spawn_pending_supervisor_turn_task_with_watches<F, Fut>(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    mut subscribe: F,
) -> (JoinHandle<()>, oneshot::Receiver<()>)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<SupervisorTurnWatches, ResourceError>> + Send,
{
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(inherit(async move {
        let mut ready_tx = Some(ready_tx);
        let mut pass_failures = 0_u32;
        let mut watch_failures = 0_u32;
        loop {
            let (mut convoys, mut sessions) = match subscribe().await {
                Ok((convoys, sessions)) => (convoys.ready_chunks(64), sessions.ready_chunks(64)),
                Err(error) => {
                    warn!(%error, %namespace, "supervisor turn watch failed; resubscribing");
                    watch_failures = watch_failures.saturating_add(1);
                    tokio::time::sleep(SUPERVISOR_TURN_RETRY.delay(watch_failures)).await;
                    continue;
                }
            };
            // Subscribe to both inputs before scanning to preserve updates during a pass.
            // Invariant: a no-op reconcile_pending_supervisor_turns_once must issue no
            // writes, or its own watch events would keep this loop running.
            loop {
                let mut failed = false;
                if let Err(error) = daemon.reconcile_pending_supervisor_turns_once(&namespace).await {
                    warn!(%error, %namespace, "failed to reconcile pending supervisor turns");
                    failed = true;
                }
                pass_failures = if failed { pass_failures.saturating_add(1) } else { 0 };
                if pass_failures == SUPERVISOR_TURN_RETRY_LIMIT {
                    error!(%namespace, failures = pass_failures, "pending supervisor turns failed repeatedly; waiting for the next change");
                }
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(());
                }
                let retry = (failed && pass_failures < SUPERVISOR_TURN_RETRY_LIMIT).then(|| SUPERVISOR_TURN_RETRY.delay(pass_failures));
                let events = tokio::select! {
                    events = convoys.next() => events,
                    events = sessions.next() => events,
                    _ = tokio::time::sleep(retry.unwrap_or_default()), if retry.is_some() => continue,
                };
                match events {
                    Some(events) => {
                        if let Some(error) = events.into_iter().find_map(Result::err) {
                            warn!(%error, %namespace, "supervisor turn watch failed; resubscribing");
                            break;
                        }
                        watch_failures = 0;
                        // A change revives a pass that gave up.
                        if pass_failures >= SUPERVISOR_TURN_RETRY_LIMIT {
                            pass_failures = 0;
                        }
                    }
                    None => break,
                }
            }
            // Re-subscribe promptly after a transient failure, then scan to recover
            // any events missed during the gap. Pace retries to avoid a hot loop.
            watch_failures = watch_failures.saturating_add(1);
            tokio::time::sleep(SUPERVISOR_TURN_RETRY.delay(watch_failures)).await;
        }
    }));
    (task, ready_rx)
}

pub(super) fn spawn_provisioned_environment_reconciliation_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task("provisioned environments", interval, PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = reconcile_provisioned_environments(&state, &namespace).await {
                warn!(%error, "failed to reconcile provisioned environment registrations");
            }
        }
    })
}

pub(super) fn spawn_environment_orphan_sweep_task(state: Arc<ControllerRuntimeState>) -> JoinHandle<()> {
    use flotilla_provisioning::environment_orphans::{EnvironmentOrphanSweep, ORPHAN_SWEEP_INTERVAL};

    // Called only after StartupRestoration has completed. A retry of restoration
    // creates a new sweep, so neither startup nor daemon restarts inherit grace.
    let mut sweep = EnvironmentOrphanSweep::default();
    sweep.mark_ready();
    let sweep = Arc::new(Mutex::new(sweep));
    spawn_periodic_task("environment orphan sweep", ORPHAN_SWEEP_INTERVAL, PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        let sweep = Arc::clone(&sweep);
        async move {
            let Some((_, provider)) = state.local_registry.environment_providers.for_legacy_input(EnvironmentInput::Docker) else {
                return;
            };
            let runtime = DockerControllerRuntime { state: Arc::clone(&state) };
            let reads = state.daemon.lifecycle_reads();
            let swept = match reads.environments() {
                Ok(environments) => sweep.lock().await.sweep(environments, &**provider, &runtime, tokio::time::Instant::now()).await,
                Err(error) => Err(error),
            };
            if let Err(error) = swept {
                warn!(%error, host = %state.local_host_ref, "orphan environment sweep failed; will retry");
            }
        }
    })
}

/// Failed dispatch passes and subscriptions back off from one second to a minute; a pass gives up
/// after this many failures until the next change.
const DISPATCH_RETRY_LIMIT: u32 = 10;
const DISPATCH_RETRY: RetryBackoff = RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(60) };

/// Dispatch passes wake on board probe changes, on Project, Repository, WorkflowTemplate and Convoy
/// changes, and when a queue goes stale. Project and Repository changes also read every Project's
/// boards, which enrols their sources in the probe and retires removed ones.
pub(super) fn spawn_dispatch_reconciler_task(daemon: Arc<InProcessDaemon>, namespace: String) -> JoinHandle<()> {
    let issues = Arc::new(DaemonDispatchIssueSource::new(Arc::clone(&daemon))) as Arc<dyn DispatchIssueSource>;
    let backend = daemon.resource_backend();
    let reconciler = DispatchReconciler::new(backend.clone(), namespace.clone(), issues);
    tokio::spawn(attributed(caller("DispatchReconciler"), async move {
        let mut boards = daemon.dispatch_board_changes();
        let mut enrol = true;
        let mut pass_failures = 0_u32;
        let mut watch_failures = 0_u32;
        loop {
            // Subscribe before the pass so a change during it wakes the next one. Each event says
            // whether it changes the board sources.
            let mut watches = SelectAll::new();
            let mut subscribed = true;
            for (kind, sources) in [("Project", true), ("Repository", true), ("WorkflowTemplate", false), ("Convoy", false)] {
                match watch_resource_kind_including_replicas(&backend, &namespace, kind).await {
                    Ok(watch) => watches.push(
                        watch
                            .stream
                            .map(move |event| event.map(|_| sources))
                            .chain(futures::stream::once(async move { Err(ResourceError::other(format!("the {kind} watch ended"))) }))
                            .boxed(),
                    ),
                    Err(error) => {
                        warn!(%kind, %error, %namespace, "could not watch a dispatch input; resubscribing");
                        subscribed = false;
                        break;
                    }
                }
            }
            if !subscribed {
                watch_failures = watch_failures.saturating_add(1);
                tokio::time::sleep(DISPATCH_RETRY.delay(watch_failures)).await;
                enrol = true;
                continue;
            }
            let mut watches = watches.ready_chunks(64);
            loop {
                // Until every board has been read once, the probe's first observations wake this
                // pass to finish enrolling and retiring.
                if enrol {
                    match daemon.refresh_dispatch_boards_internal().await {
                        Ok(()) => enrol = false,
                        Err(error) => debug!(%error, "dispatch board observation unavailable"),
                    }
                }
                let (failed, due) = match reconciler.reconcile_once().await {
                    Ok(pass) => (pass.project_errors > 0, pass.next_due),
                    Err(error) => {
                        warn!(%error, "dispatch reconciliation pass failed");
                        (true, None)
                    }
                };
                pass_failures = if failed { pass_failures.saturating_add(1) } else { 0 };
                if pass_failures == DISPATCH_RETRY_LIMIT {
                    error!(%namespace, failures = pass_failures, "dispatch passes failed repeatedly; waiting for the next change");
                }
                let retry = (failed && pass_failures < DISPATCH_RETRY_LIMIT).then(|| DISPATCH_RETRY.delay(pass_failures));
                let due = due.map(|due| due.signed_duration_since(Utc::now()).to_std().unwrap_or_default());
                let wake = retry.into_iter().chain(due).min();
                tokio::select! {
                    events = watches.next() => match events {
                        Some(events) => {
                            if let Some(error) = events.iter().find_map(|event| event.as_ref().err()) {
                                warn!(%error, %namespace, "dispatch input watch failed; resubscribing");
                                break;
                            }
                            enrol |= events.into_iter().any(|event| matches!(event, Ok(true)));
                            watch_failures = 0;
                        }
                        None => break,
                    },
                    changed = boards.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = tokio::time::sleep(wake.unwrap_or_default()), if wake.is_some() => continue,
                }
                // A change revives a pass that gave up.
                if pass_failures >= DISPATCH_RETRY_LIMIT {
                    pass_failures = 0;
                }
            }
            // Re-subscribe after a failed or ended watch, then pass again to recover any events
            // missed during the gap.
            enrol = true;
            watch_failures = watch_failures.saturating_add(1);
            tokio::time::sleep(DISPATCH_RETRY.delay(watch_failures)).await;
        }
    }))
}

pub(super) async fn convoy_ensure_dependency_watches(
    backend: &ResourceBackend,
    namespace: &str,
    reads: &Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
) -> SelectAll<BoxStream<'static, Result<Value, ResourceError>>> {
    let mut watches = Vec::new();
    match reads.clones() {
        Ok(clones) => {
            let changed = futures::stream::unfold(clones.changes(), |mut changes| async move {
                match changes.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((Ok(Value::Null), changes)),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
                }
            });
            watches.push(Box::pin(changed) as BoxStream<'static, Result<Value, ResourceError>>);
        }
        Err(error) => warn!(kind = "Clone", %error, "could not watch standing convoy admission trigger"),
    }
    // An ended environment is the backing evidence a standing convoy's successor waits for.
    match reads.environments() {
        Ok(environments) => {
            let changed = futures::stream::unfold(environments.changes(), |mut changes| async move {
                match changes.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((Ok(Value::Null), changes)),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
                }
            });
            watches.push(Box::pin(changed) as BoxStream<'static, Result<Value, ResourceError>>);
        }
        Err(error) => warn!(kind = "Environment", %error, "could not watch standing convoy backing evidence"),
    }
    // Clone and ConvoyEnsure lifecycle changes are trigger dependencies:
    // renewed clone demand and ensure readmission can unblock work even
    // though their payloads are not part of the admission hash.
    for kind in [
        "Convoy",
        "ConvoyEnsure",
        "Project",
        "Repository",
        "WorkflowTemplate",
        "FulfilmentKind",
        "CredentialGrant",
        "CredentialSpec",
        "Host",
    ] {
        match watch_resource_kind_including_replicas(backend, namespace, kind).await {
            // A watch that ends is told as a failure, so the task subscribes again rather than
            // silently losing the kind's changes.
            Ok(watch) => watches.push(
                watch
                    .stream
                    .chain(futures::stream::once(async move { Err(ResourceError::other(format!("the {kind} watch ended"))) }))
                    .boxed(),
            ),
            Err(error) => warn!(%kind, %error, "could not watch standing convoy admission dependency"),
        }
    }
    // A person acknowledging a hold releases it on the next pass; holds are acknowledged at
    // their own home, the reconciler's.
    if let Some(changes) = backend.attentions().acknowledgements() {
        let acknowledged = futures::stream::unfold(changes, |mut changes| async move {
            loop {
                match changes.recv().await {
                    Ok(()) => return Some((Ok(Value::Null), changes)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        watches.push(Box::pin(acknowledged) as BoxStream<'static, Result<Value, ResourceError>>);
    }
    futures::stream::select_all(watches)
}

/// A failed ensure pass retries from one second to a minute, and after this many failures waits
/// for the next change.
const CONVOY_ENSURE_RETRY_LIMIT: u32 = 10;
const CONVOY_ENSURE_RETRY: RetryBackoff = RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(60) };

/// The ensure pass wakes on its dependencies' changes, at the time it last deferred an ensure to
/// (a strike or admission retry, or a backoff reset), and to retry its own failure.
pub(super) fn spawn_convoy_ensure_reconciler_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    overrun_after: Duration,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(attributed(caller("ConvoyEnsure"), async move {
        let backend = state.daemon.resource_backend();
        let dependencies = || async {
            let mut events = convoy_ensure_dependency_watches(&backend, &namespace, &state.daemon.lifecycle_reads()).await;
            // The local backing inventory latching is backing evidence too.
            let observed = futures::stream::unfold(state.local_backing_observed.subscribe(), |mut observed| async move {
                observed.changed().await.ok().map(|()| (Ok(Value::Null), observed))
            });
            events.push(Box::pin(observed) as BoxStream<'static, Result<Value, ResourceError>>);
            events
        };
        let mut dependency_events = dependencies().await;
        let clock = state.daemon.clock_for_scenarios();
        let mut jumps = clock.jumps();
        let mut failures = 0_u32;
        let mut watch_failures = 0_u32;
        loop {
            let result = run_watched_operation(
                overrun_after,
                state.daemon.reconcile_convoy_ensures_once_with_backing_inspector(&namespace, &*state),
                || {
                    runtime_health.report_convoy_ensure_timeout(overrun_after);
                    warn!(timeout_secs = overrun_after.as_secs(), "standing convoy ensure reconciliation pass overran its budget");
                },
            )
            .await;
            runtime_health.clear_convoy_ensure_timeout();
            let retry = match result {
                Ok(changes) => {
                    failures = 0;
                    info!(changes = changes.len(), "standing convoy ensure reconciler alive");
                    None
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if failures >= CONVOY_ENSURE_RETRY_LIMIT {
                        error!(%error, failures, "standing convoy ensure reconciliation keeps failing; waiting for the next change");
                        None
                    } else {
                        warn!(%error, failures, "standing convoy ensure reconciliation pass failed");
                        Some(tokio::time::Instant::now() + CONVOY_ENSURE_RETRY.delay(failures))
                    }
                }
            };
            let due = state.daemon.convoy_ensure_next_due(&namespace).await.unwrap_or_else(|error| {
                warn!(%error, "standing convoy ensure due time is unavailable");
                None
            });
            let mut restart_watches = dependency_events.is_empty();
            loop {
                // Measured on the daemon's clock, again whenever that clock jumps.
                let due_wait = due.map(|due| (due - clock.now()).to_std().unwrap_or_default());
                tokio::select! {
                    _ = tokio::time::sleep_until(retry.unwrap_or_else(tokio::time::Instant::now)), if retry.is_some() => {}
                    _ = tokio::time::sleep(due_wait.unwrap_or_default()), if due_wait.is_some() => {}
                    jumped = async { jumps.as_mut().expect("guarded").changed().await }, if jumps.is_some() && due.is_some() => {
                        if jumped.is_err() {
                            jumps = None;
                        }
                        continue;
                    }
                    event = dependency_events.next(), if !dependency_events.is_empty() => {
                        if let Some(Err(error)) = event {
                            warn!(%error, "standing convoy admission dependency watch failed");
                            restart_watches = true;
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        while let Some(Some(event)) = dependency_events.next().now_or_never() {
                            if let Err(error) = event {
                                warn!(%error, "standing convoy admission dependency watch failed");
                                restart_watches = true;
                            }
                        }
                        // A change revives a pass that gave up.
                        if failures >= CONVOY_ENSURE_RETRY_LIMIT {
                            failures = 0;
                        }
                    }
                    else => {
                        // Every dependency stream ended: re-arm them, paced.
                        watch_failures = watch_failures.saturating_add(1);
                        tokio::time::sleep(CONVOY_ENSURE_RETRY.delay(watch_failures)).await;
                        restart_watches = true;
                    }
                }
                break;
            }
            if restart_watches {
                // Reconciliation is level-triggered; re-arm dependencies
                // after overflow so subsequent changes still wake admission.
                dependency_events = dependencies().await;
            } else {
                watch_failures = 0;
            }
        }
    }))
}

#[derive(Clone, Copy)]
pub(super) enum PeriodicTaskStart {
    Immediate,
    AfterInterval,
}

pub(super) fn spawn_periodic_task<Operation, OperationFuture>(
    name: &'static str,
    interval: Duration,
    start: PeriodicTaskStart,
    mut operation: Operation,
) -> JoinHandle<()>
where
    Operation: FnMut() -> OperationFuture + Send + 'static,
    OperationFuture: Future<Output = ()> + Send + 'static,
{
    // Its engine reads count for `name`.
    tokio::spawn(attributed(caller(name), async move {
        let start = match start {
            PeriodicTaskStart::Immediate => tokio::time::Instant::now(),
            PeriodicTaskStart::AfterInterval => tokio::time::Instant::now() + interval,
        };
        let mut ticker = tokio::time::interval_at(start, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            operation().await;
        }
    }))
}

/// Runs `operation` to completion, calling `on_timeout` once it is still running after `timeout`.
/// The operation is never dropped part-way: a standing convoy pass admits a generation and records
/// it in separate writes, and a pass cut between them leaves the generation unrecorded, so its end
/// is never counted against the role's restart budget (#4310). The cost is liveness: a pass that
/// never returns holds the loop, with its timeout condition raised, until shutdown aborts the task.
pub(super) async fn run_watched_operation<Output, OperationFuture, OnTimeout>(
    timeout: Duration,
    operation: OperationFuture,
    on_timeout: OnTimeout,
) -> Output
where
    OperationFuture: Future<Output = Output>,
    OnTimeout: FnOnce(),
{
    let mut operation = std::pin::pin!(operation);
    match tokio::time::timeout(timeout, &mut operation).await {
        Ok(output) => output,
        Err(_) => {
            on_timeout();
            operation.await
        }
    }
}

pub(super) fn spawn_aggregator_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    state: AggregatorProjectionState,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    let issue_polling = runtime_health.issue_polling.clone();
    tokio::spawn(async move {
        supervise_controller(AGGREGATOR_CONTROLLER, supervision, runtime_health, move || {
            let daemon = Arc::clone(&daemon);
            let namespace = namespace.clone();
            let state = state.clone();
            let issue_polling = issue_polling.clone();
            async move {
                flotilla_projections::run(
                    flotilla_projections::AggregatorContext {
                        durable: daemon.resource_backend(),
                        discovered: daemon.discovered_checkouts(),
                        host_name: daemon.host_name().clone(),
                        event_sink: daemon.event_sink(),
                        events: daemon.subscribe(),
                        attach: daemon.clone(),
                        change_requests: daemon.clone(),
                        issues: daemon.clone(),
                        lifecycles: daemon.lifecycle_reads(),
                        convoy_engine: daemon.convoy_engine_path(),
                        agent_sessions: daemon.clone(),
                        standing_roles: daemon.clone(),
                        clock: daemon.clock_for_scenarios(),
                    },
                    &namespace,
                    state,
                    issue_polling,
                )
                .await
            }
        })
        .await;
    })
}

pub(crate) async fn wait_for_listening(ready: Option<watch::Receiver<bool>>) -> Result<(), watch::error::RecvError> {
    if let Some(mut ready) = ready {
        ready.wait_for(|ready| *ready).await?;
    }
    Ok(())
}

pub(super) fn spawn_startup_restoration(restoration: StartupRestoration) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = wait_for_listening(restoration.options.startup_ready.clone()).await {
            debug!(%error, "startup restoration cancelled because the server did not listen");
            return;
        }
        supervise_controller(
            "startup_restoration",
            restoration.options.controller_supervision.clone(),
            restoration.runtime_health.clone(),
            || async {
                // A panic must not silently lose environment/credential recovery.
                // Owned controller handles are aborted before supervision retries.
                // Each retry repeats the whole restoration, including the sweep.
                std::panic::AssertUnwindSafe(restoration.run())
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(ResourceError::other("startup restoration panicked")))
            },
        )
        .await;
    })
}
