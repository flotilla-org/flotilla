pub mod docker;
pub mod runner;

#[cfg(test)]
mod tests;

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, EnvironmentStatus, ExecutionEnvironmentPath, ImageId};

use super::CommandRunner;

/// Options for creating a new provisioned environment.
///
/// Runtime-only — not serializable.
#[derive(Debug, Clone)]
pub struct CreateOpts {
    pub tokens: Vec<(String, String)>,
    pub reference_repo: Option<DaemonHostPath>,
    pub daemon_socket_path: DaemonHostPath,
    pub working_directory: Option<ExecutionEnvironmentPath>,
}

/// A live handle to a provisioned sandbox environment.
pub type EnvironmentHandle = Arc<dyn ProvisionedEnvironment>;

/// Manages lifecycle of sandbox environments: image building, creation, and listing.
#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    async fn ensure_image(&self, spec: &EnvironmentSpec) -> Result<ImageId, String>;
    async fn create(&self, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String>;
    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String>;
}

/// A handle to a single provisioned sandbox environment instance.
#[async_trait]
pub trait ProvisionedEnvironment: Send + Sync {
    fn id(&self) -> &EnvironmentId;
    fn image(&self) -> &ImageId;
    async fn status(&self) -> Result<EnvironmentStatus, String>;
    async fn env_vars(&self) -> Result<HashMap<String, String>, String>;
    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner>;
    async fn destroy(&self) -> Result<(), String>;
}
