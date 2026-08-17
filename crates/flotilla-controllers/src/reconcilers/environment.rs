use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentStatusPatch, EnvironmentWaitReason, ResourceError, ResourceObject,
};

#[async_trait]
pub trait DockerEnvironmentRuntime: Send + Sync {
    async fn provision(&self, name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, DockerProvisioningError>;
    async fn destroy(&self, environment_ref: &str, container_id: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerProvisioningError {
    Waiting { message: String, reason: EnvironmentWaitReason },
    Failed(String),
}

impl fmt::Display for DockerProvisioningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Waiting { message, .. } | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl From<String> for DockerProvisioningError {
    fn from(message: String) -> Self {
        Self::Failed(message)
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
    local_host_ref: Option<String>,
}

impl<R> EnvironmentReconciler<R> {
    pub fn new(docker: Arc<R>) -> Self {
        Self { docker, local_host_ref: None }
    }

    pub fn with_local_host_ref(mut self, local_host_ref: impl Into<String>) -> Self {
        self.local_host_ref = Some(local_host_ref.into());
        self
    }

    fn actuates(&self, environment: &ResourceObject<Environment>) -> bool {
        let Some(local_host_ref) = self.local_host_ref.as_deref() else {
            return true;
        };
        // Unscoped environments predate placement projection; their local
        // authoritative store remains their actuator.
        environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .is_none_or(|host_ref| host_ref == local_host_ref)
    }
}

pub enum EnvironmentDeps {
    None,
    Ready(DockerProvisioning),
    Waiting { message: String, reason: EnvironmentWaitReason },
    Failed(String),
}

impl<R> Reconciler for EnvironmentReconciler<R>
where
    R: DockerEnvironmentRuntime + 'static,
{
    type Resource = Environment;
    type Dependencies = EnvironmentDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if !self.actuates(obj) {
            return Ok(EnvironmentDeps::None);
        }
        match obj.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending) {
            EnvironmentPhase::Pending => {
                if let Some(spec) = &obj.spec.docker {
                    match self.docker.provision(&obj.metadata.name, spec).await {
                        Ok(provisioning) => Ok(EnvironmentDeps::Ready(provisioning)),
                        Err(DockerProvisioningError::Waiting { message, reason }) => Ok(EnvironmentDeps::Waiting { message, reason }),
                        Err(DockerProvisioningError::Failed(message)) => Ok(EnvironmentDeps::Failed(message)),
                    }
                } else {
                    Ok(EnvironmentDeps::None)
                }
            }
            _ => Ok(EnvironmentDeps::None),
        }
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        if !self.actuates(obj) {
            return ReconcileOutcome::new(None);
        }
        let patch = match obj.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending) {
            EnvironmentPhase::Pending if obj.spec.host_direct.is_some() => {
                Some(EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None })
            }
            EnvironmentPhase::Pending => match deps {
                EnvironmentDeps::Ready(provisioning) => Some(EnvironmentStatusPatch::MarkReady {
                    docker_container_id: Some(provisioning.container_id.clone()),
                    image_ref: Some(provisioning.image_ref.clone()),
                    image_digest: Some(provisioning.image_digest.clone()),
                }),
                EnvironmentDeps::Waiting { message, reason } => {
                    Some(EnvironmentStatusPatch::MarkWaiting { message: message.clone(), reason: reason.clone() })
                }
                EnvironmentDeps::Failed(message) => Some(EnvironmentStatusPatch::MarkFailed { message: message.clone() }),
                EnvironmentDeps::None => None,
            },
            _ => None,
        };

        let mut outcome = ReconcileOutcome::new(patch);
        if matches!(deps, EnvironmentDeps::Waiting { .. }) {
            outcome.requeue_after = Some(Duration::from_secs(5));
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        if !self.actuates(obj) {
            return Ok(());
        }
        if let Some(container_id) = obj.status.as_ref().and_then(|status| status.docker_container_id.as_deref()) {
            self.docker.destroy(&obj.metadata.name, container_id).await.map_err(ResourceError::other)?;
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
