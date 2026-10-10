//! Background task ownership, supervision, retention, and watch loops.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_controllers::reconcilers::checkout::runtime::sweep_host_empty_convoy_directories;
use flotilla_controllers::reconcilers::VesselPlacementProjector;
use flotilla_core::{
    aggregator_projection::AggregatorProjectionState, demand_lifecycle::DemandLifecycle, in_process::InProcessDaemon,
    vcs::REMOTE_CHECKOUT_ARCHIVE_SWEEP_TIMEOUT,
};
use flotilla_credentials::CredentialStore;
use flotilla_protocol::{CanonicalHostId, NodeId};
use flotilla_resources::{
    Checkout, Clone, Convoy, ForgeRead, ForgeReadHeartbeat, Host, HostStatusPatch, ReadWatchEvent, Resource, ResourceError,
    ResourceProvenance, RetryBackoff, SystemClock, TerminalSession, Vessel,
};
use flotilla_store::{watch_resource_kind, watch_resource_kind_including_replicas, ResourceBackend};
use futures::stream::{BoxStream, SelectAll};
use futures::FutureExt;
use futures::StreamExt;
use serde_json::Value;
use tokio::{
    sync::{oneshot, watch, Mutex},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use super::credentials::reconcile_work_credentials;
use super::discovery::{observe_fulfilment_facts, AgentlessSshProfile, BagEnvVars, FulfilmentProbeContext, LocalProvisioningProfile};
use super::docker::DockerControllerRuntime;
use super::environments::reconcile_provisioned_environments;
use super::health::{apply_host_heartbeat_with_credentials, DaemonHealthIdentity, RuntimeHealth};
use super::state::ControllerRuntimeState;
use super::StartupRestoration;
use crate::{
    blob_store::TieredBlobStore,
    dispatch_reconciler::{DaemonDispatchIssueSource, DispatchIssueSource, DispatchReconciler},
    resource_manifest::{materialize_bound_manifest_root, ResourceManifestReconciler},
    sleep_inhibitor,
    supervisor::{supervise, ControllerSupervision},
};

/// Cadence of the liveness marker (see `spawn_liveness_watchdog_task`). Long
/// enough to be quiet in a healthy log, short enough to bound how much time a
/// wedge can hide in.
pub(super) const LIVENESS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

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

pub(super) async fn run_artifact_gc(backend: ResourceBackend, namespace: String, blobs: Arc<TieredBlobStore>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let service = crate::artifact::ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
        match service.reap_expired().await {
            Ok(referenced) => {
                if let Err(error) = blobs.gc_unreferenced(&referenced, Duration::from_secs(24 * 60 * 60)).await {
                    warn!(%error, "artifact blob GC failed");
                }
            }
            Err(error) => warn!(%error, "artifact retention sweep failed"),
        }
    }
}

pub(super) async fn run_checkout_archive_gc(backend: ResourceBackend, namespace: String, archive_sweep: CheckoutArchiveSweep) {
    let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        {
            if let Some(host) = archive_sweep.daemon.local_host_id() {
                if let Err(error) = sweep_host_empty_convoy_directories(&backend, host.as_str()).await {
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
            match backend.clone().using::<Clone>(&namespace).list().await {
                Ok(clones) => {
                    for clone in clones.items {
                        if let Some(parent) = Path::new(&clone.spec.path).parent() {
                            roots.insert(CheckoutArchiveRoot { env_ref: clone.spec.env_ref, path: parent.join(".flotilla-archives") });
                        }
                    }
                }
                Err(error) => warn!(%error, "checkout archive sweep could not list clones"),
            }
            match backend.clone().using::<Checkout>(&namespace).list().await {
                Ok(checkouts) => {
                    for checkout in checkouts.items {
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
                        flotilla_core::vcs::prune_checkout_archives(&*runner, &root.path, archive_sweep.retention_days).await
                    } else {
                        flotilla_core::vcs::prune_remote_checkout_archives(
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

pub(super) fn spawn_blob_sync_status_task(
    store: Arc<TieredBlobStore>,
    backend: ResourceBackend,
    namespace: String,
    host_id: String,
) -> JoinHandle<()> {
    spawn_periodic_task(Duration::from_secs(5), PeriodicTaskStart::Immediate, move || {
        let store = Arc::clone(&store);
        let backend = backend.clone();
        let namespace = namespace.clone();
        let host_id = host_id.clone();
        async move {
            let status = store.status().await;
            let hosts = backend.using::<Host>(&namespace);
            if let Err(error) = flotilla_store::apply_status_patch(&hosts, &host_id, &HostStatusPatch::BlobSync { status }).await {
                warn!(%error, "failed to publish blob sync status");
            }
        }
    })
}

pub(super) fn spawn_manifest_reconciler_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    manifests: flotilla_core::config::ResourceManifestsConfig,
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
                materialize_bound_manifest_root(&daemon.resource_backend(), &namespace, &manifests).await.map_err(ResourceError::other)?;
                let vcs = if manifests.binding.is_some() {
                    daemon.local_charter_vcs()
                } else {
                    daemon.local_vcs_for_checkout(&manifests.dir).await
                }
                .map_err(ResourceError::other)?;
                ResourceManifestReconciler::new(daemon.resource_backend(), namespace, manifests.dir)
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
            let run = make_run();
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

pub(super) fn spawn_resource_controller<T, F, Fut>(
    backend: ResourceBackend,
    namespace: String,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
    mut make_run: F,
) -> JoinHandle<()>
where
    T: Resource,
    F: FnMut(ResourceBackend, String) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), ResourceError>> + Send + 'static,
{
    tokio::spawn(async move {
        supervise_controller(T::API_PATHS.kind, supervision, runtime_health, move || make_run(backend.clone(), namespace.clone())).await;
    })
}

pub(super) fn spawn_vessel_placement_projector(
    backend: ResourceBackend,
    namespace: String,
    local_host_ref: String,
    additional_host_refs: Vec<CanonicalHostId>,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // The projector and the Vessel reconciler need distinct health keys, while both remain tied to the resource they manage.
        supervise_controller(Vessel::API_PATHS.plural, supervision, runtime_health, move || {
            let projector =
                VesselPlacementProjector::new(backend.clone(), namespace.clone(), CanonicalHostId::resolved(local_host_ref.clone()))
                    .with_additional_host_refs(additional_host_refs.clone());
            async move { projector.run().await }
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

pub(super) fn spawn_local_fulfilment_probe_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    scratch: PathBuf,
) -> JoinHandle<()> {
    spawn_periodic_task(FULFILMENT_CHANGE_CHECK_INTERVAL, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let profile = profile.clone();
        let scratch = scratch.clone();
        async move {
            let discovery = daemon.discovery_runtime();
            let hosts = daemon.resource_backend().using::<Host>(&namespace);
            let status = hosts.get(&profile.host_id).await.ok().and_then(|host| host.status).unwrap_or_default();
            let previous = status.fulfilment_facts;
            let mut model_probes = status.model_probes.clone();
            match observe_fulfilment_facts(
                &daemon.resource_backend(),
                &namespace,
                &profile.host_id,
                &profile.available_pools,
                &previous,
                FulfilmentProbeContext { runner: discovery.runner.as_ref(), env: discovery.env.as_ref(), scratch: &scratch },
                &mut model_probes,
            )
            .await
            {
                Ok(facts) if facts != previous || model_probes != status.model_probes => {
                    if let Err(error) = flotilla_store::apply_status_patch(
                        &hosts,
                        &profile.host_id,
                        &HostStatusPatch::FulfilmentFacts { facts, model_probes },
                    )
                    .await
                    {
                        warn!(%error, "failed to publish observed fulfilment facts");
                    }
                }
                Ok(_) => {}
                Err(error) => warn!(%error, "failed to observe local fulfilment facts"),
            }
        }
    })
}

pub(super) fn spawn_ssh_fulfilment_probe_task(daemon: Arc<InProcessDaemon>, namespace: String, ssh: AgentlessSshProfile) -> JoinHandle<()> {
    spawn_periodic_task(FULFILMENT_CHANGE_CHECK_INTERVAL, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let ssh = ssh.clone();
        async move {
            let hosts = daemon.resource_backend().using::<Host>(&namespace);
            let status = hosts.get(&ssh.provisioning.host_id).await.ok().and_then(|host| host.status).unwrap_or_default();
            let previous = status.fulfilment_facts;
            let mut model_probes = status.model_probes.clone();
            let scratch = match ssh.runner.writable_config_base(None, Path::new("/tmp")).await {
                Ok(base) => base.join("probe-cwd"),
                Err(error) => {
                    warn!(%error, "cannot select SSH probe directory");
                    return;
                }
            };
            match observe_fulfilment_facts(
                &daemon.resource_backend(),
                &namespace,
                &ssh.provisioning.host_id,
                &ssh.provisioning.available_pools,
                &previous,
                FulfilmentProbeContext { runner: ssh.runner.as_ref(), env: &BagEnvVars(&ssh.env_bag), scratch: &scratch },
                &mut model_probes,
            )
            .await
            {
                Ok(facts) if facts != previous || model_probes != status.model_probes => {
                    *ssh.fulfilment_facts.write().await = facts.clone();
                    if let Err(error) = flotilla_store::apply_status_patch(
                        &hosts,
                        &ssh.provisioning.host_id,
                        &HostStatusPatch::FulfilmentFacts { facts, model_probes },
                    )
                    .await
                    {
                        warn!(host = %ssh.provisioning.host_id, %error, "failed to publish observed SSH fulfilment facts");
                    }
                }
                Ok(_) => {}
                Err(error) => warn!(host = %ssh.provisioning.host_id, %error, "failed to observe SSH fulfilment facts"),
            }
        }
    })
}

#[cfg(test)]
pub(super) fn spawn_heartbeat_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    health: DaemonHealthIdentity,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_heartbeat_task_with_credentials(daemon, namespace, profile, Arc::new(None), health, RuntimeHealth::default(), interval)
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
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let profile = profile.clone();
        let credential_store = Arc::clone(&credential_store);
        let health = health.clone();
        let runtime_health = runtime_health.clone();
        async move {
            if let Err(err) =
                apply_host_heartbeat_with_credentials(&daemon, &namespace, &profile, credential_store.as_deref(), &health, &runtime_health)
                    .await
            {
                warn!(%err, "failed to publish host heartbeat");
            }
        }
    })
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
    tokio::spawn(async move {
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
    })
}

/// Reconcile durable supervisor turns on resource changes, with periodic recovery.
/// The caller owns the returned task and must abort it when the daemon stops.
/// Readiness resolves after both watches subscribe and the startup scan completes.
#[doc(hidden)]
pub fn spawn_pending_supervisor_turn_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    interval: Duration,
) -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let backend = daemon.resource_backend();
    let watch_namespace = namespace.clone();
    spawn_pending_supervisor_turn_task_with_watches(daemon, namespace, interval, move || {
        let backend = backend.clone();
        let namespace = watch_namespace.clone();
        async move {
            let convoys = backend.including_replicas::<Convoy>(&namespace).watch().await?;
            let sessions = backend.including_replicas::<TerminalSession>(&namespace).watch().await?;
            let messages = backend.including_replicas::<flotilla_resources::Message>(&namespace).watch().await?;
            Ok((
                futures::stream::select(convoys.map(|event| event.map(|_| ())), messages.map(|event| event.map(|_| ()))).boxed(),
                sessions.map(|event| event.map(|_| ())).boxed(),
            ))
        }
    })
}

pub(super) type SupervisorTurnWatches = (BoxStream<'static, Result<(), ResourceError>>, BoxStream<'static, Result<(), ResourceError>>);

// Watch subscription is the notification boundary. Tests can close one input
// without changing the resource stores or the reconciliation they exercise.
#[visibility::make(pub)]
#[doc(hidden)]
pub(super) fn spawn_pending_supervisor_turn_task_with_watches<F, Fut>(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    interval: Duration,
    mut subscribe: F,
) -> (JoinHandle<()>, oneshot::Receiver<()>)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<SupervisorTurnWatches, ResourceError>> + Send,
{
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let retry_backoff = RetryBackoff { initial: interval.min(Duration::from_secs(1)), maximum: interval };
        let mut failures = 0_u32;
        let mut last_watch_warning = None;
        let mut report_watch_failure = |error: &ResourceError| {
            let now = tokio::time::Instant::now();
            if last_watch_warning.is_none_or(|last| now.duration_since(last) >= interval) {
                warn!(%error, %namespace, "supervisor turn watch failed; resubscribing");
                last_watch_warning = Some(now);
            } else {
                debug!(%error, %namespace, "supervisor turn watch retry failed");
            }
        };
        let mut last_reconcile_warning = None;
        let mut report_reconcile_failure = |error: &str| {
            let now = tokio::time::Instant::now();
            if last_reconcile_warning.is_none_or(|last| now.duration_since(last) >= interval) {
                warn!(%error, %namespace, "failed to reconcile pending supervisor turns");
                last_reconcile_warning = Some(now);
            } else {
                debug!(%error, %namespace, "pending supervisor turn reconciliation still failing");
            }
        };
        let mut resync = tokio::time::interval(interval);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate tick; the subscribed startup scan handles recovery once.
        resync.tick().await;
        loop {
            let (mut convoys, mut sessions) = match subscribe().await {
                Ok((convoys, sessions)) => (convoys.ready_chunks(64), sessions.ready_chunks(64)),
                Err(error) => {
                    report_watch_failure(&error);
                    // A broken watch must not disable the periodic recovery pass.
                    if let Err(error) = daemon.reconcile_pending_supervisor_turns_once(&namespace).await {
                        report_reconcile_failure(&error);
                    }
                    failures = failures.saturating_add(1);
                    tokio::time::sleep(retry_backoff.delay(failures)).await;
                    continue;
                }
            };
            // Subscribe to both inputs before scanning to preserve updates during a pass.
            // Invariant: a no-op reconcile_pending_supervisor_turns_once must issue no
            // writes, or its own watch events would keep this loop running.
            loop {
                if let Err(error) = daemon.reconcile_pending_supervisor_turns_once(&namespace).await {
                    report_reconcile_failure(&error);
                }
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(());
                }
                let events = tokio::select! {
                    events = convoys.next() => events,
                    events = sessions.next() => events,
                    _ = resync.tick() => {
                        failures = 0;
                        continue;
                    },
                };
                match events {
                    Some(events) => {
                        if let Some(error) = events.into_iter().find_map(Result::err) {
                            report_watch_failure(&error);
                            break;
                        }
                        failures = 0;
                    }
                    None => break,
                }
            }
            // Re-subscribe promptly after a transient failure, then scan to recover
            // any events missed during the gap. Pace retries to avoid a hot loop.
            failures = failures.saturating_add(1);
            tokio::time::sleep(retry_backoff.delay(failures)).await;
        }
    });
    (task, ready_rx)
}

pub(super) fn spawn_demand_expiry_task(backend: ResourceBackend, namespace: String, interval: Duration) -> JoinHandle<()> {
    let lifecycle = Arc::new(DemandLifecycle::new(backend, Arc::new(SystemClock)));
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let lifecycle = Arc::clone(&lifecycle);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = lifecycle.expire_due(&namespace).await {
                warn!(%error, %namespace, "failed to expire demands");
            }
        }
    })
}

pub(super) fn spawn_event_expiry_task(backend: ResourceBackend, namespace: String, interval: Duration) -> JoinHandle<()> {
    let recorder = flotilla_store::EventRecorder::new(backend);
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let recorder = recorder.clone();
        let namespace = namespace.clone();
        async move {
            if let Err(error) = recorder.prune_expired(&namespace, Utc::now()).await {
                warn!(%error, %namespace, "failed to prune expired events");
            }
        }
    })
}

pub(super) fn spawn_adopted_checkout_reconciliation_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = daemon.reconcile_adopted_checkouts(&namespace).await {
                warn!(%error, "failed to reconcile adopted checkout observations");
            }
        }
    })
}

pub(super) fn spawn_provisioned_environment_reconciliation_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = reconcile_provisioned_environments(&state, &namespace).await {
                warn!(%error, "failed to reconcile provisioned environment registrations");
            }
            if let Err(error) = reconcile_work_credentials(&state, &namespace).await {
                warn!(%error, "failed to reconcile work credentials");
            }
        }
    })
}

pub(super) fn spawn_environment_orphan_sweep_task(state: Arc<ControllerRuntimeState>) -> JoinHandle<()> {
    use crate::environment_orphans::{EnvironmentOrphanSweep, ORPHAN_SWEEP_INTERVAL};

    // Called only after StartupRestoration has completed. A retry of restoration
    // creates a new sweep, so neither startup nor daemon restarts inherit grace.
    let mut sweep = EnvironmentOrphanSweep::default();
    sweep.mark_ready();
    let sweep = Arc::new(Mutex::new(sweep));
    spawn_periodic_task(ORPHAN_SWEEP_INTERVAL, PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        let sweep = Arc::clone(&sweep);
        async move {
            let Some((_, provider)) =
                state.local_registry.environment_providers.for_kind(flotilla_core::providers::environment::EnvironmentKind::Docker)
            else {
                return;
            };
            let runtime = DockerControllerRuntime { state: Arc::clone(&state) };
            if let Err(error) =
                sweep.lock().await.sweep(&state.daemon.resource_backend(), &**provider, &runtime, tokio::time::Instant::now()).await
            {
                warn!(%error, host = %state.local_host_ref, "orphan environment sweep failed; will retry");
            }
        }
    })
}

pub(super) fn spawn_dispatch_reconciler_task(daemon: Arc<InProcessDaemon>, namespace: String, interval: Duration) -> JoinHandle<()> {
    let issues = Arc::new(DaemonDispatchIssueSource::new(Arc::clone(&daemon))) as Arc<dyn DispatchIssueSource>;
    let reconciler = Arc::new(DispatchReconciler::new(daemon.resource_backend(), namespace, issues));
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let reconciler = Arc::clone(&reconciler);
        async move {
            if let Err(error) = reconciler.reconcile_once().await {
                warn!(%error, "dispatch reconciliation pass failed");
            }
        }
    })
}

pub(super) async fn convoy_ensure_dependency_watches(
    backend: &ResourceBackend,
    namespace: &str,
) -> SelectAll<BoxStream<'static, Result<Value, ResourceError>>> {
    let mut watches = Vec::new();
    match watch_resource_kind(backend, namespace, "Clone").await {
        Ok(watch) => watches.push(watch.stream),
        Err(error) => warn!(kind = "Clone", %error, "could not watch standing convoy admission trigger"),
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
        "PlacementPolicy",
        "CredentialGrant",
        "CredentialSpec",
        "Host",
    ] {
        match watch_resource_kind_including_replicas(backend, namespace, kind).await {
            Ok(watch) => watches.push(watch.stream),
            Err(error) => warn!(%kind, %error, "could not watch standing convoy admission dependency"),
        }
    }
    futures::stream::select_all(watches)
}

pub(super) fn spawn_convoy_ensure_reconciler_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    interval: Duration,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let backend = state.daemon.resource_backend();
        let mut dependency_events = convoy_ensure_dependency_watches(&backend, &namespace).await;
        let mut resync = tokio::time::interval(interval);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let mut restart_watches = dependency_events.is_empty();
            tokio::select! {
                _ = resync.tick() => {}
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
                }
            }
            if restart_watches {
                // Reconciliation below is level-triggered; re-arm dependencies
                // after overflow so subsequent changes still wake admission.
                dependency_events = convoy_ensure_dependency_watches(&backend, &namespace).await;
            }
            let result = run_bounded_operation(
                interval,
                state.daemon.reconcile_convoy_ensures_once_with_backing_inspector(&namespace, &*state),
                || {
                    runtime_health.report_convoy_ensure_timeout(interval);
                    warn!(timeout_secs = interval.as_secs(), "standing convoy ensure reconciliation pass timed out");
                },
            )
            .await;
            if let Some(result) = result {
                runtime_health.clear_convoy_ensure_timeout();
                match result {
                    Ok(changes) => info!(changes = changes.len(), "standing convoy ensure reconciler alive"),
                    Err(error) => warn!(%error, "standing convoy ensure reconciliation pass failed"),
                }
            }
        }
    })
}

#[derive(Clone, Copy)]
pub(super) enum PeriodicTaskStart {
    Immediate,
    AfterInterval,
}

/// Emits one liveness line per interval so a daemon that stops scheduling is
/// visible in the log by the *absence* of a known-cadence marker, rather than by
/// the absence of incidental chatter. Added after flotilla#1111, where the only
/// evidence of a wedged daemon was that unrelated debug lines stopped.
pub(super) fn spawn_liveness_watchdog_task(tasks: usize, interval: Duration) -> JoinHandle<()> {
    let started = tokio::time::Instant::now();
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || async move {
        info!(uptime_secs = started.elapsed().as_secs(), supervisory_tasks = tasks, "daemon alive");
    })
}

pub(super) fn spawn_periodic_task<Operation, OperationFuture>(
    interval: Duration,
    start: PeriodicTaskStart,
    mut operation: Operation,
) -> JoinHandle<()>
where
    Operation: FnMut() -> OperationFuture + Send + 'static,
    OperationFuture: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
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
    })
}

pub(super) async fn run_bounded_operation<Output, OperationFuture, OnTimeout>(
    timeout: Duration,
    operation: OperationFuture,
    on_timeout: OnTimeout,
) -> Option<Output>
where
    OperationFuture: Future<Output = Output>,
    OnTimeout: FnOnce(),
{
    match tokio::time::timeout(timeout, operation).await {
        Ok(output) => Some(output),
        Err(_) => {
            on_timeout();
            None
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
        supervise_controller("aggregator", supervision, runtime_health, move || {
            let daemon = Arc::clone(&daemon);
            let namespace = namespace.clone();
            let state = state.clone();
            let issue_polling = issue_polling.clone();
            async move { flotilla_aggregator::run(daemon, &namespace, state, issue_polling).await }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DemandKind {
    Read,
    Heartbeat,
}

struct DemandEvent {
    kind: DemandKind,
    provenance: ResourceProvenance,
    name: String,
    demanded_at: Option<DateTime<Utc>>,
}

fn demand_event<R: Resource>(event: ReadWatchEvent<R>, kind: DemandKind, timestamp: impl FnOnce(&R::Spec) -> DateTime<Utc>) -> DemandEvent {
    match event {
        ReadWatchEvent::Added(record) | ReadWatchEvent::Modified(record) => DemandEvent {
            kind,
            demanded_at: Some(timestamp(&record.object.spec)),
            provenance: record.provenance,
            name: record.object.metadata.name,
        },
        ReadWatchEvent::Deleted(record) => {
            DemandEvent { kind, provenance: record.provenance, name: record.object.metadata.name, demanded_at: None }
        }
        ReadWatchEvent::DeletedByName { tombstone, provenance } => {
            DemandEvent { kind, provenance, name: tombstone.name, demanded_at: None }
        }
    }
}

#[derive(Default)]
struct DemandDedup {
    seen: BTreeMap<(DemandKind, NodeId, String), DateTime<Utc>>,
}

impl DemandDedup {
    fn accept(&mut self, event: DemandEvent) -> Option<String> {
        let ResourceProvenance::Replica { origin_root, .. } = event.provenance else { return None };
        let key = (event.kind, origin_root, event.name.clone());
        let Some(demanded_at) = event.demanded_at else {
            self.seen.remove(&key);
            return None;
        };
        if self.seen.get(&key).is_some_and(|previous| *previous >= demanded_at) {
            return None;
        }
        self.seen.insert(key, demanded_at);
        // Replayed demand after eviction may retry servicing. ForgeReads' shared
        // request lock and 60-second freshness cache coalesce provider calls.
        if self.seen.len() > 4096 {
            self.seen.clear();
        }
        Some(event.name)
    }
}

async fn forge_demand_events(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<BoxStream<'static, Result<DemandEvent, ResourceError>>, ResourceError> {
    let reads = backend.including_replicas::<ForgeRead>(namespace).watch().await?;
    let pulses = backend.including_replicas::<ForgeReadHeartbeat>(namespace).watch().await?;
    let reads = reads
        .chain(futures::stream::once(async { Err(ResourceError::other("ForgeRead watch closed")) }))
        .map(|event| event.map(|event| demand_event(event, DemandKind::Read, |spec| spec.demanded_at)));
    let pulses = pulses
        .chain(futures::stream::once(async { Err(ResourceError::other("ForgeReadHeartbeat watch closed")) }))
        .map(|event| event.map(|event| demand_event(event, DemandKind::Heartbeat, |spec| spec.demanded_at)));
    Ok(futures::stream::select(reads, pulses).boxed())
}

/// Watch-driven forge demand service. The board tick retains periodic recovery.
/// Readiness means both replicated-record watches are subscribed.
pub fn spawn_forge_read_demand_task(daemon: Arc<InProcessDaemon>, namespace: String) -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        loop {
            let backend = daemon.resource_backend();
            match forge_demand_events(&backend, &namespace).await {
                Ok(events) => {
                    if let Some(ready) = ready_tx.take() {
                        let _ = ready.send(());
                    }
                    // Watches start at now, without an initial kind replay. Reset
                    // on resubscribe so a retransmitted demand can retry; missed
                    // events retain the periodic board tick as recovery.
                    let mut dedup = DemandDedup::default();
                    let demands = events
                        .take_while(|event| {
                            if let Err(error) = event {
                                warn!(%error, "forge demand watch failed; resubscribing");
                            }
                            futures::future::ready(event.is_ok())
                        })
                        .filter_map(|event| futures::future::ready(event.ok().and_then(|event| dedup.accept(event))));
                    demands
                        .for_each_concurrent(8, |name| {
                            let daemon = daemon.clone();
                            let namespace = namespace.clone();
                            async move {
                                if let Err(error) = daemon.service_forge_read_demand(&namespace, &name).await {
                                    // Identical timestamps remain deduplicated;
                                    // renewal, resubscribe or the board tick retries.
                                    debug!(%error, %name, "forge demand unavailable");
                                }
                            }
                        })
                        .await;
                }
                Err(error) => warn!(%error, "forge demand watch subscription failed"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    (task, ready_rx)
}

#[cfg(test)]
mod forge_demand_tests {
    use flotilla_protocol::IssueSource;
    use flotilla_resources::{ForgeReadRequest, ForgeReadSpec, InputMeta, ReadResourceObject, ResourceTombstone};

    use super::*;

    // Eviction caps memory and permits servicing again rather than dropping work.
    #[test]
    fn demand_dedup_eviction_preserves_service() {
        let now = Utc::now();
        let event = |name: String| DemandEvent {
            kind: DemandKind::Read,
            provenance: ResourceProvenance::Replica { origin_root: NodeId::new("origin"), last_synced_at: now },
            name,
            demanded_at: Some(now),
        };
        let mut dedup = DemandDedup::default();
        for index in 0..4097 {
            let name = format!("read-{index}");
            assert_eq!(dedup.accept(event(name.clone())), Some(name));
            assert!(dedup.seen.len() <= 4096);
        }
        assert_eq!(dedup.accept(event("read-0".into())), Some("read-0".into()));
    }

    // Both deletion representations release the same replica dedup key.
    #[tokio::test]
    async fn demand_deletion_events_release_key() {
        let backend = ResourceBackend::InMemory(flotilla_store::InMemoryBackend::default());
        let now = Utc::now();
        let object = backend
            .using::<ForgeRead>("flotilla")
            .create(
                &InputMeta::builder().name("read".into()).build(),
                &ForgeReadSpec {
                    source: IssueSource { service: "github".into(), scope: "shared".into() },
                    request: ForgeReadRequest::Board,
                    demanded_at: now,
                },
            )
            .await
            .unwrap();
        let provenance = ResourceProvenance::Replica { origin_root: NodeId::new("origin"), last_synced_at: now };
        let record = ReadResourceObject { object, provenance: provenance.clone() };
        let mut dedup = DemandDedup::default();
        for deleted in [
            ReadWatchEvent::Deleted(record.clone()),
            ReadWatchEvent::DeletedByName {
                tombstone: ResourceTombstone {
                    name: "read".into(),
                    namespace: "flotilla".into(),
                    resource_version: "2".into(),
                    annotations: BTreeMap::new(),
                },
                provenance,
            },
        ] {
            let added = || demand_event(ReadWatchEvent::Added(record.clone()), DemandKind::Read, |spec| spec.demanded_at);
            assert_eq!(dedup.accept(added()), Some("read".into()));
            assert_eq!(dedup.accept(added()), None);
            assert_eq!(dedup.accept(demand_event(deleted, DemandKind::Read, |spec| spec.demanded_at)), None);
        }
        assert_eq!(
            dedup.accept(demand_event(ReadWatchEvent::Modified(record), DemandKind::Read, |spec| spec.demanded_at)),
            Some("read".into())
        );
    }

    // Duplicate status events do not service again. Renewals, independent
    // origins/kinds, deletion and retransmission after a new subscription do.
    #[test]
    fn demand_dedup_tracks_renewal_deletion_and_retransmission() {
        let now = Utc::now();
        let origin = NodeId::new("origin");
        let event = |kind, demanded_at| DemandEvent {
            kind,
            provenance: ResourceProvenance::Replica { origin_root: origin.clone(), last_synced_at: now },
            name: "read".into(),
            demanded_at,
        };
        let mut dedup = DemandDedup::default();
        assert_eq!(dedup.accept(event(DemandKind::Read, Some(now))), Some("read".into()));
        assert_eq!(dedup.accept(event(DemandKind::Read, Some(now))), None);
        assert_eq!(dedup.accept(event(DemandKind::Read, Some(now - chrono::Duration::seconds(1)))), None);
        assert_eq!(dedup.accept(event(DemandKind::Heartbeat, Some(now))), Some("read".into()));
        assert_eq!(dedup.accept(event(DemandKind::Read, Some(now + chrono::Duration::seconds(1)))), Some("read".into()));
        assert_eq!(dedup.accept(event(DemandKind::Read, None)), None);
        assert_eq!(dedup.accept(event(DemandKind::Read, Some(now))), Some("read".into()));
        let mut other_origin = event(DemandKind::Read, Some(now));
        other_origin.provenance = ResourceProvenance::Replica { origin_root: NodeId::new("other-origin"), last_synced_at: now };
        assert_eq!(dedup.accept(other_origin), Some("read".into()));
        let mut local = event(DemandKind::Read, Some(now));
        local.provenance = ResourceProvenance::Local;
        assert_eq!(dedup.accept(local), None);
        let mut resubscribed = DemandDedup::default();
        assert_eq!(resubscribed.accept(event(DemandKind::Read, Some(now))), Some("read".into()));
    }
}
