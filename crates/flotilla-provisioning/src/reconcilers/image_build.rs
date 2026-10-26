//! Executes per-identity ImageBuilds on their builder host through the
//! provider-neutral [`ImageBuildService`] port (ADR 0062).
//!
//! The reconciler acts only on builds whose `host_ref` is this host and whose
//! architecture this host builds. It publishes `Start` (taking one of the
//! host's build slots) before any build process runs, runs the build as a
//! background job, and records `Built` or `Failed` with every layer's probe
//! outcome and the build log artifact. Infrastructure failures wait out
//! exponential backoff while attempts remain; recipe failures
//! and exhausted attempts stay `Failed` and name the owner of the failing
//! layer. Retired ADR 0053 records are deleted.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_environment::environment::{
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest, ImageDeliveryTarget, SecretFile},
    ImageBuildService, RegistryAuth,
};
use flotilla_protocol::LifecycleReading;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::{
    Clock, Host, ImageBuild, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase, ImageBuildSpec,
    ImageBuildStatus, ImageBuildStatusPatch, ImageDelivery, ImageLayerSource, ImageProbeOutcome, ResourceError, ResourceObject,
    StatusPatch, SystemClock,
};
use flotilla_store::{
    controller::{ReconcileOutcome, Reconciler},
    ResourceBackend, TypedResolver,
};
use tokio::{sync::Mutex, task::JoinHandle};

/// Host-held materials a build needs and the place its log is kept. The
/// daemon implements it with its credential store and blob store; crews
/// never see any of it.
#[async_trait]
pub trait ImageBuildMaterials: Send + Sync {
    /// The push credential for `repository`, resolved freshly for one attempt.
    async fn push_auth(&self, repository: &str) -> Result<RegistryAuth, ImageBuildFailure>;
    /// A private file holding the named build secret's material. Unknown or
    /// ungranted names are recipe failures naming the secret.
    async fn secret(&self, name: &str) -> Result<SecretFile, ImageBuildFailure>;
    /// Keep one attempt's build log and return its reference.
    async fn keep_log(&self, build: &str, attempt: u32, log: &str) -> Result<String, String>;
}

/// The registry build cache for builds pushed to `repository` (ADR 0062
/// decision C4: `…/buildcache` beside the images), one tag per architecture
/// so concurrent builders of different architectures do not overwrite each
/// other's cache manifest.
pub fn registry_build_cache(repository: &str, architecture: &str) -> String {
    format!("{repository}/buildcache:{architecture}")
}

/// Delay before attempt `attempt + 1` after an infrastructure failure of
/// attempt `attempt` (1-based): 30 s doubling, capped at 30 minutes.
pub fn image_build_retry_backoff(attempt: u32) -> chrono::Duration {
    let seconds = 30_i64.saturating_mul(1_i64 << attempt.saturating_sub(1).min(16));
    chrono::Duration::seconds(seconds.min(30 * 60))
}

/// How often a running job checks that its ImageBuild still exists; a
/// deleted build (the operator retry, or a withdrawn demand) aborts its job.
const DEFAULT_JOB_LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

/// Requeue interval while this host holds work for a build.
const BUSY_REQUEUE: Duration = Duration::from_secs(1);

struct JobResult {
    outcome: Result<BuiltImage, FailedImageBuild>,
    log_ref: Result<String, String>,
}

struct Job {
    attempt: u32,
    handle: JoinHandle<Option<JobResult>>,
}

pub struct ImageBuildReconciler {
    service: Option<Arc<dyn ImageBuildService>>,
    materials: Arc<dyn ImageBuildMaterials>,
    builds: TypedResolver<ImageBuild>,
    hosts: TypedResolver<Host>,
    local_host_ref: String,
    probe_wrapper: Arc<str>,
    clock: Arc<dyn Clock>,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    liveness_interval: Duration,
    execution: Arc<Mutex<()>>,
    jobs: Mutex<BTreeMap<String, Job>>,
}

impl ImageBuildReconciler {
    /// `service` is `None` on hosts with no image build capability: they
    /// still garbage-collect retired records, but execute nothing.
    pub fn new(
        service: Option<Arc<dyn ImageBuildService>>,
        materials: Arc<dyn ImageBuildMaterials>,
        backend: &ResourceBackend,
        namespace: &str,
        local_host_ref: &str,
        probe_wrapper: &str,
    ) -> Self {
        Self {
            service,
            materials,
            builds: backend.using(namespace),
            hosts: backend.using(namespace),
            local_host_ref: local_host_ref.into(),
            probe_wrapper: probe_wrapper.into(),
            clock: Arc::new(SystemClock),
            lifecycle_reads: Arc::new(NoLifecycleReads),
            liveness_interval: DEFAULT_JOB_LIVENESS_INTERVAL,
            execution: Arc::new(Mutex::new(())),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.builds = self.builds.clone().with_lifecycle_status_reads(Arc::clone(&reads));
        self.lifecycle_reads = reads;
        self
    }

    async fn reading(&self, obj: &ResourceObject<ImageBuild>) -> Result<Option<LifecycleReading>, String> {
        let reads = Arc::clone(&self.lifecycle_reads);
        let name = obj.metadata.name.clone();
        tokio::task::spawn_blocking(move || reads.try_reading(ProvisioningKind::ImageBuild, &name, None))
            .await
            .map_err(|error| error.to_string())?
    }

    pub fn with_liveness_interval(mut self, interval: Duration) -> Self {
        self.liveness_interval = interval;
        self
    }

    /// Build slots this host offers for `architecture`, or `None` when it
    /// does not build that architecture. An undeclared capacity is the
    /// single-host local mode default of one slot.
    async fn slots(&self, architecture: &str) -> Result<Option<u32>, ResourceError> {
        let host = match self.hosts.get(&self.local_host_ref).await {
            Ok(host) => host,
            Err(ResourceError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        if flotilla_resources::image_architecture::host_architecture(&host).is_some_and(|actual| actual != architecture) {
            return Ok(None);
        }
        Ok(match host.spec.image_build_capacity {
            None => Some(1),
            Some(ImageBuildCapacity::None) => None,
            Some(ImageBuildCapacity::Builder { architecture: builds, slots, .. }) => {
                (flotilla_resources::image_architecture::canonical_image_architecture(&builds) == architecture && slots > 0)
                    .then_some(slots)
            }
        })
    }

    async fn building_here(&self, except: &str) -> Result<usize, ResourceError> {
        let builds = self.builds.list().await?.items;
        let reads = Arc::clone(&self.lifecycle_reads);
        let local_host = self.local_host_ref.clone();
        let except = except.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut building = 0;
            for build in builds {
                if build.metadata.name != except && build.spec.current().is_some_and(|spec| spec.host_ref == local_host) {
                    let reading =
                        reads.try_reading(ProvisioningKind::ImageBuild, &build.metadata.name, None).map_err(ResourceError::other)?;
                    if progress(&build.status.unwrap_or_default(), reading.as_ref()).phase == ImageBuildPhase::Building {
                        building += 1;
                    }
                }
            }
            Ok(building)
        })
        .await
        .map_err(|error| ResourceError::other(error.to_string()))?
    }

    fn spawn_job(&self, service: Arc<dyn ImageBuildService>, obj: &ResourceObject<ImageBuild>, spec: ImageBuildSpec, attempt: u32) -> Job {
        let materials = Arc::clone(&self.materials);
        let builds = self.builds.clone();
        let name = obj.metadata.name.clone();
        let created = obj.metadata.creation_timestamp;
        let wrapper = Arc::clone(&self.probe_wrapper);
        let interval = self.liveness_interval;
        let handle = tokio::spawn(async move {
            let work = async {
                let mut log = String::new();
                let outcome = execute(service.as_ref(), materials.as_ref(), &spec, &wrapper, &mut log).await;
                let log_ref = materials.keep_log(&name, attempt, &log).await;
                JobResult { outcome, log_ref }
            };
            // A deleted ImageBuild (or a re-created one under the same
            // name) aborts its job: nothing would record the result.
            let deleted = async {
                loop {
                    tokio::time::sleep(interval).await;
                    match builds.get(&name).await {
                        Ok(current) if current.metadata.creation_timestamp == created && current.metadata.deletion_timestamp.is_none() => {}
                        Ok(_) | Err(ResourceError::NotFound { .. }) => return,
                        Err(_) => {}
                    }
                }
            };
            tokio::select! {
                result = work => Some(result),
                () = deleted => {
                    tracing::info!(image_build = %name, "image build deleted; abandoning its running attempt");
                    None
                }
            }
        });
        Job { attempt, handle }
    }
}

/// Every layer's probe outcome for an attempt that never reached its probes.
fn not_run_probes(spec: &ImageBuildSpec) -> Vec<flotilla_resources::ImageLayerProbe> {
    spec.layers
        .iter()
        .map(|layer| flotilla_resources::ImageLayerProbe { layer: layer.name.clone(), outcome: ImageProbeOutcome::NotRun })
        .collect()
}

/// Resolve this attempt's materials and drive the port. Only the secrets the
/// layers declare are resolved and passed.
async fn execute(
    service: &dyn ImageBuildService,
    materials: &dyn ImageBuildMaterials,
    spec: &ImageBuildSpec,
    probe_wrapper: &str,
    log: &mut String,
) -> Result<BuiltImage, FailedImageBuild> {
    let refuse = |failure: ImageBuildFailure| FailedImageBuild { failure, probes: not_run_probes(spec), failed_step: None };
    let declared = spec.layers.iter().flat_map(|layer| layer.build_secrets.iter().cloned()).collect::<BTreeSet<_>>();
    let mut secrets = BTreeMap::new();
    for name in declared {
        let file = materials.secret(&name).await.map_err(refuse)?;
        secrets.insert(name, file);
    }
    let delivery = match &spec.delivery {
        ImageDelivery::Registry { repository } => ImageDeliveryTarget::Push {
            repository: repository.clone(),
            cache_repository: registry_build_cache(repository, &spec.architecture),
            auth: materials.push_auth(repository).await.map_err(refuse)?,
        },
        ImageDelivery::Local => ImageDeliveryTarget::Load,
    };
    let request = ImageBuildRequest::builder()
        .identity(&spec.identity)
        .architecture(&spec.architecture)
        .layers(&spec.layers)
        .delivery(delivery)
        .secrets(secrets)
        .probe_wrapper(probe_wrapper)
        .probe_cpus(spec.reservation.cpu)
        .build();
    let result = service.build(request, log).await;
    if let Err(failed) = &result {
        log.push_str(&format!("\n{}\n", failed.failure.reason));
    }
    result
}

/// Name the owner of a recipe failure in its reason (ADR 0062 decision A12):
/// the first layer whose probe failed, else the layer whose build step
/// failed, else the composition as a whole.
pub fn attribute_recipe_failure(spec: &ImageBuildSpec, failed: &FailedImageBuild) -> ImageBuildFailure {
    let mut failure = failed.failure.clone();
    if failure.class != ImageBuildFailureClass::Recipe {
        return failure;
    }
    let layer = failed
        .probes
        .iter()
        .find(|probe| matches!(probe.outcome, ImageProbeOutcome::Failed { .. }))
        .map(|probe| probe.layer.as_str())
        .or(failed.failed_step.as_deref())
        .and_then(|name| spec.layers.iter().find(|layer| layer.name == name));
    let owner = match layer {
        Some(layer) => {
            let source = match (&layer.source, &layer.project) {
                (ImageLayerSource::Project, Some(project)) => format!("project `{project}`"),
                (ImageLayerSource::Project, None) => "a project".to_string(),
                (ImageLayerSource::Installation, _) => "the installation".to_string(),
                (ImageLayerSource::Stdlib, _) => "the flotilla stdlib".to_string(),
            };
            format!("layer `{}` {} from {source}", layer.name, layer.version)
        }
        None => format!("the composition {}", spec.identity),
    };
    failure.reason = format!("{} (owner: {owner})", failure.reason);
    failure
}

/// The engine decides eligibility/tally/backoff; timestamps describe an attempt already started.
struct BuildProgress {
    phase: ImageBuildPhase,
    attempt: u32,
    not_before: Option<DateTime<Utc>>,
}

fn progress(status: &ImageBuildStatus, reading: Option<&LifecycleReading>) -> BuildProgress {
    let Some(reading) = reading else {
        return BuildProgress { phase: status.phase, attempt: status.attempt, not_before: status.not_before };
    };
    let (failures, not_before) = match reading {
        LifecycleReading::InProgress => (0, None),
        LifecycleReading::Retrying { failures, next, .. } => (*failures, Some(*next)),
        LifecycleReading::Ready { .. } => {
            return BuildProgress { phase: ImageBuildPhase::Built, attempt: status.attempt, not_before: None }
        }
        _ => return BuildProgress { phase: ImageBuildPhase::Failed, attempt: status.attempt, not_before: None },
    };
    let started = status.attempt_started_since(not_before);
    BuildProgress {
        phase: if started { ImageBuildPhase::Building } else { ImageBuildPhase::Queued },
        attempt: failures.saturating_add(u32::from(started)),
        not_before,
    }
}

pub struct ImageBuildPrepared {
    pub action: ImageBuildAction,
    reading: Result<Option<LifecycleReading>, String>,
    // Keep collection and publication in one execution slot across the final checked read.
    _execution: tokio::sync::OwnedMutexGuard<()>,
}

pub enum ImageBuildAction {
    None,
    /// Retired record deleted, or nothing for this host to do.
    Idle,
    /// Waiting for a slot, a retry time or a running job.
    Waiting(Option<Duration>),
    Finished(Box<FinishedAttempt>),
}

pub struct FinishedAttempt {
    pub outcome: Result<BuiltImage, FailedImageBuild>,
    pub log_ref: Result<String, String>,
}

impl ImageBuildReconciler {
    async fn prepare_build(&self, obj: &ResourceObject<ImageBuild>) -> Result<ImageBuildAction, ResourceError> {
        if obj.metadata.deletion_timestamp.is_some() {
            return Ok(ImageBuildAction::None);
        }
        let Some(spec) = obj.spec.current() else {
            // ADR 0053 per-stage records: nothing reads them; remove ours.
            match self.builds.delete(&obj.metadata.name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => {
                    tracing::info!(image_build = %obj.metadata.name, "deleted retired image build record");
                    return Ok(ImageBuildAction::Idle);
                }
                Err(error) => return Err(error),
            }
        };
        if spec.host_ref != self.local_host_ref {
            return Ok(ImageBuildAction::None);
        }
        let Some(service) = &self.service else { return Ok(ImageBuildAction::None) };
        let current = match self.builds.get(&obj.metadata.name).await {
            Ok(current) => current,
            Err(ResourceError::NotFound { .. }) => return Ok(ImageBuildAction::None),
            Err(error) => return Err(error),
        };
        let Some(spec) = current.spec.current().cloned() else { return Ok(ImageBuildAction::None) };
        let Some(slots) = self.slots(&spec.architecture).await? else {
            return Ok(ImageBuildAction::None);
        };
        let status = current.status.clone().unwrap_or_default();
        let reading = self.reading(&current).await.map_err(ResourceError::other)?;
        let progress = progress(&status, reading.as_ref());
        match progress.phase {
            ImageBuildPhase::Built | ImageBuildPhase::Failed => Ok(ImageBuildAction::None),
            ImageBuildPhase::Queued => {
                let now = self.clock.now();
                if let Some(not_before) = progress.not_before.filter(|at| *at > now) {
                    return Ok(ImageBuildAction::Waiting((not_before - now).to_std().ok()));
                }
                if self.building_here(&current.metadata.name).await? >= slots as usize {
                    return Ok(ImageBuildAction::Waiting(Some(BUSY_REQUEUE)));
                }
                // Publish the reservation before running any build process.
                let mut next = status.clone();
                ImageBuildStatusPatch::Start { at: now, attempt: progress.attempt.saturating_add(1) }.apply(&mut next);
                self.builds.update_status(&current.metadata.name, &current.metadata.resource_version, &next).await?;
                Ok(ImageBuildAction::Waiting(Some(Duration::ZERO)))
            }
            ImageBuildPhase::Building => {
                let mut jobs = self.jobs.lock().await;
                let stale = jobs.get(&current.metadata.name).is_some_and(|job| job.attempt != progress.attempt);
                if stale {
                    if let Some(job) = jobs.remove(&current.metadata.name) {
                        job.handle.abort();
                    }
                }
                if !jobs.contains_key(&current.metadata.name) {
                    // Also resumes an attempt this process did not start
                    // (a restart while Building): its slot is already held.
                    let job = self.spawn_job(Arc::clone(service), &current, spec.clone(), progress.attempt);
                    jobs.insert(current.metadata.name.clone(), job);
                    drop(jobs);
                    tokio::task::yield_now().await;
                    jobs = self.jobs.lock().await;
                }
                if !jobs.get(&current.metadata.name).is_some_and(|job| job.handle.is_finished()) {
                    return Ok(ImageBuildAction::Waiting(Some(BUSY_REQUEUE)));
                }
                let job = jobs.remove(&current.metadata.name).expect("finished job");
                match job.handle.await {
                    Ok(Some(JobResult { outcome, log_ref })) => {
                        Ok(ImageBuildAction::Finished(Box::new(FinishedAttempt { outcome, log_ref })))
                    }
                    Ok(None) => Ok(ImageBuildAction::None),
                    // This reconciler aborts only jobs it has already removed
                    // (a stale attempt, or its own drop), so a join error here
                    // is a panic or a foreign abort. It ends the attempt as an
                    // infrastructure failure: backoff and the attempt ceiling
                    // apply, instead of re-spawning the same attempt forever.
                    Err(error) => Ok(ImageBuildAction::Finished(Box::new(FinishedAttempt {
                        outcome: Err(FailedImageBuild {
                            failure: ImageBuildFailure {
                                class: ImageBuildFailureClass::Infrastructure,
                                reason: format!("the image build job ended abnormally: {error}"),
                            },
                            probes: not_run_probes(&spec),
                            failed_step: None,
                        }),
                        log_ref: Err("the image build job ended before its log was kept".into()),
                    }))),
                }
            }
        }
    }
}

impl Reconciler for ImageBuildReconciler {
    type Resource = ImageBuild;
    type Prepared = ImageBuildPrepared;

    async fn prepare(&self, obj: &ResourceObject<ImageBuild>) -> Result<Self::Prepared, ResourceError> {
        let execution = Arc::clone(&self.execution).lock_owned().await;
        let action = self.prepare_build(obj).await?;
        let reading = self.reading(obj).await;
        Ok(ImageBuildPrepared { action, reading, _execution: execution })
    }

    fn reconcile(&self, obj: &ResourceObject<ImageBuild>, prepared: &Self::Prepared, now: DateTime<Utc>) -> ReconcileOutcome<ImageBuild> {
        let Some(spec) = obj.spec.current() else { return ReconcileOutcome::new(None) };
        let status = obj.status.clone().unwrap_or_default();
        let reading = match &prepared.reading {
            Ok(reading) => reading.as_ref(),
            Err(error) => {
                let mut outcome = ReconcileOutcome::new(None);
                outcome.events.push(flotilla_resources::ObjectEvent::for_object(obj, "LifecycleReadFailed", error.clone()));
                outcome.requeue_after = Some(BUSY_REQUEUE);
                return outcome;
            }
        };
        if reading.is_some_and(|reading| !matches!(reading, LifecycleReading::InProgress | LifecycleReading::Retrying { .. })) {
            // A late finished job cannot overwrite the engine's ending. Preparation reads
            // this frame after collecting the job; the engine also checks the eventual step.
            return ReconcileOutcome::new(None);
        }
        let progress = progress(&status, reading);
        let action = &prepared.action;
        let mut outcome = match action {
            ImageBuildAction::Finished(finished) => {
                let patch = match (&finished.outcome, &finished.log_ref) {
                    (Ok(built), Ok(log_ref)) => ImageBuildStatusPatch::Built {
                        at: now,
                        digest: built.digest.clone(),
                        probes: built.probes.clone(),
                        log_ref: log_ref.clone(),
                    },
                    (Ok(built), Err(error)) => ImageBuildStatusPatch::Failed {
                        at: now,
                        // ADR 0062 keeps every build log; a build whose log
                        // was lost is retried (BuildKit's cache makes it cheap).
                        failure: ImageBuildFailure {
                            class: ImageBuildFailureClass::Infrastructure,
                            reason: format!("the build log could not be kept: {error}"),
                        },
                        probes: built.probes.clone(),
                        log_ref: String::new(),
                    },
                    (Err(failed), log_ref) => {
                        let failure = attribute_recipe_failure(spec, failed);
                        tracing::error!(
                            role = "infra",
                            image_build = %obj.metadata.name,
                            class = ?failure.class,
                            attempt = status.attempt,
                            reason = %failure.reason,
                            "image build failed"
                        );
                        ImageBuildStatusPatch::Failed {
                            at: now,
                            failure,
                            probes: failed.probes.clone(),
                            log_ref: log_ref.clone().unwrap_or_default(),
                        }
                    }
                };
                let mut outcome = ReconcileOutcome::new(Some(patch));
                outcome.requeue_after = Some(Duration::ZERO);
                outcome
            }
            ImageBuildAction::Waiting(delay) => {
                let mut outcome = ReconcileOutcome::new(None);
                outcome.requeue_after = *delay;
                outcome
            }
            ImageBuildAction::Idle => ReconcileOutcome::new(None),
            ImageBuildAction::None => {
                // A bounded infrastructure failure goes back to Queued after
                // its backoff; recipe failures never retry identical inputs.
                let retry = spec.host_ref == self.local_host_ref && self.service.is_some() && reading.is_none() && status.will_retry();
                if retry {
                    let not_before = status.finished_at.unwrap_or(now) + image_build_retry_backoff(status.attempt);
                    let mut outcome = ReconcileOutcome::new(Some(ImageBuildStatusPatch::Retry { not_before }));
                    outcome.requeue_after = (not_before - now).to_std().ok().or(Some(Duration::ZERO));
                    outcome
                } else {
                    ReconcileOutcome::new(None)
                }
            }
        };
        if outcome.requeue_after.is_none()
            && spec.host_ref == self.local_host_ref
            && self.service.is_some()
            && matches!(progress.phase, ImageBuildPhase::Queued | ImageBuildPhase::Building)
            && !matches!(action, ImageBuildAction::None)
        {
            outcome.requeue_after = Some(BUSY_REQUEUE);
        }
        outcome
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<ImageBuild>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

impl Drop for ImageBuildReconciler {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().values() {
            job.handle.abort();
        }
    }
}
