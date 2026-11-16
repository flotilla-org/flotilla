#[cfg(test)]
use flotilla_store_memory::engine_home_backend;
use std::path::Path;

use flotilla_discovery::generation_turnover::{AdvanceRequest, GenerationTurnovers, RecordedSession, RecordedSessions};
use flotilla_resources::HostCondition;
use flotilla_terminal::generation::{persist, RollReport};

use super::InProcessDaemon;

impl InProcessDaemon {
    fn generation_turnovers(&self) -> GenerationTurnovers<'_> {
        GenerationTurnovers::new(&self.environment_manager, &self.pool_turnovers)
    }

    /// This home's running terminal launches, from the engine's records (#3108).
    async fn recorded_sessions(&self) -> Result<RecordedSessions, String> {
        let reads = self.lifecycle_reads();
        let terminals = reads.terminals()?;
        let mut recorded = RecordedSessions::default();
        // Ended launches are retained history, not live daemon inventory.
        for record in terminals.at(terminals.home())?.into_iter().filter(|record| record.is_running()) {
            let vessel = record.vessel.clone().unwrap_or_else(|| record.environment.clone());
            let endpoint = record.cleat_endpoint();
            let (pool, env_ref) = (record.pool, record.environment);
            if let Some(environment) = self.resolve_environment_ref(&env_ref) {
                recorded.running.push(RecordedSession { pool, environment: environment.id, vessel, endpoint });
            } else if endpoint.is_some() {
                let environment =
                    self.lifecycle_reads().environments().ok().and_then(|environments| environments.find(&env_ref).ok().flatten());
                if environment
                    .as_ref()
                    .and_then(|environment| environment.spec.docker.as_ref())
                    .is_some_and(|docker| docker.host_ref == self.environment_manager.local_host_id().as_str())
                {
                    let message = format!("{vessel}: {pool} environment runner unavailable: {}", env_ref);
                    recorded.unavailable.push((pool, message));
                } else if environment
                    .as_ref()
                    .and_then(|environment| environment.spec.host_direct.as_ref())
                    .is_some_and(|direct| direct.host_ref == self.environment_manager.local_host_id().as_str())
                {
                    let message = format!("host {pool} environment unavailable: {}", env_ref);
                    recorded.unavailable.push((pool, message));
                }
                // Missing or foreign environment evidence cannot establish
                // actionable local host skew.
            }
        }
        Ok(recorded)
    }

    pub async fn post_install_cleat(&self, incoming: &Path, generation: &str, diagnostics_dir: &Path) -> Result<RollReport, String> {
        self.wait_for_environment_adoption().await;
        let sessions = self.recorded_sessions().await?;
        let previous = self.turnover_report.lock().await.clone();
        let tools_root = self.config.state_dir().join("environment-tools/fleet");
        let mut report = self
            .generation_turnovers()
            .advance(AdvanceRequest {
                host: self.host_name.to_string(),
                generation,
                incoming,
                tools_root: tools_root.as_path(),
                sessions: &sessions,
                previous: previous.as_ref(),
            })
            .await;
        persist(&mut report, diagnostics_dir).await;
        *self.turnover_report.lock().await = Some(report.clone());
        Ok(report)
    }

    pub async fn generation_conditions(&self) -> Vec<HostCondition> {
        let sessions =
            self.recorded_sessions().await.unwrap_or_else(|error| RecordedSessions { errors: vec![error], ..Default::default() });
        let previous = self.turnover_report.lock().await.clone();
        self.generation_turnovers().conditions(&sessions, previous.as_ref(), self.clock.now()).await
    }
}

#[cfg(test)]
mod tests {
    use crate::testkits::in_process_ext::InProcessDiscoveryExt;
    use std::{collections::HashMap, sync::Arc};

    use super::*;
    use crate::testkits::discovery::fake_discovery;
    use crate::testkits::discovery::fake_discovery_with_runner;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use crate::testkits::replay::testing::MockRunner;
    use async_trait::async_trait;
    use flotilla_config::ConfigStore;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_environment::environment::ProvisionedEnvironment;
    use flotilla_environment::environment::ProvisionedMount;
    use flotilla_protocol::{qualified_path::HostId, EnvironmentId, HostName, ImageId};
    use flotilla_provider_runtime::CommandRunner;
    use flotilla_resources::{
        terminal_records::{TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource, TerminalRecords},
        ConditionValue,
    };

    struct Contained {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
    }
    #[async_trait]
    impl ProvisionedEnvironment for Contained {
        flotilla_discovery_testkit::injected_environment_handle!();

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
        async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
            flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Running)
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
        let backend = engine_home_backend();
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path())),
            fake_discovery(false),
            HostName::new("host"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
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
        let environments = flotilla_provisioning_testkit::EnvironmentFixture::default();
        let terminals = flotilla_provisioning_testkit::TerminalsFixture::over("home", Arc::clone(&environments.store));
        for (name, env_ref, daemon_name) in
            [("first", "contained-work", "named@1"), ("second", "contained-work", "named@2"), ("remote", "remote", "foreign@1")]
        {
            running_session(&terminals, name, None, env_ref, "/named/crew root", daemon_name);
        }
        let environment_spec = serde_json::from_value(serde_json::json!({"docker": {
            "host_ref": daemon.environment_manager.local_host_id().as_str(), "image": "pinned-image"
        }}))
        .expect("stored Docker spec");
        environments.request("unregistered-vessel-env", &environment_spec, None);
        running_session(
            &terminals,
            "missing-runner",
            Some("work-vessel"),
            "unregistered-vessel-env",
            "/var/lib/flotilla/cleat",
            "default@1",
        );
        let environments = environments.serving_terminals(Arc::clone(&terminals.terminals) as Arc<dyn TerminalRecords>);
        daemon.install_lifecycle_reads(environments.reads.clone()).expect("engine reads");
        let report = daemon
            .post_install_cleat(
                &directory.path().join("missing-release/bin/cleat"),
                "test-generation",
                &directory.path().join("diagnostics"),
            )
            .await
            .unwrap();
        let unavailable: Vec<_> = report.errors.iter().filter(|error| error.contains("runner unavailable")).collect();
        assert_eq!(unavailable.len(), 1);
        assert!(unavailable[0].contains("work-vessel: cleat environment runner unavailable"));
        assert!(!report.information.iter().any(|information| information.contains("offers no generation turnover")));
        let targets = &report.attempts;
        assert!(targets.iter().any(|target| target.runtime_root == Path::new("/host-home/.local/state/cleat") && target.name == "default"));
        assert!(targets.iter().any(|target| target.environment == "contained-work"
            && target.runtime_root == Path::new("/var/lib/flotilla/cleat")
            && target.name == "default"));
        assert_eq!(targets.iter().filter(|target| target.name == "named").count(), 1);
        assert!(!targets.iter().any(|target| target.environment == "remote"));
        // Missing incoming artifacts produce an attempt for every target and
        // leave the contained runners untouched.
        assert_eq!(report.attempts.len(), 4);
        assert!(report
            .attempts
            .iter()
            .filter(|attempt| attempt.environment == "contained-work")
            .all(|attempt| { attempt.error.as_ref().is_some_and(|error| error.contains("NoPlatformBuild")) }));
        assert!(!report.errors.iter().any(|error| error.starts_with("prune vessel tools:")));
        assert!(runner.calls().is_empty(), "publication failure must leave contained sessions alone");
        // A malformed installed path is another diagnostic publication failure,
        // not an early return that suppresses independent runtime attempts.
        let malformed = daemon
            .post_install_cleat(Path::new("/cleat"), "test-generation", &directory.path().join("diagnostics"))
            .await
            .expect("captured failure");
        assert!(malformed.errors.iter().any(|error| error.contains("incoming cleat has no release root")));
        assert_eq!(malformed.attempts.len(), 4);
        assert!(malformed.diagnostics_path.expect("durable report").exists());
    }

    async fn daemon_with_unadopted_vessel() -> (Arc<InProcessDaemon>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("config");
        std::fs::write(directory.path().join("daemon.toml"), "machine_id = \"cleat-roll-test\"\n").expect("daemon identity");
        let backend = engine_home_backend();
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path())),
            fake_discovery(false),
            HostName::new("host"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let spec = serde_json::from_value(serde_json::json!({"docker": {
            "host_ref": daemon.environment_manager.local_host_id().as_str(), "image": "pinned-image"
        }}))
        .expect("stored Docker spec");
        let environments = flotilla_provisioning_testkit::EnvironmentFixture::default();
        environments.request("vessel-env", &spec, None);
        let terminals = flotilla_provisioning_testkit::TerminalsFixture::over("home", Arc::clone(&environments.store));
        running_session(&terminals, "coder", Some("work-vessel"), "vessel-env", "/var/lib/flotilla/cleat", "default@1");
        let environments = environments.serving_terminals(Arc::clone(&terminals.terminals) as Arc<dyn TerminalRecords>);
        daemon.install_lifecycle_reads(environments.reads.clone()).expect("engine reads");
        (daemon, directory)
    }

    /// A tool session `name` running in `env_ref` for `vessel`, its cleat daemon `daemon` under
    /// `runtime_root`.
    fn running_session(
        terminals: &flotilla_provisioning_testkit::TerminalsFixture,
        name: &str,
        vessel: Option<&str>,
        env_ref: &str,
        runtime_root: &str,
        daemon: &str,
    ) {
        let launch = terminals.request(&TerminalLaunchRequest {
            session: name.into(),
            environment: env_ref.into(),
            vessel: vessel.map(str::to_owned),
            role: "coder".into(),
            pool: "cleat".into(),
            cwd: "/".into(),
            env: Default::default(),
            source: TerminalLaunchSource::Tool { command: "sh".into() },
        });
        let facts = TerminalLaunchFacts { endpoint: Some((runtime_root.into(), daemon.into())), ..TerminalLaunchFacts::default() };
        terminals.ready(&launch, name, &facts);
    }

    fn adopt_vessel_env(daemon: &InProcessDaemon) {
        let id = EnvironmentId::new("vessel-env");
        let handle =
            Arc::new(Contained { id: id.clone(), image: ImageId::new("crew-image"), runner: Arc::new(MockRunner::with_outputs(vec![])) });
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"))
            .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/var/lib/flotilla/cleat"));
        daemon.register_provisioned_environment(id, handle, bag, None).expect("adopt");
    }

    #[tokio::test(start_paused = true)]
    async fn post_install_waits_for_pending_adoption_then_turns_over_the_vessel() {
        let (daemon, directory) = daemon_with_unadopted_vessel().await;
        daemon.begin_environment_adoption();
        let adopter = Arc::clone(&daemon);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            adopt_vessel_env(&adopter);
            adopter.finish_environment_adoption();
        });
        let report = daemon
            .post_install_cleat(Path::new("/missing/release/bin/cleat"), "gen", &directory.path().join("diagnostics"))
            .await
            .expect("report");
        assert!(!report.errors.iter().any(|error| error.contains("runner unavailable")), "{:?}", report.errors);
        assert!(report.attempts.iter().any(|attempt| attempt.environment == "vessel-env"));
    }

    #[tokio::test(start_paused = true)]
    async fn post_install_reports_environment_that_never_comes_back_after_the_deadline() {
        let (daemon, directory) = daemon_with_unadopted_vessel().await;
        daemon.begin_environment_adoption();
        let report = daemon
            .post_install_cleat(Path::new("/missing/release/bin/cleat"), "gen", &directory.path().join("diagnostics"))
            .await
            .expect("report");
        assert!(
            report.errors.iter().any(|error| error == "work-vessel: cleat environment runner unavailable: vessel-env"),
            "{:?}",
            report.errors
        );
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
            engine_home_backend(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        // The engine's readings, which a running daemon always has: no launches run here.
        daemon.install_lifecycle_reads(flotilla_provisioning_testkit::EngineFixture::default().reads()).expect("engine reads");
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("cleat", "/installed/cleat"))
            .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/crew/cleat"));
        daemon.environment_manager.update_direct_environment_bag(daemon.local_environment_id(), bag).expect("local bag");
        let condition = daemon
            .generation_conditions()
            .await
            .into_iter()
            .find(|condition| condition.condition_type == "CleatBuildSkew")
            .expect("skew condition");
        assert_eq!(condition.condition_type, "CleatBuildSkew");
        assert_eq!(condition.value, ConditionValue::False);
        assert!(!condition.blocks_readiness());
        assert!(condition.message.contains("/crew/cleat/default: installed new vs serving old"));
        assert!(!daemon.generation_conditions().await.iter().any(|condition| condition.condition_type == "CleatBuildSkew"));
    }
}
