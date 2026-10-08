use super::*;

// Process boundary only: the actual Docker provider decodes the real incident
// recording; this runner supplies its subprocess outputs without a live Docker.
struct MemoryIncidentRunner {
    status: &'static str,
    inspect_error: bool,
}

impl Default for MemoryIncidentRunner {
    fn default() -> Self {
        Self { status: "exited", inspect_error: false }
    }
}

#[async_trait]
impl CommandRunner for MemoryIncidentRunner {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        match cmd {
            "docker" if args.first() == Some(&"ps") => Ok("container\tenv-memory-incident\timage\t[]".into()),
            "docker" if args == ["inspect", "container"] && self.inspect_error => Err("inspect unavailable".into()),
            "docker" if args == ["inspect", "container"] => {
                Ok(include_str!("../../../../flotilla-core/src/providers/environment/fixtures/oomd-exit137.json").into())
            }
            "docker" if args.starts_with(&["inspect", "--format", "{{.State.Status}}"]) => Ok(self.status.into()),
            "journalctl" if args.contains(&"--unit=systemd-oomd") => {
                Ok(include_str!("../../../../flotilla-core/src/providers/environment/fixtures/oomd-journal.txt").into())
            }
            "journalctl" => Err("kernel journal denied".into()),
            _ => Err(format!("unexpected subprocess: {cmd} {args:?}")),
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        true
    }
}

// #2653: death evidence must survive backing loss, duplicate observations and
// environment deletion. Last successful usage and recovery paths stay visible.
#[tokio::test]
async fn memory_incident_survives_backing_loss_and_is_visible_in_explain() {
    use flotilla_protocol::{EnvironmentExitCause, EnvironmentMemoryLimits, EnvironmentRuntimeObservation};
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"memory-incident-test\"\n").expect("machine identity");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), config.clone()).await;
    let backend = daemon.resource_backend();
    let env_id = EnvironmentId::new("env-memory-incident");
    create_ready_docker_environment(&daemon, env_id.as_str(), "container", BTreeSet::new()).await;
    let convoy = backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("memory-incident"), &ConvoySpec::builder().workflow_ref("scratch".into()).build())
        .await
        .expect("convoy");
    backend
        .using::<Convoy>(NAMESPACE)
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                observed_workflow_ref: Some("scratch".into()),
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                    vessels: vec![VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![CrewSpec::builder().role("shell".into()).source(CrewSource::Tool { command: "true".into() }).build()])
                        .build()],
                }),
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([(
                        "shell".into(),
                        flotilla_resources::CrewWorkState::builder().phase(flotilla_resources::CrewWorkPhase::Working).build(),
                    )]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("admitted convoy");
    let repo = RepositoryKey("test-repository".into());
    let checkouts = backend.using::<ResourceCheckout>(NAMESPACE);
    checkouts
        .create(
            &empty_meta("recoverable-work"),
            &ResourceCheckoutSpec::Observed(
                ResourceObservedCheckoutSpec::builder()
                    .r#ref("fix/retained-work".into())
                    .path("/hulls/work/flotilla".into())
                    .repo_ref(repo.clone())
                    .host_ref("host".into())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("checkout");
    flotilla_resources::apply_status_patch(
        &checkouts,
        "recoverable-work",
        &flotilla_resources::CheckoutStatusPatch::MarkReady {
            path: "/hulls/work/flotilla".into(),
            commit: None,
            branch_provenance: Default::default(),
        },
    )
    .await
    .expect("checkout ready");
    let vessels = backend.using::<Vessel>(NAMESPACE);
    let vessel = vessels
        .create(
            &InputMeta::builder()
                .name("memory-incident-work".into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "memory-incident".into())]))
                .build(),
            &VesselSpec {
                convoy_ref: "memory-incident".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test-policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    vessels
        .update_status(
            &vessel.metadata.name,
            &vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some(env_id.to_string()),
                checkout_refs: BTreeMap::from([(repo, "recoverable-work".into())]),
                ..Default::default()
            },
        )
        .await
        .expect("vessel ready");
    let provider = flotilla_core::providers::environment::docker::DockerEnvironmentProvider::new(Arc::new(MemoryIncidentRunner::default()));
    let handles = provider.list().await.expect("list backing");
    let state = Arc::new(ControllerRuntimeState::new(
        daemon.clone(),
        config,
        adoption_registry(handles),
        None,
        daemon.local_host_id().expect("local host").to_string(),
        None,
        "host-direct-test".into(),
    ));
    let sample = EnvironmentRuntimeObservation {
        memory_limits: Some(EnvironmentMemoryLimits { memory_bytes: 8589934592, swap_bytes: 0 }),
        memory_usage_bytes: Some(123456789),
        memory_observed_at: Some("2026-10-05T10:13:00Z".into()),
        termination: None,
        ..Default::default()
    };
    // Duplicate samples must be harmless; a dead cgroup cannot erase them.
    for _ in 0..2 {
        record_environment_observation(&state, NAMESPACE, &env_id, &sample).await.expect("sample");
    }
    let environments = backend.using::<Environment>(NAMESPACE);
    let before_environment = environments.get(env_id.as_str()).await.expect("environment");
    let before_vessel = vessels.get("memory-incident-work").await.expect("vessel");
    let mut duplicate = sample.clone();
    duplicate.memory_observed_at = Some("2026-10-05T10:13:30Z".into());
    record_environment_observation(&state, NAMESPACE, &env_id, &duplicate).await.expect("unchanged sample");
    assert_eq!(
        environments.get(env_id.as_str()).await.expect("environment").metadata.resource_version,
        before_environment.metadata.resource_version
    );
    assert_eq!(
        vessels.get("memory-incident-work").await.expect("vessel").metadata.resource_version,
        before_vessel.metadata.resource_version
    );
    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("observe death");
    let environments = backend.using::<Environment>(NAMESPACE);
    let environment = environments.get(env_id.as_str()).await.expect("environment");
    let status = environment.status.expect("status");
    assert_eq!(status.phase, EnvironmentPhase::Lost);
    let message = status.message.expect("failure message");
    assert!(message.contains("host oomd") && message.contains("exit code 137") && message.contains("/hulls/work/flotilla"), "{message}");
    let observation = status.runtime_observation.expect("persisted evidence");
    assert_eq!(observation.memory_usage_bytes, sample.memory_usage_bytes);
    assert_eq!(observation.memory_observed_at, sample.memory_observed_at);
    assert_eq!(observation.termination.as_ref().expect("termination").cause, EnvironmentExitCause::HostOomd);
    assert_eq!(
        vessels.get("memory-incident-work").await.expect("vessel").status.expect("status").runtime_observation,
        Some(observation.clone())
    );
    let vessel_reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let vessel = vessels.get("memory-incident-work").await.expect("vessel");
    let prepared = vessel_reconciler.prepare(&vessel).await.expect("prepare vessel");
    let outcome = vessel_reconciler.reconcile(&vessel, &prepared, chrono::Utc::now());
    flotilla_resources::apply_status_patch(&vessels, &vessel.metadata.name, &outcome.patch.expect("mark vessel lost"))
        .await
        .expect("persist vessel loss");
    let lost_status = vessels.get(&vessel.metadata.name).await.expect("vessel").status.expect("status");
    assert_eq!(lost_status.phase, flotilla_resources::VesselPhase::Lost);
    assert_eq!(lost_status.message.as_deref(), Some(message.as_str()));
    let lost = vessels.get(&vessel.metadata.name).await.expect("lost vessel");
    let prepared = vessel_reconciler.prepare(&lost).await.expect("lost vessel prepare");
    let frozen = vessel_reconciler.reconcile(&lost, &prepared, chrono::Utc::now());
    assert!(frozen.patch.is_none() && frozen.actuations.is_empty(), "lost backing must not reprovision");
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy_reconciler = ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(NAMESPACE)).with_vessels(vessels.clone());
    let convoy = convoys.get("memory-incident").await.expect("convoy");
    let prepared = convoy_reconciler.prepare(&convoy).await.expect("prepare convoy");
    let outcome = convoy_reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
    flotilla_resources::apply_status_patch(&convoys, "memory-incident", &outcome.patch.expect("copy runtime evidence"))
        .await
        .expect("persist convoy evidence");
    assert_eq!(
        backend.using::<Convoy>(NAMESPACE).get("memory-incident").await.expect("convoy").status.expect("status").environment_observations
            ["work"],
        observation
    );
    // Following reconciliations interrupt work and crew; copying diagnostic
    // evidence must not swallow the recoverable phase transition.
    for _ in 0..2 {
        let convoy = convoys.get("memory-incident").await.expect("convoy");
        let prepared = convoy_reconciler.prepare(&convoy).await.expect("prepare convoy");
        let outcome = convoy_reconciler.reconcile(&convoy, &prepared, chrono::Utc::now());
        flotilla_resources::apply_status_patch(&convoys, "memory-incident", &outcome.patch.expect("roll up loss"))
            .await
            .expect("persist convoy interruption");
    }
    let interrupted = convoys.get("memory-incident").await.expect("convoy").status.expect("status");
    assert_eq!(interrupted.phase, ConvoyPhase::Interrupted);
    assert_eq!(interrupted.work["work"].message.as_deref(), Some(message.as_str()));
    assert_eq!(interrupted.crew_work["work"]["shell"].phase, flotilla_resources::CrewWorkPhase::Interrupted);
    assert!(interrupted.crew_work["work"]["shell"].message.as_ref().expect("loss reason").contains("lost, recoverable"));
    environments.delete(env_id.as_str()).await.expect("remove backing resource");
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.into()), name: "memory-incident".into() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain");
    let CommandValue::ConvoyExplanation(explanation) = result else {
        panic!("expected explanation, got {result:?}");
    };
    assert_eq!(explanation.environment_observations["work"], observation);
    assert!(checkouts.get("recoverable-work").await.is_ok(), "failure must retain work");
}

// Terminal liveness must still mark backing lost when richer inspection is unavailable;
// running backing remains Ready for retry. The runner is the subprocess boundary.
#[tokio::test]
async fn observation_error_preserves_terminal_liveness_and_retries_running_backing() {
    for terminal in [true, false] {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"observation-error-test\"\n").expect("machine identity");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let daemon = in_memory_daemon(Vec::new(), config.clone()).await;
        let env_id = EnvironmentId::new("env-memory-incident");
        create_ready_docker_environment(&daemon, env_id.as_str(), "container", BTreeSet::new()).await;
        let provider = flotilla_core::providers::environment::docker::DockerEnvironmentProvider::new(Arc::new(MemoryIncidentRunner {
            status: if terminal { "exited" } else { "running" },
            inspect_error: true,
        }));
        let handles = provider.list().await.expect("list backing");
        let state = Arc::new(ControllerRuntimeState::new(
            daemon.clone(),
            config,
            adoption_registry(handles),
            None,
            daemon.local_host_id().expect("local host").to_string(),
            None,
            "host-direct-test".into(),
        ));
        let result = reconcile_provisioned_environments(&state, NAMESPACE).await;
        let status = daemon
            .resource_backend()
            .using::<Environment>(NAMESPACE)
            .get(env_id.as_str())
            .await
            .expect("environment")
            .status
            .expect("status");
        if terminal {
            result.expect("terminal backing is lost even without inspect");
            assert_eq!(status.phase, EnvironmentPhase::Lost);
            assert!(status.message.expect("message").contains("Stopped"));
        } else {
            assert!(result.expect_err("running inspection failure retries").contains("will retry"));
            assert_eq!(status.phase, EnvironmentPhase::Ready);
        }
        assert!(status.runtime_observation.is_none(), "unavailable evidence stays unknown");
    }
}
