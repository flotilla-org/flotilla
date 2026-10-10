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
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_composition: None,
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
        [(None, false, true), (Some("docker"), true, true), (Some("absent"), false, false), (None, true, false)]
    {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"environment-readoption-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let backend = ResourceBackend::InMemory(Default::default());
        let first = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::clone(&config),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            flotilla_protocol::HostName::new("udder"),
            backend.clone(),
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
                .insert(flotilla_core::providers::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL.into(), instance.into());
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
            assert!(result.expect_err("missing or ambiguous instance must not adopt").contains("unavailable or ambiguous"));
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
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_composition: None,
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
