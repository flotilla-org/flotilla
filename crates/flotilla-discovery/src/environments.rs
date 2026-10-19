//! Environment ports owned by discovery (#3152 ruling 7).
//!
//! Capability crates read environment facts through [`Environments`]; only the
//! executor drives provisioned-environment lifetimes through
//! [`EnvironmentLifecycle`]. [`crate::environment_manager::EnvironmentManager`]
//! implements both.
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_environment::environment::PreparedEnvironment;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::{qualified_path::HostId, EnvironmentId};
use flotilla_provider_runtime::CommandRunner;

use crate::registry::ProviderRegistry;

/// Read-only host identity, environment facts, runners and provider registries.
pub trait Environments: Send + Sync {
    fn local_host_id(&self) -> &HostId;
    fn environment_bag(&self, environment: &EnvironmentId) -> Option<EnvironmentBag>;
    fn environment_runner(&self, environment: &EnvironmentId) -> Option<Arc<dyn CommandRunner>>;
    fn environment_registry(&self, environment: &EnvironmentId) -> Option<Arc<ProviderRegistry>>;
    fn host_id_for_environment(&self, environment: &EnvironmentId) -> Option<HostId>;
}

/// Provisioned-environment lifetimes, used only by the executor.
#[async_trait]
pub trait EnvironmentLifecycle: Environments {
    /// Probe a provisioned environment's providers if none are registered yet.
    async fn ensure_provisioned_environment_providers(
        &self,
        environment: &EnvironmentId,
        config_base: &DaemonHostPath,
    ) -> Result<(), String>;
    async fn create_provisioned_environment(&self, request: CreateProvisionedEnvironmentRequest<'_>) -> Result<(), String>;
    async fn destroy_provisioned_environment(&self, environment: &EnvironmentId) -> Result<(), String>;
}

pub struct CreateProvisionedEnvironmentRequest<'a> {
    pub env_id: EnvironmentId,
    pub provider: &'a str,
    pub registry: &'a ProviderRegistry,
    pub prepared: PreparedEnvironment,
    pub tokens: Vec<(String, String)>,
    pub config_base: &'a DaemonHostPath,
    pub daemon_socket_path: &'a DaemonHostPath,
    pub reference_repo: Option<DaemonHostPath>,
}
