use std::{collections::HashMap, path::Path};

use flotilla_protocol::result_set::CleatEndpoint;
use flotilla_resources::{ConditionValue, HostCondition, TerminalSession};

use super::InProcessDaemon;
use crate::{
    cleat_roll::{self, CleatEnvironment, CleatTarget, RollReport},
    environment_manager::ManagedEnvironmentKind,
    providers::ChannelLabel,
};

impl InProcessDaemon {
    async fn crew_cleat_targets(&self, require_local: bool) -> Result<(Vec<CleatTarget>, Vec<String>), String> {
        let namespaces = self.resource_backend.local_namespaces::<TerminalSession>().await.map_err(|error| error.to_string())?;
        let mut endpoints = HashMap::<_, Vec<CleatEndpoint>>::new();
        let mut errors = vec![];
        for namespace in namespaces {
            let sessions = self.resource_backend.using::<TerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
            for session in sessions.items.into_iter().filter(|session| session.spec.pool == "cleat") {
                if let Some(environment) = self.resolve_environment_ref(&session.spec.env_ref) {
                    if let Some(endpoint) = session.status.and_then(|status| status.cleat_endpoint) {
                        endpoints.entry(environment.id).or_default().push(endpoint);
                    }
                } else if session.status.is_some_and(|status| status.cleat_endpoint.is_some()) {
                    errors.push(format!("crew cleat environment unavailable: {}", session.spec.env_ref));
                }
            }
        }
        let mut targets = vec![];
        for (id, state) in self.environment_manager.managed_environments() {
            let (bag, local, contained) = match state {
                ManagedEnvironmentKind::Direct(state) => {
                    (state.env_bag, state.host_id.as_ref() == Some(self.environment_manager.local_host_id()), false)
                }
                ManagedEnvironmentKind::Provisioned(state) => {
                    (state.env_bag, &state.owning_host_id == self.environment_manager.local_host_id(), true)
                }
            };
            if !local
                || (bag.find_binary("cleat").is_none()
                    && !endpoints.contains_key(&id)
                    && !(require_local && id == self.local_environment_id))
            {
                continue;
            }
            let Some(runner) = self.environment_manager.environment_runner(&id) else {
                errors.push(format!("crew cleat runner unavailable for {id}"));
                continue;
            };
            let environment = CleatEnvironment::builder().id(id.clone()).bag(bag).runner(runner).contained(contained).build();
            match cleat_roll::crew_targets(&environment, endpoints.get(&id).map_or(&[], Vec::as_slice)) {
                Ok(environment_targets) => targets.extend(environment_targets),
                Err(error) => errors.push(format!("{id}: {error}")),
            }
        }
        Ok((targets, errors))
    }

    pub async fn post_install_cleat(&self, incoming: &Path, generation: &str, diagnostics_dir: &Path) -> Result<RollReport, String> {
        let (targets, errors) = self.crew_cleat_targets(true).await?;
        let mut report = cleat_roll::drain(self.host_name.to_string(), generation.to_string(), incoming, &targets, errors).await;
        cleat_roll::persist(&mut report, diagnostics_dir).await;
        Ok(report)
    }

    pub async fn cleat_build_skew_condition(&self) -> Option<HostCondition> {
        let bag = self.local_environment_bag()?;
        let binary = bag.find_binary("cleat")?;
        let runner = self.local_command_runner()?;
        let output = runner
            .run_with_timeout(
                &binary.as_path().display().to_string(),
                &["version", "--json"],
                Path::new("/"),
                &ChannelLabel::Default,
                std::time::Duration::from_secs(5),
            )
            .await;
        let installed = output
            .ok()
            .and_then(|output| serde_json::from_str::<serde_json::Value>(&output).ok())
            .and_then(|value| value["client"]["git_sha"].as_str().map(str::to_string));
        let (targets, mut messages) = match self.crew_cleat_targets(false).await {
            Ok(targets) => targets,
            Err(error) => (vec![], vec![error]),
        };
        messages.extend(cleat_roll::build_skew(installed.as_deref(), &targets).await);
        if messages.is_empty() {
            return None;
        }
        Some(
            HostCondition::builder()
                .condition_type("CleatBuildSkew")
                .value(ConditionValue::False)
                .reason("InstalledServingMismatch")
                .message(messages.join("; "))
                .observed_at(self.clock.now())
                .blocks_readiness(false)
                .build(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use flotilla_protocol::{qualified_path::HostId, EnvironmentId, EnvironmentStatus, HostName, ImageId};
    use flotilla_resources::{
        InMemoryBackend, InputMeta, ResourceBackend, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
    };

    use super::*;
    use crate::{
        config::ConfigStore,
        providers::{
            discovery::{
                test_support::{fake_discovery, fake_discovery_with_runner, DiscoveryMockRunner},
                EnvironmentAssertion, EnvironmentBag,
            },
            environment::{ProvisionedEnvironment, ProvisionedMount},
            testing::MockRunner,
            CommandRunner,
        },
    };

    struct Contained {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
    }
    #[async_trait]
    impl ProvisionedEnvironment for Contained {
        fn id(&self) -> &EnvironmentId {
            &self.id
        }
        fn image(&self) -> &ImageId {
            &self.image
        }
        fn container_name(&self) -> Option<&str> {
            Some("work-container")
        }
        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
            vec![]
        }
        async fn status(&self) -> Result<EnvironmentStatus, String> {
            Ok(EnvironmentStatus::Running)
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

    // Owner ruling: enumerate native crew endpoints across namespaces and managed
    // contained environments; never drain another host's registered environment.
    #[tokio::test]
    async fn inventory_includes_contained_and_named_crew_roots_but_excludes_remote_hosts() {
        let directory = tempfile::tempdir().expect("config");
        std::fs::write(directory.path().join("daemon.toml"), "machine_id = \"cleat-roll-test\"\n").expect("daemon identity");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path())),
            fake_discovery(false),
            HostName::new("host"),
            backend.clone(),
        )
        .await;
        let local_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", "/host-home"))
            .with(EnvironmentAssertion::env_var("CLEAT_DAEMON", "local-named@2"));
        daemon.environment_manager.update_direct_environment_bag(daemon.local_environment_id(), local_bag).expect("local bag");
        let runner = Arc::new(MockRunner::with_outputs(vec![]));
        let id = EnvironmentId::new("contained-work");
        let handle = Arc::new(Contained { id: id.clone(), image: ImageId::new("crew-image"), runner: runner.clone() });
        let contained_bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"))
            .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/var/lib/flotilla/cleat"));
        daemon.register_provisioned_environment(id.clone(), handle, contained_bag.clone(), None).expect("contained environment");
        daemon
            .register_direct_environment_for_test(
                EnvironmentId::new("remote"),
                runner.clone(),
                contained_bag,
                Some(HostId::new("another-host")),
            )
            .expect("remote environment");
        for (namespace, name, env_ref, daemon_name) in [
            ("other-namespace", "first", "contained-work", "named@1"),
            ("flotilla", "second", "contained-work", "named@2"),
            ("other-namespace", "remote", "remote", "foreign@1"),
        ] {
            let resolver = backend.using::<TerminalSession>(namespace);
            let session = resolver
                .create(
                    &InputMeta::builder().name(name.to_string()).build(),
                    &TerminalSessionSpec::builder()
                        .env_ref(env_ref.to_string())
                        .role("coder".to_string())
                        .source(TerminalSessionSource::Tool { command: "sh".into() })
                        .cwd("/".to_string())
                        .pool("cleat".to_string())
                        .build(),
                )
                .await
                .expect("crew session");
            resolver
                .update_status(name, &session.metadata.resource_version, &TerminalSessionStatus {
                    cleat_endpoint: Some(CleatEndpoint {
                        runtime_root: "/named/crew root".into(),
                        daemon: daemon_name.into(),
                        session: name.into(),
                    }),
                    ..Default::default()
                })
                .await
                .expect("record crew endpoint");
        }
        let (targets, errors) = daemon.crew_cleat_targets(true).await.expect("known targets");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(targets.len(), 4);
        assert!(targets.iter().any(|target| target.runtime_root == Path::new("/host-home/.local/state/cleat") && target.name == "default"));
        assert!(targets.iter().any(|target| target.environment == "contained-work"
            && target.runtime_root == Path::new("/var/lib/flotilla/cleat")
            && target.name == "default"
            && target.contained));
        assert_eq!(targets.iter().filter(|target| target.name == "named").count(), 1);
        assert!(!targets.iter().any(|target| target.environment == "remote"));
        assert!(runner.calls().is_empty(), "enumeration is a resource/environment read");
    }

    // The advisory host condition gives fleet-wide users exact installed and
    // serving SHAs. A later matching observation clears it without blocking crews.
    #[tokio::test]
    async fn host_skew_condition_is_advisory_and_clears_when_serving_matches() {
        let version_args = ["--runtime-root", "/crew/cleat", "--server", "default", "version", "--daemon", "--json"];
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("/installed/cleat", &["version", "--json"], Ok(r#"{"client":{"git_sha":"new"}}"#.into()))
                .on_run("/installed/cleat", &["version", "--json"], Ok(r#"{"client":{"git_sha":"new"}}"#.into()))
                .on_run("/installed/cleat", &version_args, Ok(r#"{"daemon":{"git_sha":"old"}}"#.into()))
                .on_run("/installed/cleat", &version_args, Ok(r#"{"daemon":{"git_sha":"new"}}"#.into()))
                .build(),
        );
        let directory = tempfile::tempdir().expect("config");
        std::fs::write(directory.path().join("daemon.toml"), "machine_id = \"cleat-roll-test\"\n").expect("daemon identity");
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path())),
            fake_discovery_with_runner(false, runner),
            HostName::new("host"),
            ResourceBackend::InMemory(InMemoryBackend::default()),
        )
        .await;
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("cleat", "/installed/cleat"))
            .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/crew/cleat"));
        daemon.environment_manager.update_direct_environment_bag(daemon.local_environment_id(), bag).expect("local bag");
        let condition = daemon.cleat_build_skew_condition().await.expect("skew condition");
        assert_eq!(condition.condition_type, "CleatBuildSkew");
        assert_eq!(condition.value, ConditionValue::False);
        assert!(!condition.blocks_readiness());
        assert!(condition.message.contains("/crew/cleat/default: installed new vs serving old"));
        assert!(daemon.cleat_build_skew_condition().await.is_none());
    }
}
