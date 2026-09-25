use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentStatusPatch, Host, ResourceBackend, ResourceError, ResourceObject,
    TypedResolver,
};

#[async_trait]
pub trait DockerEnvironmentRuntime: Send + Sync {
    async fn provision(&self, name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String>;
    async fn destroy(&self, environment_ref: &str, container_id: &str) -> Result<(), String>;
    async fn cleanup(&self, _environment_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerProvisioning {
    pub container_id: String,
    pub image_ref: String,
    pub image_digest: String,
}

pub struct EnvironmentReconciler<R> {
    docker: Arc<R>,
    hosts: TypedResolver<Host>,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
}

impl<R> EnvironmentReconciler<R> {
    pub fn new(docker: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self { docker, hosts: backend.using::<Host>(namespace), local_host_ref: None, additional_host_refs: Default::default() }
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
        let Some(host_ref) = environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
        else {
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

pub enum EnvironmentPrepared {
    Foreign,
    None,
    Ready(DockerProvisioning),
    Failed(String),
}

impl<R> Reconciler for EnvironmentReconciler<R>
where
    R: DockerEnvironmentRuntime + 'static,
{
    type Resource = Environment;
    type Prepared = EnvironmentPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        if !self.actuates(obj).await? {
            return Ok(EnvironmentPrepared::Foreign);
        }
        match obj.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending) {
            EnvironmentPhase::Pending => {
                if let Some(spec) = &obj.spec.docker {
                    match self.docker.provision(&obj.metadata.name, spec).await {
                        Ok(provisioning) => Ok(EnvironmentPrepared::Ready(provisioning)),
                        Err(message) => Ok(EnvironmentPrepared::Failed(message)),
                    }
                } else {
                    Ok(EnvironmentPrepared::None)
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
            EnvironmentPhase::Pending if obj.spec.host_direct.is_some() => {
                Some(EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None })
            }
            EnvironmentPhase::Pending => match prepared {
                EnvironmentPrepared::Ready(provisioning) => Some(EnvironmentStatusPatch::MarkReady {
                    docker_container_id: Some(provisioning.container_id.clone()),
                    image_ref: Some(provisioning.image_ref.clone()),
                    image_digest: Some(provisioning.image_digest.clone()),
                }),
                EnvironmentPrepared::Failed(message) => Some(EnvironmentStatusPatch::MarkFailed { message: message.clone() }),
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
        if let Some(container_id) = obj.status.as_ref().and_then(|status| status.docker_container_id.as_deref()) {
            self.docker.destroy(&obj.metadata.name, container_id).await.map_err(ResourceError::other)?;
        } else {
            self.docker.cleanup(&obj.metadata.name).await.map_err(ResourceError::other)?;
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
