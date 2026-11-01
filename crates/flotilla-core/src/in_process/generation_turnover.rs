#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use std::path::Path;

use flotilla_discovery::generation_turnover::{AdvanceRequest, GenerationTurnovers, RecordedSession, RecordedSessions};
use flotilla_resources::{Environment, HostCondition, TerminalSession, TerminalSessionPhase};
use flotilla_terminal::generation::{persist, RollReport};

use super::InProcessDaemon;

impl InProcessDaemon {
    fn generation_turnovers(&self) -> GenerationTurnovers<'_> {
        GenerationTurnovers::new(&self.environment_manager, &self.pool_turnovers)
    }

    async fn recorded_sessions(&self) -> Result<RecordedSessions, String> {
        let namespaces = self.resource_backend.local_namespaces::<TerminalSession>().await.map_err(|error| error.to_string())?;
        let mut recorded = RecordedSessions::default();
        for namespace in namespaces {
            let sessions = self.resource_backend.using::<TerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
            for session in sessions.items {
                let (session, _) = flotilla_store::read_terminal_session(self.lifecycle_reads(), &session, None)
                    .await
                    .map_err(|error| error.to_string())?;
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
                let pool = session.spec.pool.clone();
                let endpoint = session.status.and_then(|status| status.cleat_endpoint);
                if let Some(environment) = self.resolve_environment_ref(&session.spec.env_ref) {
                    recorded.running.push(RecordedSession { pool, environment: environment.id, vessel, endpoint });
                } else if endpoint.is_some() {
                    let environment = self.resource_backend.using::<Environment>(&namespace).get(&session.spec.env_ref).await.ok();
                    if environment
                        .as_ref()
                        .and_then(|environment| environment.spec.docker.as_ref())
                        .is_some_and(|docker| docker.host_ref == self.environment_manager.local_host_id().as_str())
                    {
                        let message = format!("{vessel}: {pool} environment runner unavailable: {}", session.spec.env_ref);
                        recorded.unavailable.push((pool, message));
                    } else if environment
                        .as_ref()
                        .and_then(|environment| environment.spec.host_direct.as_ref())
                        .is_some_and(|direct| direct.host_ref == self.environment_manager.local_host_id().as_str())
                    {
                        let message = format!("host {pool} environment unavailable: {}", session.spec.env_ref);
                        recorded.unavailable.push((pool, message));
                    }
                    // Missing or foreign environment evidence cannot establish
                    // actionable local host skew.
                }
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

    use async_trait::async_trait;
    use flotilla_protocol::{qualified_path::HostId, result_set::CleatEndpoint, EnvironmentId, HostName, ImageId};
    use flotilla_resources::{ConditionValue, InputMeta, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus};
    use flotilla_store::ResourceBackend;

    use super::*;
    use crate::testkits::discovery::fake_discovery;
    use crate::testkits::discovery::fake_discovery_with_runner;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use crate::testkits::replay::testing::MockRunner;
    use flotilla_config::ConfigStore;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_environment::environment::ProvisionedEnvironment;
    use flotilla_environment::environment::ProvisionedMount;
    use flotilla_provider_runtime::CommandRunner;

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
        let backend = ResourceBackend::new(InMemoryBackend::default());
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
        let backend = ResourceBackend::new(InMemoryBackend::default());
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
        backend.using::<Environment>("flotilla").create(&InputMeta::builder().name("vessel-env".into()).build(), &spec).await.expect("env");
        let sessions = backend.using::<TerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("coder".into())
                    .labels(std::collections::BTreeMap::from([(flotilla_resources::VESSEL_REF_LABEL.into(), "work-vessel".into())]))
                    .build(),
                &TerminalSessionSpec::builder()
                    .env_ref("vessel-env".into())
                    .role("coder".into())
                    .source(TerminalSessionSource::Tool { command: "sh".into() })
                    .cwd("/".into())
                    .pool("cleat".into())
                    .build(),
            )
            .await
            .expect("session");
        sessions
            .update_status(
                "coder",
                &session.metadata.resource_version,
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
            .expect("endpoint");
        (daemon, directory)
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
            ResourceBackend::new(InMemoryBackend::default()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("cleat", "/installed/cleat"))
            .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/crew/cleat"));
        daemon.environment_manager.update_direct_environment_bag(daemon.local_environment_id(), bag).expect("local bag");
        let condition = daemon.generation_conditions().await.into_iter().next().expect("skew condition");
        assert_eq!(condition.condition_type, "CleatBuildSkew");
        assert_eq!(condition.value, ConditionValue::False);
        assert!(!condition.blocks_readiness());
        assert!(condition.message.contains("/crew/cleat/default: installed new vs serving old"));
        assert!(daemon.generation_conditions().await.is_empty());
    }
}
