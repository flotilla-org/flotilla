use super::*;
use crate::testkits::engine::{admit_convoy, engine, runtime_checkouts};
use flotilla_provisioning::engine::controller::{Backing, BackingEvent};

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
                Ok(include_str!("../../../../flotilla-environment/src/environment/fixtures/oomd-exit137.json")
                    .replace("exited", self.status))
            }
            "journalctl" if args.contains(&"--unit=systemd-oomd") => {
                Ok(include_str!("../../../../flotilla-environment/src/environment/fixtures/oomd-journal.txt").into())
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
    // The work's repository, whose retained checkout a loss names.
    let repo = RepositoryKey("test-repository".into());
    let status = ConvoyStatus {
        observed_workflow_ref: Some("scratch".into()),
        workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder()
                .name("work".into())
                .repository_refs(vec![repo.clone()])
                .crew(vec![CrewSpec::builder()
                    .role("shell".into())
                    .promises(vec![flotilla_resources::TemplatePromise::decision_ledger("shell")])
                    .source(CrewSource::Tool { command: "true".into() })
                    .build()])
                .build()],
        }),
        ..Default::default()
    };
    let spec = ConvoySpec::builder().workflow_ref("scratch".into()).build();
    let convoy = admit_convoy(&daemon, flotilla_engine_convoys_testkit::convoy(NAMESPACE, "memory-incident", spec, status)).await;
    // The work's adopted checkout, ready on the engine and held for its convoy.
    let checkouts = runtime_checkouts(&daemon);
    checkouts.put_ready(
        "recoverable-work",
        &ResourceCheckoutSpec::Observed(
            ResourceObservedCheckoutSpec::builder()
                .r#ref("fix/retained-work".into())
                .path("/hulls/work/flotilla".into())
                .repo_ref(repo.clone())
                .host_ref("host".into())
                .is_main(false)
                .build(),
        ),
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy.clone())),
    );
    // The work's launch, ready in its environment; the planner reads it as the old store's view.
    let engine = engine(&daemon);
    let launch = engine.vessels.request(&convoy, "work", daemon.local_host_id().expect("local host").as_str());
    // Its container, requested for it and running.
    let host_ref = daemon.local_host_id().expect("local host").to_string();
    engine_environments(&daemon).put_ready(
        env_id.as_str(),
        &docker_environment_spec(&host_ref, BTreeSet::new()),
        Some(&launch),
        "container",
        flotilla_resources::environment_records::LaunchFacts {
            image_ref: Some("contained-image".to_string()),
            local_image_id: Some("sha256:test-interior".to_string()),
            ..Default::default()
        },
    );
    engine.vessels.ready(&launch, env_id.as_str());
    let provider = flotilla_environment::environment::docker::DockerEnvironmentProvider::new(Arc::new(MemoryIncidentRunner::default()));
    let handles = provider.list().await.expect("list backing");
    engine_environments(&daemon);
    let state = Arc::new(ControllerRuntimeState::new(
        daemon.clone(),
        config,
        adoption_registry(handles),
        None,
        daemon.local_host_id().expect("local host").to_string(),
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
        record_environment_observation(&state, &env_id, &sample).await.expect("sample");
    }
    // The observation is held in this home's memory; a later sample of the same usage keeps the
    // time it was first seen.
    let held = || daemon.lifecycle_reads().environments().expect("environments").observations().get(env_id.as_str());
    let mut duplicate = sample.clone();
    duplicate.memory_observed_at = Some("2026-10-05T10:13:30Z".into());
    record_environment_observation(&state, &env_id, &duplicate).await.expect("unchanged sample");
    assert_eq!(held().expect("held sample").memory_observed_at, sample.memory_observed_at);
    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("observe death");
    // The probe tells the Environment controller the backing is gone, and why; the controller
    // ends the launch Lost with that reason, as the fixture steps it here.
    let observed = observed_backings(&state);
    let [BackingEvent { name, observed: Backing::Absent { reason: Some(reason) } }] = observed.as_slice() else {
        panic!("the dead backing is reported absent with its cause: {observed:?}");
    };
    assert_eq!(name, env_id.as_str());
    engine_environments(&daemon).lose(env_id.as_str(), reason);
    let observation = held().expect("held evidence");
    assert_eq!(observation.memory_usage_bytes, sample.memory_usage_bytes);
    assert_eq!(observation.memory_observed_at, sample.memory_observed_at);
    assert_eq!(observation.termination.as_ref().expect("termination").cause, EnvironmentExitCause::HostOomd);
    // The Vessel controller, the launch's single writer, adopts it and ends it Lost with the
    // backing's cause and where the work is.
    let _running = flotilla_orchestration_testkit::run_vessels(
        &backend,
        NAMESPACE,
        VesselPlanning::new(backend.clone(), NAMESPACE, daemon.account_records()).with_lifecycle_reads(daemon.lifecycle_reads()),
        Arc::clone(&engine.vessels.lifecycles),
        daemon.lifecycle_reads(),
        daemon.decided_store().expect("decided store"),
    );
    let reading = || engine.vessels.vessels.get(&launch, None).expect("read").expect("launch").reading;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(reading(), LifecycleReading::Lost { .. }) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the launch ends lost: {:?}", reading()));
    let LifecycleReading::Lost { reason: Some(message) } = reading() else {
        panic!("the launch ends lost with its cause: {:?}", reading());
    };
    assert!(message.contains("host oomd") && message.contains("exit code 137") && message.contains("/hulls/work/flotilla"), "{message}");
    // The convoy's work and crew read interrupted from the lost launch.
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let interrupted = convoys.get(&convoy).await.expect("convoy").status.expect("status");
    assert_eq!(interrupted.phase, ConvoyPhase::Interrupted);
    assert_eq!(interrupted.work["work"].message.as_deref(), Some(message.as_str()));
    assert_eq!(interrupted.crew_work["work"]["shell"].phase, flotilla_resources::CrewWorkPhase::Interrupted);
    // The launch has already ended Lost, so nothing is left to release. The evidence is this
    // home's, read for the work's launch.
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.into()), name: convoy.clone() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain");
    let CommandValue::ConvoyExplanation(explanation) = result else {
        panic!("expected explanation, got {result:?}");
    };
    assert_eq!(explanation.environment_observations["work"], observation);
    assert!(!checkouts.record("recoverable-work").ended(), "failure must retain work");
}

// A failed observation has no separate liveness authority: Unknown preserves
// the ready backing for retry, regardless of the engine's inaccessible state.
#[tokio::test]
async fn observation_error_preserves_backing_and_retries() {
    for terminal in [true, false] {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"observation-error-test\"\n").expect("machine identity");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let daemon = in_memory_daemon(Vec::new(), config.clone()).await;
        let env_id = EnvironmentId::new("env-memory-incident");
        create_ready_docker_environment(&daemon, env_id.as_str(), "container", BTreeSet::new()).await;
        let provider = flotilla_environment::environment::docker::DockerEnvironmentProvider::new(Arc::new(MemoryIncidentRunner {
            status: if terminal { "exited" } else { "running" },
            inspect_error: true,
        }));
        let handles = provider.list().await.expect("list backing");
        engine_environments(&daemon);
        let state = Arc::new(ControllerRuntimeState::new(
            daemon.clone(),
            config,
            adoption_registry(handles),
            None,
            daemon.local_host_id().expect("local host").to_string(),
            "host-direct-test".into(),
        ));
        let result = reconcile_provisioned_environments(&state, NAMESPACE).await;
        assert!(result.expect_err("unknown inspection failure retries").contains("will retry"));
        assert_eq!(observed_backings(&state), [], "an unknown inspection tells the Environment controller nothing");
        let environment = engine_environments(&daemon).environments.find(env_id.as_str()).expect("read").expect("environment");
        assert!(environment.is_ready());
        // An unknown inspection observes nothing.
        let reads = daemon.lifecycle_reads();
        assert_eq!(reads.environments().expect("environments").observations().get(env_id.as_str()), None);
    }
}
