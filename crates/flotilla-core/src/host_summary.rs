use std::{collections::BTreeSet, fs, path::Path};

use flotilla_protocol::{HostEnvironment, HostName, HostProviderStatus, HostSummary, NodeId, NodeInfo, SystemInfo};
use sysinfo::System;

use crate::{
    convert::inventory_from_bag,
    environment_manager::EnvironmentManager,
    model::provider_names_from_registry,
    providers::{discovery::EnvVars, registry::ProviderRegistry},
};

pub async fn build_local_host_summary(
    node_id: &NodeId,
    host_name: &HostName,
    environment_manager: &EnvironmentManager,
    providers: Vec<HostProviderStatus>,
    env: &dyn EnvVars,
) -> HostSummary {
    let host_bag = environment_manager.local_environment_bag();
    let environments = environment_manager.host_summary_environments().await;

    HostSummary {
        node: NodeInfo::new(node_id.clone(), host_name.to_string()),
        system: collect_system_info(env),
        inventory: inventory_from_bag(&host_bag),
        providers,
        environments,
    }
}

pub fn provider_statuses_from_registries<'a>(registries: impl IntoIterator<Item = &'a ProviderRegistry>) -> Vec<HostProviderStatus> {
    let mut seen = BTreeSet::new();
    let mut statuses = Vec::new();

    for registry in registries {
        for (category, entries) in provider_names_from_registry(registry) {
            for entry in entries {
                if seen.insert((category.clone(), entry.implementation.clone())) {
                    statuses.push(HostProviderStatus {
                        category: category.clone(),
                        name: entry.display_name,
                        implementation: entry.implementation,
                        healthy: true,
                    });
                }
            }
        }
    }

    statuses.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    statuses
}

pub fn collect_system_info(env: &dyn EnvVars) -> SystemInfo {
    SystemInfo {
        home_dir: env.get("HOME").map(Into::into),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        cpu_count: std::thread::available_parallelism().ok().and_then(|n| u16::try_from(n.get()).ok()),
        memory_total_mb: total_memory_mb(),
        environment: detect_host_environment(),
    }
}

pub fn classify_host_environment_from_markers(dockerenv_present: bool, cgroup: Option<&str>) -> HostEnvironment {
    if dockerenv_present {
        return HostEnvironment::Container;
    }

    let Some(cgroup) = cgroup else {
        return HostEnvironment::Unknown;
    };

    if ["docker", "containerd", "kubepods", "podman", "libpod"].iter().any(|marker| cgroup.contains(marker)) {
        HostEnvironment::Container
    } else {
        HostEnvironment::Unknown
    }
}

fn detect_host_environment() -> HostEnvironment {
    let dockerenv_present = Path::new("/.dockerenv").exists();
    let cgroup = fs::read_to_string("/proc/1/cgroup").ok();
    classify_host_environment_from_markers(dockerenv_present, cgroup.as_deref())
}

fn total_memory_mb() -> Option<u64> {
    let mut system = System::new();
    system.refresh_memory();
    let total_bytes = system.total_memory();
    (total_bytes > 0).then_some(total_bytes / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use flotilla_protocol::HostEnvironment;

    use super::*;
    use crate::{
        environment_manager::EnvironmentManager,
        providers::{
            discovery::{EnvironmentAssertion, EnvironmentBag},
            environment::{EnvironmentHandle, ProvisionedEnvironment},
            CommandRunner,
        },
    };

    struct TestEnvVars;

    impl EnvVars for TestEnvVars {
        fn get(&self, key: &str) -> Option<String> {
            match key {
                "HOME" => Some("/home/tester".into()),
                _ => None,
            }
        }
    }

    struct TestProvisionedEnvironment {
        id: flotilla_protocol::EnvironmentId,
        image: flotilla_protocol::ImageId,
        status: flotilla_protocol::EnvironmentStatus,
        runner: Arc<dyn CommandRunner>,
    }

    #[async_trait]
    impl ProvisionedEnvironment for TestProvisionedEnvironment {
        fn id(&self) -> &flotilla_protocol::EnvironmentId {
            &self.id
        }

        fn image(&self) -> &flotilla_protocol::ImageId {
            &self.image
        }

        fn container_name(&self) -> Option<&str> {
            None
        }

        fn provisioned_mounts(&self) -> Vec<crate::providers::environment::ProvisionedMount> {
            vec![]
        }

        async fn status(&self) -> Result<flotilla_protocol::EnvironmentStatus, String> {
            Ok(self.status.clone())
        }

        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(HashMap::new())
        }

        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::clone(&self.runner)
        }

        async fn destroy(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn classify_host_environment_unknown_without_markers() {
        let env = classify_host_environment_from_markers(false, None);
        assert_eq!(env, HostEnvironment::Unknown);
    }

    #[tokio::test]
    async fn build_local_host_summary_uses_manager_backed_local_inventory() {
        use flotilla_protocol::{qualified_path::HostId, EnvironmentId};

        let host_name = HostName::new("test-host");
        let manager = EnvironmentManager::from_local_state(
            EnvironmentId::new("test-local-environment"),
            HostId::new("test-local-host-id"),
            Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0")),
        );
        let env = TestEnvVars;

        let summary = build_local_host_summary(&NodeId::new("test-node"), &host_name, &manager, vec![], &env).await;

        assert_eq!(summary.inventory.binaries.len(), 1);
        assert_eq!(summary.inventory.binaries[0].name, "git");
    }

    #[tokio::test]
    async fn build_local_host_summary_populates_provisioned_environments_from_manager() {
        use flotilla_protocol::{qualified_path::HostId, EnvironmentId, EnvironmentStatus, ImageId};

        let host_name = HostName::new("test-host");
        let manager = EnvironmentManager::from_local_state(
            EnvironmentId::new("test-local-environment"),
            HostId::new("test-local-host-id"),
            Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
        );
        let env = TestEnvVars;
        let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
            id: EnvironmentId::new("env-1"),
            image: ImageId::new("test-image:latest"),
            status: EnvironmentStatus::Running,
            runner: Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
        });

        manager
            .register_provisioned_environment(EnvironmentId::new("env-1"), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let summary = build_local_host_summary(&NodeId::new("test-node"), &host_name, &manager, vec![], &env).await;

        assert_eq!(summary.environments.len(), 1);

        let provisioned = summary
            .environments
            .iter()
            .find_map(|environment| match environment {
                flotilla_protocol::EnvironmentInfo::Provisioned { id, image, status, .. } => Some((id, image, status)),
                _ => None,
            })
            .expect("provisioned environment should be visible");
        assert_eq!(provisioned.0, &EnvironmentId::new("env-1"));
        assert_eq!(provisioned.1, &ImageId::new("test-image:latest"));
        assert_eq!(provisioned.2, &EnvironmentStatus::Running);
    }

    #[tokio::test]
    async fn build_local_host_summary_keeps_direct_environments_out_of_summary_environments() {
        use flotilla_protocol::{qualified_path::HostId, EnvironmentId, EnvironmentStatus, ImageId};

        let host_name = HostName::new("test-host");
        let manager = EnvironmentManager::from_local_state(
            EnvironmentId::new("test-local-environment"),
            HostId::new("test-local-host-id"),
            Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
        );
        let env = TestEnvVars;
        let direct_env_id = EnvironmentId::new("direct-env");
        manager
            .register_direct_environment(
                direct_env_id.clone(),
                Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
                EnvironmentBag::new(),
                Some(HostId::new("direct-host-id")),
            )
            .expect("register direct environment");

        let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
            id: EnvironmentId::new("env-1"),
            image: ImageId::new("test-image:latest"),
            status: EnvironmentStatus::Running,
            runner: Arc::new(crate::providers::discovery::test_support::DiscoveryMockRunner::builder().build()),
        });
        manager
            .register_provisioned_environment(EnvironmentId::new("env-1"), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let summary = build_local_host_summary(&NodeId::new("test-node"), &host_name, &manager, vec![], &env).await;

        assert_eq!(summary.environments.len(), 1);
        match &summary.environments[0] {
            flotilla_protocol::EnvironmentInfo::Provisioned { id, image, status, .. } => {
                assert_eq!(id, &EnvironmentId::new("env-1"));
                assert_eq!(image, &ImageId::new("test-image:latest"));
                assert_eq!(status, &EnvironmentStatus::Running);
            }
            other => panic!("expected only provisioned environment in summary, got {other:?}"),
        }
        assert!(manager.environment_bag(&direct_env_id).is_some());
    }
}
