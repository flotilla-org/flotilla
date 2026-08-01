pub mod docker;
pub mod runner;

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, EnvironmentStatus, ExecutionEnvironmentPath, ImageId};
use serde::{Deserialize, Serialize};

use super::CommandRunner;

/// Options for creating a new provisioned environment.
///
/// Runtime-only — not serializable.
#[derive(Debug, Clone)]
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
    pub docker_config_dir: Option<DaemonHostPath>,
}

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

/// Manages lifecycle of sandbox environments: image building, creation, and listing.
#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    async fn ensure_image(&self, spec: &EnvironmentSpec, repo_root: &Path) -> Result<ImageId, String>;
    async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String>;
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
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
    fn image_digest(&self) -> Option<&str> {
        None
    }
    /// Provider-specific transport identifier (e.g. Docker container name).
    /// Used by hop chain to construct exec/enter commands.
    fn container_name(&self) -> Option<&str>;
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount>;
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self) -> Arc<dyn CommandRunner>;
    async fn destroy(&self) -> Result<(), String>;
}
