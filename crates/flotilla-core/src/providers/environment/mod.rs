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
    pub daemon_socket_path: DaemonHostPath,
    pub working_directory: Option<ExecutionEnvironmentPath>,
    pub provisioned_mounts: Vec<ProvisionedMount>,
}

/// Structured metadata for a flotilla-managed bind mount inside a provisioned environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionedMount {
    pub host_path: DaemonHostPath,
    pub environment_path: ExecutionEnvironmentPath,
}

impl ProvisionedMount {
    pub fn new(host_path: impl Into<PathBuf>, environment_path: impl Into<PathBuf>) -> Self {
        Self { host_path: DaemonHostPath::new(host_path), environment_path: ExecutionEnvironmentPath::new(environment_path) }
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
}

/// A handle to a single provisioned sandbox environment instance.
#[async_trait]
pub trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;
    /// Provider-specific transport identifier (e.g. Docker container name).
    /// Used by hop chain to construct exec/enter commands.
    fn container_name(&self) -> Option<&str>;
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        vec![]
    }
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner>;
    async fn destroy(&self) -> Result<(), String>;
}
