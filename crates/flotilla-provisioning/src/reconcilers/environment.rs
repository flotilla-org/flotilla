use std::sync::Arc;

use async_trait::async_trait;
use flotilla_environment::environment::EnvironmentProvisionError;
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits, LifecycleReading};
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::{
    Checkout, DockerImagePullPolicy, Environment, EnvironmentPhase, EnvironmentStatusPatch, Host, ImageBuildPhase, ImageDelivery,
    ObjectEvent, ResourceError, ResourceObject,
};
use flotilla_store::{
    controller::{ReconcileOutcome, Reconciler},
    ResourceBackend, StoreQuery, TypedResolver,
};

#[async_trait]
pub trait EnvironmentRuntime: Send + Sync {
    /// Adopt or provision through the provider addressed by the full spec.
    async fn provision_environment(
        &self,
        name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<EnvironmentProvisioning, EnvironmentProvisionError>;
    async fn destroy(&self, environment_ref: &str, backing_id: &str) -> Result<(), String>;
    /// Recover a provider backing whose identity was never committed to status.
    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        self.cleanup(environment_ref).await
    }
    async fn cleanup(&self, _environment_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

pub struct EnvironmentReconciler<R> {
    runtime: Arc<R>,
    hosts: TypedResolver<Host>,
    backend: ResourceBackend,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    namespace: String,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
    adoption_retries: std::sync::Mutex<std::collections::HashMap<String, u32>>,
}

impl<R> EnvironmentReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            runtime,
            hosts: backend.using::<Host>(namespace),
            backend,
            lifecycle_reads: Arc::new(NoLifecycleReads),
            namespace: namespace.into(),
            local_host_ref: None,
            additional_host_refs: Default::default(),
            adoption_retries: Default::default(),
        }
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }

    fn reading(&self, obj: &ResourceObject<Environment>) -> Result<Option<LifecycleReading>, String> {
        self.lifecycle_reads.try_reading(ProvisioningKind::Environment, &obj.metadata.name, None)
    }

    fn provisioning(obj: &ResourceObject<Environment>, reading: Option<&LifecycleReading>) -> bool {
        match reading {
            Some(reading) => matches!(reading, LifecycleReading::InProgress | LifecycleReading::Retrying { .. }),
            None => matches!(
                obj.status.as_ref().map(|status| status.phase).unwrap_or_default(),
                EnvironmentPhase::Pending | EnvironmentPhase::Provisioning
            ),
        }
    }

    pub fn with_local_host_ref(mut self, local_host_ref: CanonicalHostId) -> Self {
        self.local_host_ref = Some(local_host_ref);
        self
    }

    pub fn with_additional_host_refs(mut self, host_refs: impl IntoIterator<Item = CanonicalHostId>) -> Self {
        self.additional_host_refs = host_refs.into_iter().collect();
        self
    }

    async fn actuates(&self, environment: &ResourceObject<Environment>) -> Result<bool, ResourceError> {
        let Some(local_host_ref) = self.local_host_ref.as_ref() else {
            return Ok(true);
        };
        // Unscoped environments predate placement projection; their local
        // authoritative store remains their actuator.
        let Some(host_ref) = environment.spec.host_ref() else {
            return Ok(true);
        };
        let hosts = self.hosts.list().await?;
        let canonical = match flotilla_resources::canonical_host_id(&hosts.items, host_ref) {
            Ok(Some(canonical)) => canonical,
            Ok(None) | Err(_) => return Ok(false),
        };
        Ok(&canonical == local_host_ref || self.additional_host_refs.contains(&canonical) && environment.spec.host_direct.is_some())
    }
}

/// Provider facts mapped to the preceding-generation stored status shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentProvisioning {
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub provider_descriptor: Option<Box<flotilla_resources::EnvironmentProviderDescriptor>>,
    pub survivors: Option<flotilla_resources::EnvironmentSurvivors>,
    pub backing_id: Option<String>,
    pub image_ref: Option<String>,
    pub local_image_id: Option<String>,
    pub registry_digest: Option<String>,
}

pub enum EnvironmentPrepared {
    Foreign,
    None,
    Ready(Box<EnvironmentProvisioning>),
    Failed(String),
    Waiting(String),
    RetryAdoption(String),
}

impl<R> Reconciler for EnvironmentReconciler<R>
where
    R: EnvironmentRuntime + 'static,
{
    type Resource = Environment;
    type Prepared = EnvironmentPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        if !self.actuates(obj).await? {
            return Ok(EnvironmentPrepared::Foreign);
        }
        let reading = self.reading(obj).map_err(ResourceError::other)?;
        if Self::provisioning(obj, reading.as_ref()) {
            if let Some(original) = &obj.spec.docker {
                let mut spec = original.clone();
                if let Some(name) = &original.image_build_ref {
                    // The composed image is the per-identity build's digest.
                    // Never fall back to another image: wait with the reason.
                    let reading =
                        match flotilla_store::read_image_build(&self.backend, &self.namespace, name, Arc::clone(&self.lifecycle_reads))
                            .await
                        {
                            Ok(build) => build,
                            Err(ResourceError::NotFound { .. }) => {
                                // A previous-generation per-stage build can never
                                // be satisfied: fail so recreation relaunches it.
                                let retired = match self
                                    .backend
                                    .including_replicas::<flotilla_resources::ImageBuild>(&self.namespace)
                                    .get(name)
                                    .await
                                {
                                    Ok(record) => record.object.spec.is_retired(),
                                    Err(ResourceError::NotFound { .. }) => false,
                                    Err(error) => return Err(error),
                                };
                                if retired {
                                    return Ok(EnvironmentPrepared::Failed(format!(
                                        "ImageBuild {name} is a previous-generation image build; relaunch the vessel"
                                    )));
                                }
                                return Ok(EnvironmentPrepared::Waiting(format!("ImageBuild {name} awaiting replicated build status")));
                            }
                            Err(error) => return Err(error),
                        };
                    let build = reading.build.object;
                    let status = build.status.clone().unwrap_or_default();
                    let digest = match reading.lifecycle {
                        Some(LifecycleReading::Ready { identity }) => identity,
                        Some(reading) => {
                            let reason = reading
                                .reason()
                                .filter(|reason| !reason.is_empty())
                                .map(|reason| format!(": {reason}"))
                                .unwrap_or_default();
                            return Ok(EnvironmentPrepared::Waiting(format!("ImageBuild {name} {}{reason}", reading.label())));
                        }
                        None => match (status.phase, &status.digest) {
                            (ImageBuildPhase::Built, Some(digest)) => digest.clone(),
                            (phase, _) => {
                                let reason = status.failure.as_ref().map(|failure| format!(": {}", failure.reason)).unwrap_or_default();
                                return Ok(EnvironmentPrepared::Waiting(format!("ImageBuild {name} {phase:?}{reason}")));
                            }
                        },
                    };
                    spec.image = digest;
                    // A local build exists only on its builder. A registry
                    // digest is pinned, so pulling it if absent is always
                    // safe; the provider pulls with its pull credential.
                    spec.pull_policy = match build.spec.current().map(|spec| &spec.delivery) {
                        Some(ImageDelivery::Local) => DockerImagePullPolicy::Never,
                        _ => DockerImagePullPolicy::IfNotPresent,
                    };
                }
                match self
                    .runtime
                    .provision_environment(
                        &obj.metadata.name,
                        &flotilla_resources::EnvironmentSpec { docker: Some(spec), ..obj.spec.clone() },
                    )
                    .await
                {
                    Ok(provisioning) => Ok(EnvironmentPrepared::Ready(Box::new(provisioning))),
                    Err(EnvironmentProvisionError::RetryAdoption(message)) => {
                        Ok(EnvironmentPrepared::RetryAdoption(format!("Unknown: adoption failed; backing retained for retry: {message}")))
                    }
                    Err(error) if error.to_string().starts_with("waiting on build:") => Ok(EnvironmentPrepared::Waiting(error.to_string())),
                    Err(error) => Ok(EnvironmentPrepared::Failed(error.to_string())),
                }
            } else {
                match self.runtime.provision_environment(&obj.metadata.name, &obj.spec).await {
                    Ok(provisioning) => Ok(EnvironmentPrepared::Ready(Box::new(provisioning))),
                    Err(EnvironmentProvisionError::RetryAdoption(message)) => {
                        Ok(EnvironmentPrepared::RetryAdoption(format!("Unknown: adoption failed; backing retained for retry: {message}")))
                    }
                    Err(error) => Ok(EnvironmentPrepared::Failed(error.to_string())),
                }
            }
        } else {
            Ok(EnvironmentPrepared::None)
        }
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        if matches!(prepared, EnvironmentPrepared::Foreign) {
            return ReconcileOutcome::new(None);
        }
        // The current judgement gates publication: provider work may have finished after this lifecycle ended.
        let reading = match self.reading(obj) {
            Ok(reading) => reading,
            Err(error) => {
                let mut outcome = ReconcileOutcome::new(None);
                outcome.events.push(ObjectEvent::for_object(obj, "LifecycleReadFailed", error));
                outcome.requeue_after = Some(std::time::Duration::from_secs(30));
                return outcome;
            }
        };
        let patch = if Self::provisioning(obj, reading.as_ref()) {
            match prepared {
                EnvironmentPrepared::Ready(provisioning) if obj.spec.cloud_sandbox.is_some() => {
                    match (&provisioning.backing_id, &provisioning.image_ref, &provisioning.provider_descriptor, &provisioning.survivors) {
                        (Some(backing_id), Some(image_ref), Some(provider_descriptor), Some(survivors)) => {
                            Some(EnvironmentStatusPatch::MarkCloudReady {
                                backing_id: backing_id.clone(),
                                image_ref: image_ref.clone(),
                                provider_descriptor: provider_descriptor.clone(),
                                survivors: survivors.clone(),
                            })
                        }
                        _ => Some(EnvironmentStatusPatch::MarkFailed {
                            message: "cloud provider omitted backing, image, descriptor or survivors".into(),
                        }),
                    }
                }
                EnvironmentPrepared::Ready(provisioning) => Some(EnvironmentStatusPatch::MarkReady {
                    configured_limits: provisioning.configured_limits.clone(),
                    docker_container_id: provisioning.backing_id.clone(),
                    image_ref: provisioning.image_ref.clone(),
                    local_image_id: provisioning.local_image_id.clone(),
                    registry_digest: provisioning.registry_digest.clone(),
                }),
                EnvironmentPrepared::Failed(message) => Some(EnvironmentStatusPatch::MarkFailed { message: message.clone() }),
                // This legacy patch writes only generic Provisioning/ready=false
                // and the supplied message. It stores no image-build wait state;
                // adoption gets its own runtime variant and retry scheduling.
                EnvironmentPrepared::Waiting(message) | EnvironmentPrepared::RetryAdoption(message) => {
                    Some(EnvironmentStatusPatch::WaitForImageBuild { message: message.clone() })
                }
                EnvironmentPrepared::Foreign | EnvironmentPrepared::None => None,
            }
        } else {
            None
        };

        let mut outcome = ReconcileOutcome::new(patch);
        let mut retries = self.adoption_retries.lock().expect("adoption retries");
        if matches!(prepared, EnvironmentPrepared::RetryAdoption(_)) {
            let attempt = retries.entry(obj.metadata.name.clone()).or_default();
            outcome.requeue_after = Some(std::time::Duration::from_secs((5_u64 << *attempt).min(300)));
            *attempt = (*attempt + 1).min(6);
        } else {
            retries.remove(&obj.metadata.name);
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        self.adoption_retries.lock().expect("adoption retries").remove(&obj.metadata.name);
        if !self.actuates(obj).await? {
            return Ok(());
        }
        // A checkout finalizer still needs this environment's command runner.
        // Convoy/vessel deletion requests race across controllers: destroying
        // the backing first leaves the checkout permanently unable to finalize.
        let query = StoreQuery { fields: [("spec.env_ref".into(), obj.metadata.name.clone())].into(), ..StoreQuery::default() };
        if !self.backend.query_declared::<Checkout>(&self.namespace, &query).await?.items.is_empty() {
            return Err(ResourceError::FinalizerPending);
        }
        if let Some(container_id) =
            obj.status.as_ref().and_then(|status| status.backing_id.as_deref().or(status.docker_container_id.as_deref()))
        {
            self.runtime.destroy(&obj.metadata.name, container_id).await.map_err(ResourceError::other)?;
        } else if obj.spec.provider_address.is_some() || obj.spec.docker.is_some() {
            self.runtime.destroy_unrecorded(&obj.metadata.name).await.map_err(ResourceError::other)?;
        } else {
            self.runtime.cleanup(&obj.metadata.name).await.map_err(ResourceError::other)?;
        }
        Ok(())
    }

    fn retry_disposition(&self, obj: &ResourceObject<Self::Resource>) -> Option<flotilla_resources::ControllerRetryDisposition> {
        use flotilla_resources::ControllerRetryDisposition;
        // Exhausted still owes GiveUp in engine prepare; only GaveUp is terminal.
        match self.reading(obj) {
            Ok(Some(LifecycleReading::Retrying { next, .. })) => Some(ControllerRetryDisposition::Retryable { next_attempt_at: next }),
            Ok(Some(LifecycleReading::GaveUp { cause })) => Some(ControllerRetryDisposition::Terminal { needs: cause }),
            // A failed read is reported by prepare, allowing controller error backoff.
            Ok(_) | Err(_) => None,
        }
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/environment-teardown")
    }

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<EnvironmentStatusPatch> {
        let message = format!("environment teardown failed: {error}");
        let already_failed = match self.reading(obj) {
            Ok(Some(LifecycleReading::GaveUp { cause } | LifecycleReading::Exhausted { cause })) => cause == message,
            Ok(Some(_)) | Err(_) => false,
            Ok(None) => obj
                .status
                .as_ref()
                .is_some_and(|status| status.phase == EnvironmentPhase::Failed && status.message.as_deref() == Some(&message)),
        };
        if already_failed {
            return None;
        }
        Some(EnvironmentStatusPatch::MarkFailed { message })
    }
}
