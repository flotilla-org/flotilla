use async_trait::async_trait;
use flotilla_core::discovery_api::EnvironmentBag;
use flotilla_core::in_process::discover_repo_for_environment;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_core::providers::discovery::DiscoveryResult;
use flotilla_core::providers::environment::EnvironmentHandle;
use flotilla_core::providers::CommandRunner;
use flotilla_protocol::EnvironmentId;
use flotilla_resources::Repository;
use std::{path::Path, sync::Arc};

#[async_trait]
pub trait InProcessDiscoveryExt {
    fn register_direct_environment_for_test(
        &self,
        env_id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_id: Option<flotilla_protocol::qualified_path::HostId>,
    ) -> Result<(), String>;
    fn set_direct_environment_ssh_destination_for_test(&self, env_id: &EnvironmentId, destination: String) -> Result<(), String>;
    fn register_provisioned_environment_for_test(
        &self,
        env_id: EnvironmentId,
        handle: EnvironmentHandle,
        env_bag: EnvironmentBag,
    ) -> Result<(), String>;
    fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String>;
    fn managed_environment_ids_for_test(&self) -> Vec<EnvironmentId>;
    fn environment_bag_for_test(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag>;
    async fn discover_repo_for_environment_for_test(
        &self,
        repo_path: &Path,
        environment_id: &EnvironmentId,
    ) -> Result<DiscoveryResult, String>;
}
#[async_trait]
impl InProcessDiscoveryExt for InProcessDaemon {
    fn register_direct_environment_for_test(
        &self,
        env_id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_id: Option<flotilla_protocol::qualified_path::HostId>,
    ) -> Result<(), String> {
        self.environment_manager().register_direct_environment(env_id, runner, env_bag, host_id)
    }

    fn set_direct_environment_ssh_destination_for_test(&self, env_id: &EnvironmentId, destination: String) -> Result<(), String> {
        self.environment_manager().set_direct_environment_ssh_destination(env_id, destination)
    }

    fn register_provisioned_environment_for_test(
        &self,
        env_id: EnvironmentId,
        handle: EnvironmentHandle,
        env_bag: EnvironmentBag,
    ) -> Result<(), String> {
        self.environment_manager().register_provisioned_environment(env_id, handle, env_bag, None)
    }

    fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String> {
        self.environment_manager().replace_local_environment_bag(env_bag)
    }

    fn managed_environment_ids_for_test(&self) -> Vec<EnvironmentId> {
        self.environment_manager().managed_environments().into_iter().map(|(env_id, _)| env_id).collect()
    }

    fn environment_bag_for_test(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager().environment_bag(env_id)
    }

    async fn discover_repo_for_environment_for_test(
        &self,
        repo_path: &Path,
        environment_id: &EnvironmentId,
    ) -> Result<DiscoveryResult, String> {
        let repository = match self.repository_key_for_path(repo_path).await {
            Some(key) => self
                .resource_backend()
                .including_replicas::<Repository>(&self.provisioning_namespace().await)
                .get(&key.to_string())
                .await
                .ok()
                .map(|source| source.object),
            None => None,
        };
        discover_repo_for_environment(
            self.environment_manager(),
            self.discovery_runtime(),
            &self.config_store(),
            &self.resource_backend(),
            &self.provisioning_namespace().await,
            self.local_environment_id(),
            environment_id,
            repo_path,
            repository.as_ref().map(|repository| &repository.spec),
        )
        .await
    }
}
