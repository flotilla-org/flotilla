use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use flotilla_protocol::result_set::CleatEndpoint;
use flotilla_resources::{Environment, HostCondition, TerminalSession, TerminalSessionPhase};

use super::InProcessDaemon;
use crate::{
    cleat_roll::{self, CleatEnvironment, CleatTarget, RollReport},
    environment_manager::ManagedEnvironmentKind,
    providers::ChannelLabel,
};

#[derive(Default)]
struct CleatInventory {
    targets: Vec<CleatTarget>,
    errors: Vec<String>,
    information: BTreeSet<String>,
}

impl InProcessDaemon {
    async fn crew_cleat_targets(&self, require_local: bool) -> Result<CleatInventory, String> {
        let namespaces = self.resource_backend.local_namespaces::<TerminalSession>().await.map_err(|error| error.to_string())?;
        let mut endpoints = HashMap::<_, Vec<CleatEndpoint>>::new();
        let mut inventory = CleatInventory::default();
        let mut vessels = HashMap::<_, BTreeSet<String>>::new();
        for namespace in namespaces {
            let sessions = self.resource_backend.using::<TerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
            for session in sessions.items.into_iter().filter(|session| session.spec.pool == "cleat") {
                // Stopped records are retained history, not live daemon inventory.
                if session.status.as_ref().is_none_or(|status| status.phase != TerminalSessionPhase::Running) {
                    continue;
                }
                let vessel = session
                    .metadata
                    .labels
                    .get(flotilla_resources::VESSEL_REF_LABEL)
                    .cloned()
                    .unwrap_or_else(|| session.spec.env_ref.clone());
                if let Some(environment) = self.resolve_environment_ref(&session.spec.env_ref) {
                    if let Some(endpoint) = session.status.and_then(|status| status.cleat_endpoint) {
                        vessels.entry(environment.id.clone()).or_default().insert(vessel);
                        endpoints.entry(environment.id).or_default().push(endpoint);
                    }
                } else if session.status.is_some_and(|status| status.cleat_endpoint.is_some()) {
                    let environment = self.resource_backend.using::<Environment>(&namespace).get(&session.spec.env_ref).await.ok();
                    if environment
                        .as_ref()
                        .and_then(|environment| environment.spec.docker.as_ref())
                        .is_some_and(|docker| docker.host_ref == self.environment_manager.local_host_id().as_str())
                    {
                        inventory
                            .information
                            .insert(format!("{vessel}: vessel cleat unknown, refreshes on restart (environment runner unavailable)"));
                    } else if environment
                        .as_ref()
                        .and_then(|environment| environment.spec.host_direct.as_ref())
                        .is_some_and(|direct| direct.host_ref == self.environment_manager.local_host_id().as_str())
                    {
                        inventory.errors.push(format!("host cleat environment unavailable: {}", session.spec.env_ref));
                    }
                    // Missing or foreign environment evidence cannot establish
                    // actionable local host skew.
                }
            }
        }
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
                if contained {
                    inventory.information.insert(format!("{id}: vessel cleat unknown, refreshes on restart (runner unavailable)"));
                } else {
                    inventory.errors.push(format!("host cleat runner unavailable for {id}"));
                }
                continue;
            };
            let environment = CleatEnvironment::builder().id(id.clone()).bag(bag).runner(runner).contained(contained).build();
            match cleat_roll::crew_targets(&environment, endpoints.get(&id).map_or(&[], Vec::as_slice)) {
                Ok(environment_targets) => inventory.targets.extend(environment_targets.into_iter().map(|mut target| {
                    target.vessels = vessels.get(&id).map_or_else(Vec::new, |vessels| vessels.iter().cloned().collect());
                    target
                })),
                Err(error) if contained => {
                    inventory.information.insert(format!("{id}: vessel cleat unknown, refreshes on restart ({error})"));
                }
                Err(error) => inventory.errors.push(format!("{id}: {error}")),
            }
        }
        Ok(inventory)
    }

    pub async fn post_install_cleat(&self, incoming: &Path, generation: &str, diagnostics_dir: &Path) -> Result<RollReport, String> {
        let inventory = self.crew_cleat_targets(true).await?;
        let mut report =
            cleat_roll::drain(self.host_name.to_string(), generation.to_string(), incoming, &inventory.targets, inventory.errors).await;
        report.information.extend(inventory.information);
        cleat_roll::persist(&mut report, diagnostics_dir).await;
        *self.cleat_roll_report.lock().await = Some(report.clone());
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
        let inventory = match self.crew_cleat_targets(false).await {
            Ok(inventory) => inventory,
            Err(error) => CleatInventory { errors: vec![error], ..Default::default() },
        };
        let report = self.cleat_roll_report.lock().await.clone();
        let mut assessment = cleat_roll::build_skew(installed.as_deref(), &inventory.targets, report.as_ref()).await;
        assessment.actionable.extend(inventory.errors);
        assessment.information.extend(inventory.information);
        if let Some(report) = report.as_ref() {
            let drain = cleat_roll::assess_drain(report);
            assessment.actionable.extend(drain.actionable);
        }
        assessment.into_condition(self.clock.now())
    }
}

#[cfg(test)]
mod tests {
    use crate::testkits::discovery::InProcessDiscoveryExt;
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use flotilla_protocol::{qualified_path::HostId, EnvironmentId, EnvironmentStatus, HostName, ImageId};
    use flotilla_resources::{
        ConditionValue, InMemoryBackend, InputMeta, ResourceBackend, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
    };

    use super::*;
    use crate::config::ConfigStore;
    use crate::discovery_api::EnvironmentAssertion;
    use crate::discovery_api::EnvironmentBag;
    use crate::providers::environment::ProvisionedEnvironment;
    use crate::providers::environment::ProvisionedMount;
    use crate::providers::CommandRunner;
    use crate::testkits::discovery::fake_discovery;
    use crate::testkits::discovery::fake_discovery_with_runner;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use crate::testkits::replay::testing::MockRunner;

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
                .update_status(
                    name,
                    &session.metadata.resource_version,
                    &TerminalSessionStatus {
                        cleat_endpoint: Some(CleatEndpoint {
                            runtime_root: "/named/crew root".into(),
                            daemon: daemon_name.into(),
                            session: name.into(),
                        }),
                        phase: TerminalSessionPhase::Running,
                        ..Default::default()
                    },
                )
                .await
                .expect("record crew endpoint");
        }
        let environment_spec = serde_json::from_value(serde_json::json!({"docker": {
            "host_ref": daemon.environment_manager.local_host_id().as_str(), "image": "pinned-image"
        }}))
        .expect("stored Docker spec");
        backend
            .using::<Environment>("other-namespace")
            .create(&InputMeta::builder().name("unregistered-vessel-env".into()).build(), &environment_spec)
            .await
            .expect("stored environment");
        let sessions = backend.using::<TerminalSession>("other-namespace");
        let missing = sessions
            .create(
                &InputMeta::builder()
                    .name("missing-runner".into())
                    .labels(std::collections::BTreeMap::from([(flotilla_resources::VESSEL_REF_LABEL.into(), "work-vessel".into())]))
                    .build(),
                &TerminalSessionSpec::builder()
                    .env_ref("unregistered-vessel-env".into())
                    .role("coder".into())
                    .source(TerminalSessionSource::Tool { command: "sh".into() })
                    .cwd("/".into())
                    .pool("cleat".into())
                    .build(),
            )
            .await
            .expect("retained vessel session");
        sessions
            .update_status(
                "missing-runner",
                &missing.metadata.resource_version,
                &TerminalSessionStatus {
                    phase: TerminalSessionPhase::Running,
                    cleat_endpoint: Some(CleatEndpoint {
                        runtime_root: "/var/lib/flotilla/cleat".into(),
                        daemon: "default@1".into(),
                        session: "coder".into(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("retained endpoint");
        let inventory = daemon.crew_cleat_targets(true).await.expect("known targets");
        assert!(inventory.errors.is_empty(), "{:?}", inventory.errors);
        assert_eq!(inventory.information.len(), 1);
        assert!(inventory
            .information
            .iter()
            .next()
            .expect("vessel info")
            .contains("work-vessel: vessel cleat unknown, refreshes on restart"));
        let targets = inventory.targets;
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
        let version_args = [
            "-i",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "CLEAT_RUNTIME_DIR=/crew/cleat",
            "/installed/cleat",
            "--runtime-root",
            "/crew/cleat",
            "--server",
            "default",
            "version",
            "--daemon",
            "--json",
        ];
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("/installed/cleat", &["version", "--json"], Ok(r#"{"client":{"git_sha":"new"}}"#.into()))
                .on_run("/installed/cleat", &["version", "--json"], Ok(r#"{"client":{"git_sha":"new"}}"#.into()))
                .on_run("/usr/bin/env", &version_args, Ok(r#"{"daemon":{"git_sha":"old"}}"#.into()))
                .on_run("/usr/bin/env", &version_args, Ok(r#"{"daemon":{"git_sha":"new"}}"#.into()))
                .on_run(
                    "/usr/bin/env",
                    &[
                        "-i",
                        "PATH=/usr/local/bin:/usr/bin:/bin",
                        "CLEAT_RUNTIME_DIR=/crew/cleat",
                        "readlink",
                        "/crew/cleat/.default.current",
                    ],
                    Err("no sidecar".into()),
                )
                .on_run(
                    "/usr/bin/env",
                    &[
                        "-i",
                        "PATH=/usr/local/bin:/usr/bin:/bin",
                        "CLEAT_RUNTIME_DIR=/crew/cleat",
                        "readlink",
                        "/crew/cleat/.default.current",
                    ],
                    Err("no sidecar".into()),
                )
                .on_run(
                    "/usr/bin/env",
                    &["-i", "PATH=/usr/local/bin:/usr/bin:/bin", "CLEAT_RUNTIME_DIR=/crew/cleat", "readlink", "/crew/cleat/default"],
                    Err("legacy".into()),
                )
                .on_run(
                    "/usr/bin/env",
                    &["-i", "PATH=/usr/local/bin:/usr/bin:/bin", "CLEAT_RUNTIME_DIR=/crew/cleat", "readlink", "/crew/cleat/default"],
                    Err("legacy".into()),
                )
                .on_run(
                    "/usr/bin/env",
                    &[
                        "-i",
                        "PATH=/usr/local/bin:/usr/bin:/bin",
                        "CLEAT_RUNTIME_DIR=/crew/cleat",
                        "/installed/cleat",
                        "--runtime-root",
                        "/crew/cleat",
                        "--server",
                        "default",
                        "daemons",
                        "--json",
                    ],
                    Ok("[]".into()),
                )
                .on_run(
                    "/usr/bin/env",
                    &[
                        "-i",
                        "PATH=/usr/local/bin:/usr/bin:/bin",
                        "CLEAT_RUNTIME_DIR=/crew/cleat",
                        "/installed/cleat",
                        "--runtime-root",
                        "/crew/cleat",
                        "--server",
                        "default",
                        "daemons",
                        "--json",
                    ],
                    Ok("[]".into()),
                )
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
