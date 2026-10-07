use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits};
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    DockerEnvironmentSpec, DockerImagePullPolicy, Environment, EnvironmentPhase, EnvironmentStatusPatch, Host, ImageBuildPhase,
    ResourceBackend, ResourceError, ResourceObject, TypedResolver,
};

#[async_trait]
pub trait DockerEnvironmentRuntime: Send + Sync {
    async fn ensure_image(
        &self,
        _build: &ResourceObject<flotilla_resources::ImageBuild>,
        _host: &str,
    ) -> Result<Option<flotilla_resources::PlacedImageIdentity>, String> {
        Ok(None)
    }
    async fn provision(&self, name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String>;
    async fn destroy(&self, environment_ref: &str, container_id: &str) -> Result<(), String>;
    /// Recover a Docker backing whose identity was never committed to status.
    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        self.cleanup(environment_ref).await
    }
    async fn cleanup(&self, _environment_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerProvisioning {
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub container_id: String,
    pub image_ref: String,
    pub local_image_id: String,
    pub registry_digest: Option<String>,
}

pub struct EnvironmentReconciler<R> {
    docker: Arc<R>,
    hosts: TypedResolver<Host>,
    backend: ResourceBackend,
    namespace: String,
    image_inputs: Option<Arc<dyn flotilla_core::image_build::ImageBuildInputResolver>>,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
}

impl<R> EnvironmentReconciler<R> {
    pub fn new(docker: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            docker,
            hosts: backend.using::<Host>(namespace),
            backend,
            namespace: namespace.into(),
            image_inputs: None,
            local_host_ref: None,
            additional_host_refs: Default::default(),
        }
    }

    pub fn with_image_build_inputs(mut self, inputs: Option<Arc<dyn flotilla_core::image_build::ImageBuildInputResolver>>) -> Self {
        self.image_inputs = inputs;
        self
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
    Waiting(String, Vec<String>),
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
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => {
                if let Some(original) = &obj.spec.docker {
                    let mut spec = original.clone();
                    let mut resolved_identity = None;
                    let mut progress = obj.status.as_ref().map(|status| status.image_build_refs.clone()).unwrap_or_default();
                    if let Some(composition) = &original.image_composition {
                        let Some(inputs) = &self.image_inputs else {
                            return Ok(EnvironmentPrepared::Waiting("ImageBuild input resolver unavailable".into(), progress));
                        };
                        let mut composition = composition.as_ref().clone();
                        if !progress.is_empty() {
                            composition.build_refs = progress.clone();
                        }
                        match flotilla_core::image_build::ImageBuildAdmission::new(
                            self.backend.clone(),
                            &self.namespace,
                            Arc::clone(inputs),
                        )
                        .join(&spec.host_ref, &composition)
                        .await
                        {
                            Ok(refs) => progress = refs,
                            Err(reason) => {
                                return Ok(EnvironmentPrepared::Waiting(format!("ImageBuild resolution waiting: {reason}"), progress))
                            }
                        }
                        spec.image_build_ref = progress.last().cloned();
                    }
                    let waiting = |message: String| Ok(EnvironmentPrepared::Waiting(message, progress.clone()));

                    if let Some(name) = &spec.image_build_ref {
                        let mut build = match flotilla_resources::read_image_build(&self.backend, &self.namespace, name).await {
                            Ok(build) => build,
                            Err(ResourceError::NotFound { .. }) => {
                                return waiting(format!("ImageBuild {name} awaiting replicated build status"))
                            }
                            Err(error) => return Err(error),
                        };
                        for _ in 0..3 {
                            match flotilla_resources::read_image_build(
                                &self.backend,
                                &self.namespace,
                                &format!("{}-retry", build.metadata.name),
                            )
                            .await
                            {
                                Ok(next) => build = next,
                                Err(ResourceError::NotFound { .. }) => break,
                                Err(error) => return Err(error),
                            }
                        }
                        let phase = build.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase);
                        let mut dependency = build.clone();
                        let mut dependency_reason = String::new();
                        for _ in 0..64 {
                            if let Some(failure) = dependency.status.as_ref().and_then(|status| status.failure.as_ref()) {
                                dependency_reason = format!("; ImageBuild {} failed: {}", dependency.metadata.name, failure.reason);
                                break;
                            }
                            let parent = dependency.spec.previous_build_ref.as_ref().or(dependency.spec.parent_build_ref.as_ref());
                            let Some(parent) = parent else {
                                break;
                            };
                            match flotilla_resources::read_image_build(&self.backend, &self.namespace, parent).await {
                                Ok(parent) if parent.status.as_ref().is_some_and(|status| status.phase == ImageBuildPhase::Built) => break,
                                Ok(parent) => dependency = parent,
                                Err(ResourceError::NotFound { .. }) => {
                                    dependency_reason = format!("; awaiting ImageBuild {parent}");
                                    break;
                                }
                                Err(error) => return Err(error),
                            }
                        }

                        let Some(status) = build.status.as_ref().filter(|status| status.phase == ImageBuildPhase::Built) else {
                            let reason =
                                build.status.as_ref().and_then(|status| status.failure.as_ref()).map(|failure| failure.reason.as_str());
                            return waiting(format!(
                                "ImageBuild {} {phase:?}{}{dependency_reason}",
                                build.metadata.name,
                                reason.map(|reason| format!(": {reason}")).unwrap_or_default()
                            ));
                        };
                        if original.image_composition.as_ref().is_some_and(|composition| progress.len() < composition.layers.len()) {
                            return waiting(format!("ImageBuild {} built; awaiting next composition stage", build.metadata.name));
                        }
                        let identity = status.identity.as_ref().ok_or_else(|| ResourceError::invalid("built image has no identity"))?;
                        let delivered = match self.docker.ensure_image(&build, &spec.host_ref).await {
                            Ok(Some(delivered)) => delivered,
                            Ok(None) if build.spec.host_ref == spec.host_ref => identity.clone(),
                            Ok(None) => {
                                return waiting(format!("ImageBuild {} awaiting digest transfer to {}", build.metadata.name, spec.host_ref))
                            }
                            Err(reason) => return waiting(format!("ImageBuild {} distribution waiting: {reason}", build.metadata.name)),
                        };
                        spec.image = delivered.local_image_id.clone();
                        resolved_identity = Some(delivered);
                        spec.pull_policy = DockerImagePullPolicy::Never;
                    }
                    match self.docker.provision(&obj.metadata.name, &spec).await {
                        Ok(mut provisioning) => {
                            if let Some(identity) = resolved_identity {
                                if provisioning.local_image_id != identity.local_image_id {
                                    return Ok(EnvironmentPrepared::Failed("provisioned image differs from delivered digest".into()));
                                }
                                provisioning.registry_digest = identity.registry_digest;
                            }
                            Ok(EnvironmentPrepared::Ready(provisioning))
                        }
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
            EnvironmentPhase::Pending if obj.spec.host_direct.is_some() => Some(EnvironmentStatusPatch::MarkReady {
                configured_limits: None,
                docker_container_id: None,
                image_ref: None,
                local_image_id: None,
                registry_digest: None,
            }),
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => match prepared {
                EnvironmentPrepared::Ready(provisioning) => Some(EnvironmentStatusPatch::MarkReady {
                    configured_limits: provisioning.configured_limits.clone(),
                    docker_container_id: Some(provisioning.container_id.clone()),
                    image_ref: Some(provisioning.image_ref.clone()),
                    local_image_id: Some(provisioning.local_image_id.clone()),
                    registry_digest: provisioning.registry_digest.clone(),
                }),
                EnvironmentPrepared::Failed(message) => Some(EnvironmentStatusPatch::MarkFailed { message: message.clone() }),
                EnvironmentPrepared::Waiting(message, build_refs) => {
                    Some(EnvironmentStatusPatch::WaitForImageBuild { message: message.clone(), build_refs: build_refs.clone() })
                }
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
        } else if obj.spec.docker.is_some() {
            self.docker.destroy_unrecorded(&obj.metadata.name).await.map_err(ResourceError::other)?;
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
