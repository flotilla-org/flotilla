//! Host orchestration collaborators required by admission.
use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_protocol::{qualified_path::HostId, EnvironmentId, PrincipalRef};
use flotilla_provider_runtime::CommandRunner;
use std::sync::Arc;

/// Read-only host identity, environment facts and command runners used by admission.
pub trait AdmissionEnvironments: Send + Sync {
    fn local_host_id(&self) -> &HostId;
    fn environment_bag(&self, environment: &EnvironmentId) -> Option<EnvironmentBag>;
    fn environment_runner(&self, environment: &EnvironmentId) -> Option<Arc<dyn CommandRunner>>;
    fn host_id_for_environment(&self, environment: &EnvironmentId) -> Option<HostId>;
}

#[async_trait]
/// Crew-service teardown required by standing-convoy admission.
pub trait ConvoyTeardown: Send + Sync {
    async fn abandon(&self, namespace: &str, name: &str, reason: &str, principal: Option<&PrincipalRef>) -> Result<(), String>;
    async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String>;
}
