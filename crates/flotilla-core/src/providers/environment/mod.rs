pub mod docker;
pub mod docker_image;
pub mod host_direct;
pub mod image;
pub mod registry_auth;
pub mod registry_client;
pub mod runner;
pub use image::{ImageBuilder, LocalImageCache};
pub use registry_auth::RegistryAuth;

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentStatus, ExecutionEnvironmentPath, ImageId};
use serde::{Deserialize, Serialize};

use super::CommandRunner;

/// Options for creating a new provisioned environment.
///
/// Runtime-only — not serializable.
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    pub tokens: Vec<(String, String)>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub provisioned_mounts: Vec<ProvisionedMount>,
    /// Tools that must be made available inside the environment.
    ///
    /// Providers choose how to deliver these assets. Docker currently lowers
    /// them to bind mounts; remote sandbox providers may upload files or expose
    /// sockets through their own transport.
    pub tools: Vec<EnvironmentTool>,
    pub image_pull_policy: ImagePullPolicy,
    pub prepared_auth: PreparedEnvironmentAuth,
    /// CPU quota for this vessel. None leaves the provider's default.
    pub cpu_limit: Option<usize>,
    pub memory_policy: flotilla_resources::EnvironmentMemoryPolicy,
}

/// Inputs shared by environment provisioning, without image concepts.
#[derive(Debug, Clone, Default)]
pub struct ProvisionOpts {
    pub tokens: Vec<(String, String)>,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub provisioned_mounts: Vec<ProvisionedMount>,
    pub tools: Vec<EnvironmentTool>,
    pub cpu_limit: Option<usize>,
    pub memory_policy: flotilla_resources::EnvironmentMemoryPolicy,
}

/// Admitted operation credentials; callers never select an image through these options.
#[derive(Debug, Clone, Default)]
pub struct PrepareOpts {
    /// Transitional admission for baseline-sourced tags until #2731 cuts the
    /// fleet over to ImageBuild. Only the resource controller verifies provenance.
    pub legacy_baseline: bool,
    pub prepared_auth: PreparedEnvironmentAuth,
}

impl From<CreateOpts> for ProvisionOpts {
    fn from(opts: CreateOpts) -> Self {
        Self {
            tokens: opts.tokens,
            working_directory: opts.working_directory,
            provisioned_mounts: opts.provisioned_mounts,
            tools: opts.tools,
            cpu_limit: opts.cpu_limit,
            memory_policy: opts.memory_policy,
        }
    }
}

/// Runtime-only opaque admitted pull credentials.
pub type PreparedEnvironmentAuth = Option<RegistryAuth>;

/// A host-side tool that an environment provider must make invokable inside a
/// provisioned environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentTool {
    pub name: String,
    pub executable: ExecutionEnvironmentPath,
    pub assets: Vec<EnvironmentToolAsset>,
    pub environment: Vec<EnvironmentVariableUpdate>,
}

impl EnvironmentTool {
    pub fn new(name: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self { name: name.into(), executable: ExecutionEnvironmentPath::new(executable), assets: Vec::new(), environment: Vec::new() }
    }

    pub fn with_asset(mut self, asset: EnvironmentToolAsset) -> Self {
        self.assets.push(asset);
        self
    }

    pub fn with_environment(mut self, update: EnvironmentVariableUpdate) -> Self {
        self.environment.push(update);
        self
    }
}

/// An asset needed by a tool inside an environment.
///
/// The source is deliberately described as a host path rather than as a bind
/// mount. Bind mounting is one provider's delivery strategy, not the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentToolAsset {
    pub host_path: DaemonHostPath,
    pub environment_path: ExecutionEnvironmentPath,
    pub kind: EnvironmentToolAssetKind,
    pub access: EnvironmentToolAssetAccess,
    pub purpose: String,
}

impl EnvironmentToolAsset {
    pub fn new(
        host_path: impl Into<PathBuf>,
        environment_path: impl Into<PathBuf>,
        kind: EnvironmentToolAssetKind,
        access: EnvironmentToolAssetAccess,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            host_path: DaemonHostPath::new(host_path),
            environment_path: ExecutionEnvironmentPath::new(environment_path),
            kind,
            access,
            purpose: purpose.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentToolAssetKind {
    File,
    Directory,
    UnixSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentToolAssetAccess {
    ReadOnly,
    SharedWritable,
}

/// Stable directory used for the host daemon socket inside contained
/// environments. Docker bind-mounts the host socket's parent directory here
/// so a daemon restart can replace the socket inode without breaking the
/// container's view of it.
pub const CONTAINED_DAEMON_SOCKET_DIRECTORY: &str = "/run/flotilla-daemon";
pub const CONTAINED_DAEMON_REQUIRED_ENV: &str = "FLOTILLA_CONTAINED_HOST_DAEMON";

pub fn contained_daemon_socket_path(host_socket_path: &Path) -> PathBuf {
    let file_name = host_socket_path.file_name().expect("daemon socket path must name a socket file");
    Path::new(CONTAINED_DAEMON_SOCKET_DIRECTORY).join(file_name)
}

/// A tool's requested mutation to the environment it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentVariableUpdate {
    Set { name: String, value: String, purpose: String },
    PrependPath { name: String, value: String },
}

impl EnvironmentVariableUpdate {
    pub fn set(name: impl Into<String>, value: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self::Set { name: name.into(), value: value.into(), purpose: purpose.into() }
    }

    pub fn prepend_path(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::PrependPath { name: name.into(), value: value.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImagePullPolicy {
    Always,
    #[default]
    IfNotPresent,
    Never,
}

impl ImagePullPolicy {
    fn docker_value(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::IfNotPresent => "missing",
            Self::Never => "never",
        }
    }
}

impl From<flotilla_resources::DockerImagePullPolicy> for ImagePullPolicy {
    fn from(value: flotilla_resources::DockerImagePullPolicy) -> Self {
        match value {
            flotilla_resources::DockerImagePullPolicy::Always => Self::Always,
            flotilla_resources::DockerImagePullPolicy::IfNotPresent => Self::IfNotPresent,
            flotilla_resources::DockerImagePullPolicy::Never => Self::Never,
        }
    }
}

/// Structured metadata for a flotilla-managed bind mount inside a provisioned environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionedMount {
    pub host_path: DaemonHostPath,
    pub environment_path: ExecutionEnvironmentPath,
    #[serde(default)]
    pub mode: ProvisionedMountMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisionedMountMode {
    #[default]
    Ro,
    Rw,
}

impl ProvisionedMount {
    pub fn new(host_path: impl Into<PathBuf>, environment_path: impl Into<PathBuf>, mode: ProvisionedMountMode) -> Self {
        Self { host_path: DaemonHostPath::new(host_path), environment_path: ExecutionEnvironmentPath::new(environment_path), mode }
    }
}

/// A live handle to a provisioned sandbox environment.
pub type EnvironmentHandle = Arc<dyn ProvisionedEnvironment>;

/// Minimal backing identity for recovery. No mutable mount or image metadata is
/// needed to reclaim a container left behind by an earlier daemon generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvironmentBacking {
    pub environment_id: EnvironmentId,
    pub container_id: String,
}

/// One kind of environment supported by a provider instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentKind {
    HostDirect,
    Docker,
}

impl EnvironmentKind {
    pub fn of(spec: &flotilla_resources::EnvironmentSpec) -> Result<Self, String> {
        match (spec.host_direct.is_some(), spec.docker.is_some()) {
            (true, false) => Ok(Self::HostDirect),
            (false, true) => Ok(Self::Docker),
            _ => Err("environment spec must select exactly one kind".into()),
        }
    }
}

/// A provider's private preparation result. Only the creating instance may
/// consume it; callers never need to know the backing runtime's inputs.
pub struct PreparedEnvironment {
    owner: Arc<()>,
    value: Box<dyn std::any::Any + Send + Sync>,
}

impl PreparedEnvironment {
    pub fn new<T: Send + Sync + 'static>(owner: &Arc<()>, value: T) -> Self {
        Self { owner: Arc::clone(owner), value: Box::new(value) }
    }

    pub(crate) fn get<T: 'static>(&self, owner: &Arc<()>) -> Result<&T, String> {
        if !Arc::ptr_eq(&self.owner, owner) {
            return Err("preparation belongs to a different provider instance".into());
        }
        self.value.downcast_ref().ok_or_else(|| "preparation has a different provider kind".into())
    }
}

/// Makes an environment spec real, without exposing runtime-specific inputs.
#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    fn kind(&self) -> EnvironmentKind;
    /// Optional image capability owned by this exact provider instance.
    fn local_image_cache(&self) -> Option<&dyn LocalImageCache> {
        None
    }
    /// Optional builder using the same endpoint as this instance's local cache.
    fn image_builder(&self) -> Option<Arc<dyn ImageBuilder>> {
        None
    }
    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String>;
    async fn provision(&self, id: EnvironmentId, prepared: &PreparedEnvironment, opts: ProvisionOpts) -> Result<EnvironmentHandle, String>;
    async fn inspect(&self, id: &EnvironmentId) -> Result<Option<EnvironmentHandle>, String> {
        Ok(self.list().await?.into_iter().find(|handle| handle.id() == id))
    }
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
    async fn list_backings(&self) -> Result<Vec<EnvironmentBacking>, String> {
        self.list()
            .await?
            .into_iter()
            .map(|handle| {
                let container_id = handle.container_name().ok_or("environment backing has no container identity")?.to_string();
                Ok(EnvironmentBacking { environment_id: handle.id().clone(), container_id })
            })
            .collect()
    }
    /// Destroy an environment using only its stable provider identity.
    ///
    /// Teardown must not depend on parsing mutable actuated-object metadata:
    /// that metadata can outlive the daemon version which wrote it.
    async fn destroy(&self, container_id: &str) -> Result<(), String>;
}

/// A handle to a single provisioned sandbox environment instance.
#[async_trait]
pub trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;
    /// Immutable content digest of the image actually backing this environment.
    fn local_image_id(&self) -> Option<&str> {
        None
    }

    /// Registry manifest identity, when the image was pulled by digest.
    fn registry_digest(&self) -> Option<&str> {
        None
    }

    /// Provider-specific transport identifier (e.g. Docker container name).
    /// Used by hop chain to construct exec/enter commands.
    fn container_name(&self) -> Option<&str>;
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount>;
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    /// Provider observations are optional for non-Docker adapters.
    async fn runtime_observation(&self) -> Result<Option<flotilla_protocol::EnvironmentRuntimeObservation>, String> {
        Ok(None)
    }
    /// The vessel's configured environment, used as the terminal baseline.
    /// This must not include the provisioning host's ambient environment.
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self) -> Arc<dyn CommandRunner>;
    async fn destroy(&self) -> Result<(), String>;
}

/// Decode the preceding checkout configuration at its admission seam. Building
/// Dockerfiles is demand-driven through ImageBuild, never a provider operation.
pub fn legacy_environment_spec(spec: &flotilla_protocol::EnvironmentSpec) -> Result<flotilla_resources::EnvironmentSpec, String> {
    let flotilla_protocol::ImageSource::Registry(image) = &spec.image else {
        return Err("waiting on build: declare an ImageBuild and pin its resulting digest".into());
    };
    Ok(flotilla_resources::EnvironmentSpec {
        host_direct: None,
        docker: Some(flotilla_resources::DockerEnvironmentSpec {
            host_ref: String::new(),
            image: image.clone(),
            image_build_ref: None,
            image_composition: None,
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            memory_policy: Default::default(),
            mounts: Vec::new(),
            env: Default::default(),
        }),
    })
}

/// Frozen provider instance identity carried from placement policy to its
/// environment. Metadata is extensible, so this adds no stored spec shape.
pub const ENVIRONMENT_PROVIDER_INSTANCE_LABEL: &str = "flotilla.work/environment-provider-instance";

#[cfg(test)]
mod registry_client_tests;

#[cfg(test)]
mod image_tests;
