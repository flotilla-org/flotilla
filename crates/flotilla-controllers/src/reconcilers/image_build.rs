use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    Clock, Host, ImageBuild, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass, ImageBuildPhase, ImageBuildSpec,
    ImageBuildStatusPatch, InputMeta, PlacedImageIdentity, ResourceBackend, ResourceError, ResourceObject, SystemClock, TypedResolver,
};
use tokio::{sync::Mutex, task::JoinHandle};

/// An executor must be idempotent for the immutable execution name, retain its
/// log as an artifact, and verify provides before returning success.
#[async_trait]
pub trait ImageBuildRunner: Send + Sync {
    async fn build(&self, name: &str, spec: &ImageBuildSpec, parent_digest: &str) -> Result<ImageBuildResult, String>;
}

#[derive(Debug, Clone)]
pub enum ImageBuildResult {
    Built { identity: PlacedImageIdentity, verified_provides: BTreeSet<String>, log_ref: String },
    Failed { failure: ImageBuildFailure, log_ref: String },
}

pub struct ImageBuildReconciler<R> {
    runner: Arc<R>,
    builds: TypedResolver<ImageBuild>,
    backend: ResourceBackend,
    namespace: String,
    hosts: TypedResolver<Host>,
    local_host_ref: String,
    execution: Mutex<()>,
    clock: Arc<dyn Clock>,
    jobs: Mutex<BTreeMap<String, JoinHandle<Result<ImageBuildResult, String>>>>,
}

impl<R> ImageBuildReconciler<R> {
    pub fn new(runner: Arc<R>, backend: ResourceBackend, namespace: &str, local_host_ref: &str) -> Self {
        Self {
            runner,
            builds: backend.using(namespace),
            hosts: backend.using(namespace),
            backend,
            namespace: namespace.into(),
            local_host_ref: local_host_ref.into(),
            execution: Mutex::new(()),
            clock: Arc::new(SystemClock),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    async fn successor(&self, obj: &ResourceObject<ImageBuild>) -> Result<(), ResourceError> {
        let Some(status) = &obj.status else {
            return Ok(());
        };
        if status.failure.as_ref().is_none_or(|failure| failure.class != ImageBuildFailureClass::Transient) || obj.spec.attempt >= 3 {
            return Ok(());
        }
        let Some(finished) = status.finished_at else {
            return Ok(());
        };
        let mut spec = obj.spec.clone();
        spec.attempt += 1;
        spec.previous_build_ref = Some(obj.metadata.name.clone());
        spec.not_before = Some(finished + chrono::Duration::seconds(30 * (1_i64 << obj.spec.attempt)));
        spec.reason.old_inputs = spec.reason.new_inputs.clone();
        spec.reason.description = format!("transient retry of {}", obj.metadata.name);
        let name = format!("{}-retry", obj.metadata.name);
        match self.builds.create(&InputMeta::builder().name(name.clone()).build(), &spec).await {
            Ok(_) => Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                if self.builds.get(&name).await?.spec != spec {
                    return Err(ResourceError::invalid("image retry execution changed inputs"));
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn latest(&self, name: &str) -> Result<ResourceObject<ImageBuild>, ResourceError> {
        let mut current = flotilla_resources::read_image_build(&self.backend, &self.namespace, name).await?;
        for _ in 0..3 {
            match flotilla_resources::read_image_build(&self.backend, &self.namespace, &format!("{}-retry", current.metadata.name)).await {
                Ok(next) => current = next,
                Err(ResourceError::NotFound { .. }) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(current)
    }
}

pub enum ImageBuildPrepared {
    None,
    Result(ImageBuildResult),
}

impl<R: ImageBuildRunner + 'static> Reconciler for ImageBuildReconciler<R> {
    type Resource = ImageBuild;
    type Prepared = ImageBuildPrepared;

    async fn prepare(&self, obj: &ResourceObject<ImageBuild>) -> Result<Self::Prepared, ResourceError> {
        if obj.spec.host_ref != self.local_host_ref || obj.metadata.deletion_timestamp.is_some() {
            return Ok(ImageBuildPrepared::None);
        }
        let _execution = self.execution.lock().await;
        let host = self.hosts.get(&self.local_host_ref).await?;
        if host
            .status
            .as_ref()
            .and_then(|status| status.description.as_ref())
            .and_then(|description| description.system.arch.as_deref())
            .is_some_and(|actual| flotilla_core::image_build::canonical_image_architecture(actual) != obj.spec.inputs.architecture)
        {
            return Ok(ImageBuildPrepared::None);
        }

        let current = self.builds.get(&obj.metadata.name).await?;
        let phase = current.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase);
        if phase == ImageBuildPhase::Failed {
            self.successor(&current).await?;
            return Ok(ImageBuildPrepared::None);
        }
        if phase == ImageBuildPhase::Built || current.spec.not_before.is_some_and(|at| at > self.clock.now()) {
            return Ok(ImageBuildPrepared::None);
        }
        let parent_digest = if let Some(parent) = &current.spec.parent_build_ref {
            let parent = self.latest(parent).await?;
            let Some(status) = parent.status.as_ref().filter(|status| status.phase == ImageBuildPhase::Built) else {
                return Ok(ImageBuildPrepared::None);
            };
            let identity = status.identity.as_ref().ok_or_else(|| ResourceError::invalid("built parent has no image identity"))?;
            identity.local_image_id.clone()
        } else {
            current.spec.inputs.parent_digest.clone()
        };
        if phase == ImageBuildPhase::Queued {
            let capacity = host.spec.image_build_capacity;
            let slots = match capacity {
                Some(ImageBuildCapacity::None) => return Ok(ImageBuildPrepared::None),
                Some(ImageBuildCapacity::Builder { architecture, slots, .. })
                    if flotilla_core::image_build::canonical_image_architecture(&architecture) == current.spec.inputs.architecture =>
                {
                    slots
                }
                Some(_) => return Ok(ImageBuildPrepared::None),
                None => 1,
            };
            let reserved = self
                .builds
                .list()
                .await?
                .items
                .iter()
                .filter(|build| {
                    build.spec.host_ref == self.local_host_ref
                        && build.status.as_ref().is_some_and(|status| status.phase == ImageBuildPhase::Building)
                })
                .count();
            if reserved >= slots as usize {
                return Ok(ImageBuildPrepared::None);
            }
            let mut status = current.status.clone().unwrap_or_default();
            flotilla_resources::StatusPatch::apply(
                &ImageBuildStatusPatch::Start { at: self.clock.now(), parent_digest: parent_digest.clone() },
                &mut status,
            );
            self.builds.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await?;
            // Publish the reservation before performing any build process.
            return Ok(ImageBuildPrepared::None);
        }
        {
            let mut jobs = self.jobs.lock().await;
            if !jobs.contains_key(&current.metadata.name) {
                let runner = Arc::clone(&self.runner);
                let spec = current.spec.clone();
                let name = current.metadata.name.clone();
                jobs.insert(name.clone(), tokio::spawn(async move { runner.build(&name, &spec, &parent_digest).await }));
            }
        }
        tokio::task::yield_now().await;
        let mut jobs = self.jobs.lock().await;
        if jobs.get(&current.metadata.name).is_some_and(|job| job.is_finished()) {
            let job = jobs.remove(&current.metadata.name).expect("finished job");
            let result = job.await.map_err(|error| ResourceError::other(error.to_string()))?.map_err(ResourceError::other)?;
            Ok(ImageBuildPrepared::Result(result))
        } else {
            Ok(ImageBuildPrepared::None)
        }
    }

    fn reconcile(&self, obj: &ResourceObject<ImageBuild>, prepared: &Self::Prepared, now: DateTime<Utc>) -> ReconcileOutcome<ImageBuild> {
        let patch = match prepared {
            ImageBuildPrepared::None => None,
            ImageBuildPrepared::Result(ImageBuildResult::Built { identity, verified_provides, log_ref }) => {
                let valid = identity.validate().is_ok() && obj.spec.layer.spec.provides.is_subset(verified_provides);
                if valid {
                    Some(ImageBuildStatusPatch::Built {
                        at: now,
                        identity: identity.clone(),
                        verified_provides: verified_provides.clone(),
                        log_ref: log_ref.clone(),
                    })
                } else {
                    tracing::error!(role = "infra", image_build = %obj.metadata.name,
                        "image build did not verify its declared provides or image identity");
                    Some(ImageBuildStatusPatch::Failed {
                        at: now,
                        failure: ImageBuildFailure {
                            class: ImageBuildFailureClass::Deterministic,
                            reason: "build did not verify its declared provides or image identity".into(),
                        },
                        log_ref: log_ref.clone(),
                    })
                }
            }
            ImageBuildPrepared::Result(ImageBuildResult::Failed { failure, log_ref }) => {
                tracing::error!(role = "infra", image_build = %obj.metadata.name, reason = %failure.reason, "image build failed");
                Some(ImageBuildStatusPatch::Failed { at: now, failure: failure.clone(), log_ref: log_ref.clone() })
            }
        };
        let mut outcome = ReconcileOutcome::new(patch);
        if obj.spec.host_ref == self.local_host_ref
            && !matches!(obj.status.as_ref().map(|status| status.phase), Some(ImageBuildPhase::Built))
        {
            outcome.requeue_after = Some(Duration::from_secs(1));
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

impl<R> Drop for ImageBuildReconciler<R> {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().values() {
            job.abort();
        }
    }
}
