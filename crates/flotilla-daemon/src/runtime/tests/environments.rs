use super::*;

#[tokio::test]
async fn standing_backing_inspection_accepts_durable_pre_provisioning_failure() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"standing-pre-provision-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("dinghy"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("quartermaster"), &ConvoySpec::builder().workflow_ref("standing".to_string()).build())
        .await
        .expect("convoy");
    let convoy = convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &flotilla_resources::ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Failed,
                provisioning: Some(ConvoyProvisioningState::NotStarted),
                message: Some("workflow validation failed".to_string()),
                finished_at: Some(Utc::now()),
                ..Default::default()
            },
        )
        .await
        .expect("failed convoy status");
    let state = ControllerRuntimeState::new(
        daemon,
        config,
        Arc::new(ProviderRegistry::new()),
        Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    );

    state.verify_backing_dead(&convoy).await.expect("provisioning never started, so no backing can be live");
}

#[tokio::test]
async fn standing_backing_inspection_accepts_local_absence_only_after_startup_observation() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"standing-absent-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("dinghy"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("quartermaster"), &ConvoySpec::builder().workflow_ref("standing".to_string()).build())
        .await
        .expect("convoy");
    let convoy = convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &flotilla_resources::ConvoyStatus {
                provisioning: Some(ConvoyProvisioningState::Started { started_at: Utc::now() }),
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()],
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                }),
                placement_decision: Some(
                    PlacementDecision::builder()
                        .policy_name("docker".to_string())
                        .target_host(PlacementTargetHost {
                            reference: CanonicalHostId::resolved("host-test"),
                            display_name: "host-test".to_string(),
                        })
                        .build(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status");
    let mut registry = ProviderRegistry::new();
    let sibling: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("env-quartermaster-extra-work"),
        image: ImageId::new("contained-image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::new(AtomicBool::new(false)),
    });
    registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(AdoptionEnvironmentProvider { handles: vec![sibling] }),
    );
    let state = Arc::new(ControllerRuntimeState::new(
        daemon,
        config,
        Arc::new(registry),
        Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    ));

    assert!(state.verify_backing_dead(&convoy).await.is_err(), "startup absence alone is not evidence");
    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("complete local observation pass");
    state.verify_backing_dead(&convoy).await.expect("observed absence on the home host proves backing dead");

    let mut remote = convoy;
    remote.status.as_mut().expect("status").placement_decision.as_mut().expect("placement").target_host.reference =
        CanonicalHostId::resolved("remote-host");
    assert!(state.verify_backing_dead(&remote).await.is_err(), "the local pass cannot prove absence on a remote host");
}

#[tokio::test]
async fn standing_backing_inspection_trusts_live_docker_over_failed_resource_phase() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"standing-backing-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("dinghy"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let convoy = daemon
        .resource_backend()
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("quartermaster"), &ConvoySpec::builder().workflow_ref("standing".to_string()).build())
        .await
        .expect("convoy");
    let environments = daemon.resource_backend().using::<Environment>(NAMESPACE);
    let environment = environments
        .create(
            &empty_meta_with_labels("quartermaster-work", BTreeMap::from([(CONVOY_LABEL.to_string(), "quartermaster".to_string())])),
            &flotilla_resources::EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "host-test".to_string(),
                    image: "contained-image".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("environment");
    environments
        .update_status(
            &environment.metadata.name,
            &environment.metadata.resource_version,
            &flotilla_resources::EnvironmentStatus {
                phase: EnvironmentPhase::Failed,
                ready: false,
                docker_container_id: Some("test-interior".to_string()),
                message: Some("provider registry unavailable".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("failed daemon-side environment phase");
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("quartermaster-work"),
        image: ImageId::new("contained-image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::new(AtomicBool::new(false)),
    });
    let mut registry = ProviderRegistry::new();
    registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(AdoptionEnvironmentProvider { handles: vec![handle] }),
    );
    let state = ControllerRuntimeState::new(
        daemon,
        config,
        Arc::new(registry),
        Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    );

    let refusal = state.verify_backing_dead(&convoy).await.expect_err("live Docker backing must hold standing teardown");

    assert!(refusal.contains("backing is live") && refusal.contains("test-interior"), "unexpected refusal: {refusal}");

    let environment = environments.get("quartermaster-work").await.expect("environment");
    environments
        .update_status(
            &environment.metadata.name,
            &environment.metadata.resource_version,
            &flotilla_resources::EnvironmentStatus { phase: EnvironmentPhase::Failed, ..Default::default() },
        )
        .await
        .expect("remove container identity");
    let refusal = state.verify_backing_dead(&convoy).await.expect_err("missing container identity must hold standing teardown");
    assert!(refusal.contains("no Docker container identity"), "unexpected refusal: {refusal}");
}

#[tokio::test]
async fn fresh_daemon_readopts_running_environment_from_shared_store_and_backing() {
    for (instance, extra_provider, succeeds) in
        [(None, false, true), (Some("docker"), true, true), (Some("absent"), false, false), (None, true, true)]
    {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"environment-readoption-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let first = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::clone(&config),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            flotilla_protocol::HostName::new("udder"),
            backend.clone(),
            std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
        )
        .await;
        let env_id = EnvironmentId::new("env-governor-govern");
        let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
            id: env_id.clone(),
            image: ImageId::new("contained-image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
            destroyed: Arc::new(AtomicBool::new(false)),
        });
        create_ready_docker_environment(&first, env_id.as_str(), "test-interior", BTreeSet::new()).await;
        if let Some(instance) = instance {
            let api = first.resource_backend().using::<Environment>(NAMESPACE);
            let mut environment = api.get(env_id.as_str()).await.expect("record");
            environment
                .metadata
                .labels
                .insert(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL.into(), instance.into());
            environment.spec.provider_address = Some(
                flotilla_address::Address::root(flotilla_address::Kind::Host, environment.spec.host_ref().expect("host"))
                    .and_then(|place| place.child(flotilla_address::Kind::Provider, instance))
                    .expect("address"),
            );
            api.update(&InputMeta::from(&environment.metadata), &environment.metadata.resource_version, &environment.spec)
                .await
                .expect("bind instance");
        }
        first
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), EnvironmentBag::new(), Some(passthrough_registry()))
            .expect("initial provision registration");
        drop(first);

        let restarted = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::clone(&config),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            flotilla_protocol::HostName::new("udder"),
            backend,
            std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
        )
        .await;
        assert!(restarted.environment_registry_for_environment(&env_id).is_none(), "fresh daemon starts without ephemeral registration");
        let mut registry = adoption_registry(vec![handle]);
        if extra_provider {
            Arc::get_mut(&mut registry).expect("unshared registry").environment_providers.insert(
                "other",
                ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "other"),
                Arc::new(AdoptionEnvironmentProvider { handles: Vec::new() }),
            );
        }
        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&restarted),
            config,
            registry,
            None,
            restarted.local_host_id().expect("local host identity").to_string(),
            "host-direct-test".to_string(),
        ));

        let result = reconcile_provisioned_environments(&state, NAMESPACE).await;
        if !succeeds {
            assert!(result.expect_err("missing or ambiguous instance must not adopt").contains("unavailable"));
            assert!(restarted.environment_registry_for_environment(&env_id).is_none());
            continue;
        }
        result.expect("readopt running environment");

        assert!(restarted.environment_registry_for_environment(&env_id).is_some(), "interior provider registry should be restored");
        assert!(
            restarted.command_runner_for_environment_ref(env_id.as_str()).is_some(),
            "environment runner should be restored for attach resolution"
        );
        assert!(state.provisioned_environments.lock().await.contains_key("test-interior"));
    }
}

#[tokio::test]
async fn environment_readoption_marks_missing_container_lost() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"dead-environment-readoption-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let env_id = EnvironmentId::new("env-dead");
    create_ready_docker_environment(&daemon, env_id.as_str(), "missing-container", BTreeSet::new()).await;
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(Vec::new()),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("reconcile missing backing");

    let environment = daemon.resource_backend().using::<Environment>(NAMESPACE).get(env_id.as_str()).await.expect("environment record");
    let status = environment.status.expect("environment status");
    assert_eq!(status.phase, EnvironmentPhase::Lost);
    assert_eq!(
        status.message.as_deref(),
        Some("lost, recoverable: Docker container missing-container is not running; rehydration is not available yet (#2872)")
    );
    assert!(daemon.environment_registry_for_environment(&env_id).is_none());
}

#[tokio::test]
async fn transient_environment_liveness_error_remains_ready_for_retry() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"transient-environment-readoption-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let env_id = EnvironmentId::new("env-transient");
    create_ready_docker_environment(&daemon, env_id.as_str(), "uncertain-container", BTreeSet::new()).await;
    let handle: EnvironmentHandle =
        Arc::new(TestUncertainEnvironment { id: env_id.clone(), status_error: "Docker daemon temporarily unavailable".to_string() });
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(vec![handle]),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    let error = reconcile_provisioned_environments(&state, NAMESPACE).await.expect_err("uncertain liveness should request a retry");

    assert!(error.contains("temporarily unavailable") && error.contains("will retry"));
    let environment = daemon.resource_backend().using::<Environment>(NAMESPACE).get(env_id.as_str()).await.expect("environment record");
    assert_eq!(environment.status.expect("environment status").phase, EnvironmentPhase::Ready);
    assert!(daemon.environment_registry_for_environment(&env_id).is_none());
}

#[tokio::test]
async fn failed_environment_readoption_does_not_block_a_live_sibling() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"isolated-environment-readoption-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let bad_id = EnvironmentId::new("env-bad");
    let good_id = EnvironmentId::new("env-good");
    let orphaned_id = EnvironmentId::new("env-orphaned");
    create_ready_docker_environment(&daemon, bad_id.as_str(), "test-interior", BTreeSet::from(["missing-adapter".to_string()])).await;
    create_ready_docker_environment(&daemon, good_id.as_str(), "test-interior", BTreeSet::new()).await;
    let environments = daemon.resource_backend().using::<Environment>(NAMESPACE);
    environments
        .create(
            &empty_meta(orphaned_id.as_str()),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "deleted-host".to_string(),
                    image: "contained-image".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("create orphaned environment");
    flotilla_store::apply_status_patch(
        &environments,
        orphaned_id.as_str(),
        &EnvironmentStatusPatch::MarkReady {
            configured_limits: None,
            docker_container_id: Some("orphaned-container".to_string()),
            image_ref: Some("contained-image".to_string()),
            local_image_id: Some("sha256:contained".to_string()),
            registry_digest: None,
        },
    )
    .await
    .expect("mark orphaned environment ready");
    let handle = |id: EnvironmentId| {
        Arc::new(TestInteriorEnvironment {
            id,
            image: ImageId::new("contained-image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            destroyed: Arc::new(AtomicBool::new(false)),
        }) as EnvironmentHandle
    };
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(vec![handle(bad_id.clone()), handle(good_id.clone())]),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("fail one environment without aborting its sibling");

    let bad = daemon.resource_backend().using::<Environment>(NAMESPACE).get(bad_id.as_str()).await.expect("bad environment");
    assert_eq!(bad.status.expect("bad environment status").phase, EnvironmentPhase::Failed);
    assert!(daemon.environment_registry_for_environment(&bad_id).is_none());
    assert!(daemon.environment_registry_for_environment(&orphaned_id).is_none());
    assert!(daemon.environment_registry_for_environment(&good_id).is_some(), "live sibling should still be adopted");
}

// #3765 S1/S6: cloud Unknown must not abort a sibling's loss detection.
// The in-memory provider represents the remote API boundary; durable status
// transitions run through the real runtime and resource backend.
#[tokio::test]
async fn cloud_unknown_does_not_abort_gone_sibling() {
    use flotilla_environment::environment::{GoneCause, ObservationState, ProviderObservation, UnknownReason};
    struct CloudHandle {
        id: EnvironmentId,
        state: ObservationState,
    }
    #[async_trait]
    impl ProvisionedEnvironment for CloudHandle {
        flotilla_discovery_testkit::injected_environment_handle!();
        fn id(&self) -> &EnvironmentId {
            &self.id
        }
        fn image(&self) -> &ImageId {
            static IMAGE: std::sync::LazyLock<ImageId> = std::sync::LazyLock::new(|| ImageId::new("cloud"));
            &IMAGE
        }
        async fn detect_readiness(&self) -> flotilla_environment::environment::ProviderReadiness {
            flotilla_environment::environment::ProviderReadiness::Ready
        }
        fn health_message(&self) -> Option<String> {
            Some("degraded: injected failure".into())
        }
        async fn observe(&self) -> ProviderObservation {
            ProviderObservation::new(self.state.clone())
        }
        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(Default::default())
        }
        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::new(DiscoveryMockRunner::builder().build())
        }
        async fn destroy(&self) -> Result<(), String> {
            panic!("liveness must never destroy backing")
        }
    }
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("config")).unwrap();
    fs::write(temp.path().join("config/daemon.toml"), r#"machine_id = "cloud-liveness-test""#).unwrap();
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        config.clone(),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("controller"),
        Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let address: flotilla_address::Address = "account:cloud/provider:cloud".parse().unwrap();
    let handles: Vec<EnvironmentHandle> = vec![
        Arc::new(CloudHandle {
            id: EnvironmentId::new("a-unknown"),
            state: ObservationState::Unknown { reason: UnknownReason::GuestUnresponsive },
        }),
        Arc::new(CloudHandle { id: EnvironmentId::new("b-gone"), state: ObservationState::Gone { cause: GoneCause::LifetimeCapReached } }),
        Arc::new(CloudHandle { id: EnvironmentId::new("c-stale"), state: ObservationState::Running }),
    ];
    let mut state = ControllerRuntimeState::new(
        daemon.clone(),
        config,
        Arc::new(ProviderRegistry::new()),
        None,
        daemon.local_host_id().unwrap().to_string(),
        "direct".into(),
    );
    state.environment_providers.insert(address.clone(), Arc::new(AdoptionEnvironmentProvider { handles: handles.clone() })).unwrap();
    let state = Arc::new(state);
    let environments = daemon.resource_backend().using::<Environment>(NAMESPACE);
    for handle in &handles {
        let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(serde_json::json!({
            "provider_address": address.to_string(), "cloud_sandbox": {
                "image": "cloud", "size": {"cpus":1,"memory_bytes":1024,"disk_bytes":1024}, "env": {} }
        }))
        .unwrap();
        let object = environments.create(&empty_meta(handle.id().as_str()), &spec).await.unwrap();
        environments
            .update_status(
                &object.metadata.name,
                &object.metadata.resource_version,
                &flotilla_resources::EnvironmentStatus {
                    phase: EnvironmentPhase::Ready,
                    ready: true,
                    backing_id: Some(handle.backing_id().into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        state
            .provisioned_environments
            .lock()
            .await
            .insert(handle.id().to_string(), ActiveProvisionedEnvironment { handle: handle.clone() });
    }
    assert!(reconcile_provisioned_environments(&state, NAMESPACE).await.unwrap_err().contains("unknown"));
    assert_eq!(environments.get("a-unknown").await.unwrap().status.unwrap().phase, EnvironmentPhase::Ready);
    assert_eq!(environments.get("b-gone").await.unwrap().status.unwrap().phase, EnvironmentPhase::Lost);
    let stale = environments.get("c-stale").await.unwrap().status.unwrap();
    assert!(stale.ready);
    assert_eq!(stale.phase, EnvironmentPhase::Ready);
    assert!(stale.message.unwrap().contains("degraded"));
}

// #3780: bound active cloud work, admit queued siblings in order as slots free,
// and cancel timed-out jobs without letting Unknown/slow jobs abort siblings.
// The jobs stand in for provider/SSH IO; the actual production scheduler runs.
#[tokio::test(start_paused = true)]
async fn cloud_scheduler_bounds_refills_and_cancels_without_starving_siblings() {
    use crate::runtime::environments::{start_cloud_reconciliations, CLOUD_RECONCILIATION_LIMIT, ENVIRONMENT_ADOPTION_TIMEOUT};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    struct Active {
        count: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        completed: bool,
    }
    impl Drop for Active {
        fn drop(&mut self) {
            self.count.fetch_sub(1, Ordering::SeqCst);
            if !self.completed {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    async fn next_start(starts: &mut tokio::sync::mpsc::UnboundedReceiver<usize>) -> usize {
        tokio::time::timeout(Duration::from_secs(1), starts.recv()).await.unwrap().unwrap()
    }
    let healthy_and_gone = Arc::new(Mutex::new(Vec::new()));
    let release_unknown = Arc::new(tokio::sync::Notify::new());
    let jobs = (0..CLOUD_RECONCILIATION_LIMIT + 3)
        .map(|id| {
            let (count, peak, cancelled, started, healthy_and_gone, release_unknown) =
                (count.clone(), peak.clone(), cancelled.clone(), started.clone(), healthy_and_gone.clone(), release_unknown.clone());
            (id.to_string(), async move {
                let mut guard = Active { count: count.clone(), cancelled, completed: false };
                peak.fetch_max(count.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                started.send(id).unwrap();
                if id == CLOUD_RECONCILIATION_LIMIT - 1 {
                    release_unknown.notified().await;
                    guard.completed = true;
                    Err("Unknown: provider unresponsive".into())
                } else if id == CLOUD_RECONCILIATION_LIMIT || id == CLOUD_RECONCILIATION_LIMIT + 1 {
                    healthy_and_gone.lock().unwrap().push(id);
                    guard.completed = true;
                    Ok(())
                } else {
                    std::future::pending().await
                }
            })
        })
        .collect();
    drop(started);
    let mut batch = start_cloud_reconciliations(jobs);
    for id in 0..CLOUD_RECONCILIATION_LIMIT {
        assert_eq!(next_start(&mut starts).await, id);
    }
    // Let runnable jobs settle before asserting that queued work stays queued.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(starts.try_recv().is_err());
    assert_eq!(count.load(Ordering::SeqCst), CLOUD_RECONCILIATION_LIMIT);
    // One Unknown completion must refill repeatedly: healthy and Gone jobs
    // queued behind blocked peers finish before any timeout or peer release.
    release_unknown.notify_one();
    for id in CLOUD_RECONCILIATION_LIMIT..CLOUD_RECONCILIATION_LIMIT + 3 {
        assert_eq!(next_start(&mut starts).await, id);
    }
    let last_admitted = tokio::time::Instant::now();
    assert_eq!(*healthy_and_gone.lock().unwrap(), vec![CLOUD_RECONCILIATION_LIMIT, CLOUD_RECONCILIATION_LIMIT + 1]);
    assert_eq!(peak.load(Ordering::SeqCst), CLOUD_RECONCILIATION_LIMIT);
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
    tokio::time::advance(ENVIRONMENT_ADOPTION_TIMEOUT).await;
    let errors = batch.join_next().await.unwrap().unwrap().unwrap_err();
    assert!(errors.contains("Unknown: provider unresponsive"));
    assert_eq!(errors.matches("timed out").count(), CLOUD_RECONCILIATION_LIMIT);
    assert_eq!(cancelled.load(Ordering::SeqCst), CLOUD_RECONCILIATION_LIMIT);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(last_admitted.elapsed(), ENVIRONMENT_ADOPTION_TIMEOUT);
    assert!(batch.join_next().await.is_none());
}
