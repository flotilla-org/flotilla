use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits};
use flotilla_resources::{
    Checkout, DockerImagePullPolicy, Environment, EnvironmentPhase, EnvironmentStatusPatch, Host, ImageBuildPhase, ImageDelivery,
    ResourceError, ResourceObject,
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
    ) -> Result<EnvironmentProvisioning, String>;
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
    namespace: String,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
}

impl<R> EnvironmentReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            runtime,
            hosts: backend.using::<Host>(namespace),
            backend,
            namespace: namespace.into(),
            local_host_ref: None,
            additional_host_refs: Default::default(),
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
    pub backing_id: Option<String>,
    pub image_ref: Option<String>,
    pub local_image_id: Option<String>,
    pub registry_digest: Option<String>,
}

pub enum EnvironmentPrepared {
    Foreign,
    None,
    Ready(EnvironmentProvisioning),
    Failed(String),
    Waiting(String),
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
        match obj.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending) {
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => {
                if let Some(original) = &obj.spec.docker {
                    let mut spec = original.clone();
                    if let Some(name) = &original.image_build_ref {
                        // The composed image is the per-identity build's digest.
                        // Never fall back to another image: wait with the reason.
                        let build = match flotilla_store::read_image_build(&self.backend, &self.namespace, name).await {
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
                        let status = build.status.clone().unwrap_or_default();
                        let digest = match (status.phase, &status.digest) {
                            (ImageBuildPhase::Built, Some(digest)) => digest.clone(),
                            (phase, _) => {
                                let reason = status.failure.as_ref().map(|failure| format!(": {}", failure.reason)).unwrap_or_default();
                                return Ok(EnvironmentPrepared::Waiting(format!("ImageBuild {name} {phase:?}{reason}")));
                            }
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
                        Ok(provisioning) => Ok(EnvironmentPrepared::Ready(provisioning)),
                        Err(message) if message.starts_with("waiting on build:") => Ok(EnvironmentPrepared::Waiting(message)),
                        Err(message) => Ok(EnvironmentPrepared::Failed(message)),
                    }
                } else {
                    match self.runtime.provision_environment(&obj.metadata.name, &obj.spec).await {
                        Ok(provisioning) => Ok(EnvironmentPrepared::Ready(provisioning)),
                        Err(message) => Ok(EnvironmentPrepared::Failed(message)),
                    }
                }
            }
            _ => Ok(EnvironmentPrepared::None),
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
        let patch = match obj.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending) {
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => match prepared {
                EnvironmentPrepared::Ready(provisioning) => Some(EnvironmentStatusPatch::MarkReady {
                    configured_limits: provisioning.configured_limits.clone(),
                    docker_container_id: provisioning.backing_id.clone(),
                    image_ref: provisioning.image_ref.clone(),
                    local_image_id: provisioning.local_image_id.clone(),
                    registry_digest: provisioning.registry_digest.clone(),
                }),
                EnvironmentPrepared::Failed(message) => Some(EnvironmentStatusPatch::MarkFailed { message: message.clone() }),
                EnvironmentPrepared::Waiting(message) => Some(EnvironmentStatusPatch::WaitForImageBuild { message: message.clone() }),
                EnvironmentPrepared::Foreign | EnvironmentPrepared::None => None,
            },
            _ => None,
        };

        ReconcileOutcome::new(patch)
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
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
        if let Some(container_id) = obj.status.as_ref().and_then(|status| status.docker_container_id.as_deref()) {
            self.runtime.destroy(&obj.metadata.name, container_id).await.map_err(ResourceError::other)?;
        } else if obj.spec.provider_address.is_some() || obj.spec.docker.is_some() {
            self.runtime.destroy_unrecorded(&obj.metadata.name).await.map_err(ResourceError::other)?;
        } else {
            self.runtime.cleanup(&obj.metadata.name).await.map_err(ResourceError::other)?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/environment-teardown")
    }

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<EnvironmentStatusPatch> {
        let message = format!("environment teardown failed: {error}");
        if obj.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Failed && status.message.as_deref() == Some(&message))
        {
            return None;
        }
        Some(EnvironmentStatusPatch::MarkFailed { message })
    }
}
