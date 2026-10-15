#[cfg(test)]
mod tests {
    use flotilla_projections::host_summary::*;
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use flotilla_protocol::HostEnvironment;

    use crate::testkits::discovery::DiscoveryMockRunner;
    use flotilla_environment::environment::ProvisionedMount;

    // Glue: the sampler queries only the current process and reports bytes.
    #[test]
    fn daemon_rss_reports_resident_bytes() {
        assert!(daemon_rss_bytes().is_some_and(|bytes| bytes > 0));
    }

    use crate::testkits::discovery::TestEnvVars as FixtureEnvVars;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_environment::environment::EnvironmentHandle;
    use flotilla_environment::environment::ProvisionedEnvironment;
    use flotilla_executor::environment_manager::EnvironmentManager;
    use flotilla_protocol::{HostName, NodeId};
    use flotilla_provider_runtime::{CommandRunner, EnvVars};

    #[test]
    fn system_info_uses_fixture_host_os() {
        let env = FixtureEnvVars::default().with_host_os("macos");
        assert_eq!(collect_system_info(&env).os.as_deref(), Some("macos"));
    }

    struct TestEnvVars;

    impl EnvVars for TestEnvVars {
        fn get(&self, key: &str) -> Option<String> {
            match key {
                "HOME" => Some("/home/tester".into()),
                _ => None,
            }
        }

        fn host_os(&self) -> &str {
            "linux"
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

        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
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
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new().with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0")),
            Arc::new(vec![]),
        );
        let env = TestEnvVars;

        let summary = build_local_host_summary(
            &NodeId::new("test-node"),
            &host_name,
            EnvironmentId::host(HostId::new("test-local-host-id")),
            &super::super::projection_ports::ProjectionEnvironments(&manager),
            vec![],
            &env,
        )
        .await;

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
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
            Arc::new(vec![]),
        );
        let env = TestEnvVars;
        let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
            id: EnvironmentId::new("env-1"),
            image: ImageId::new("test-image:latest"),
            status: EnvironmentStatus::Running,
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
        });

        manager
            .register_provisioned_environment(EnvironmentId::new("env-1"), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let summary = build_local_host_summary(
            &NodeId::new("test-node"),
            &host_name,
            EnvironmentId::host(HostId::new("test-local-host-id")),
            &super::super::projection_ports::ProjectionEnvironments(&manager),
            vec![],
            &env,
        )
        .await;

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
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
            Arc::new(vec![]),
        );
        let env = TestEnvVars;
        let direct_env_id = EnvironmentId::new("direct-env");
        manager
            .register_direct_environment(
                direct_env_id.clone(),
                Arc::new(DiscoveryMockRunner::builder().build()),
                EnvironmentBag::new(),
                Some(HostId::new("direct-host-id")),
            )
            .expect("register direct environment");

        let handle: EnvironmentHandle = Arc::new(TestProvisionedEnvironment {
            id: EnvironmentId::new("env-1"),
            image: ImageId::new("test-image:latest"),
            status: EnvironmentStatus::Running,
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
        });
        manager
            .register_provisioned_environment(EnvironmentId::new("env-1"), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let summary = build_local_host_summary(
            &NodeId::new("test-node"),
            &host_name,
            EnvironmentId::host(HostId::new("test-local-host-id")),
            &super::super::projection_ports::ProjectionEnvironments(&manager),
            vec![],
            &env,
        )
        .await;

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
