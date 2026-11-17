use super::*;
use crate::environment_tools::EnvironmentToolProvisioner;
use crate::runtime::environments::{mark_unavailable_environment, BackingDisposition};
use crate::testkits::engine::{admit_convoy, engine, runtime_checkouts};
use flotilla_provisioning::engine::controller::{Backing, BackingEvent};

/// Admits standing convoy `name` with `status` as its next launch on `daemon`'s engine, and
/// reads the launch back.
async fn launch_standing(
    daemon: &InProcessDaemon,
    name: &str,
    status: flotilla_resources::ConvoyStatus,
) -> flotilla_resources::ResourceObject<Convoy> {
    engine(daemon);
    let spec = ConvoySpec::builder().workflow_ref("standing".to_string()).build();
    let launch = admit_convoy(daemon, flotilla_engine_convoys_testkit::convoy(NAMESPACE, name, spec, status)).await;
    daemon.resource_backend().using::<Convoy>(NAMESPACE).get(&launch).await.expect("launched convoy")
}

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
    let convoy = launch_standing(
        &daemon,
        "quartermaster",
        flotilla_resources::ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Failed,
            provisioning: Some(ConvoyProvisioningState::NotStarted),
            message: Some("workflow validation failed".to_string()),
            finished_at: Some(Utc::now()),
            ..flotilla_engine_convoys_testkit::single_agent("coder")
        },
    )
    .await;
    engine_environments(&daemon);
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
    let convoy = launch_standing(
        &daemon,
        "quartermaster",
        flotilla_resources::ConvoyStatus {
            provisioning: Some(ConvoyProvisioningState::Started { started_at: Utc::now() }),
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                cascade: None,
                vessels: vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![flotilla_engine_convoys_testkit::agent("coder")])
                    .build()],
                exit: None,
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
            }),
            placement_decision: Some(
                PlacementDecision::builder()
                    .kind("docker".to_string())
                    .target_host(PlacementTargetHost {
                        reference: CanonicalHostId::resolved("host-test"),
                        display_name: "host-test".to_string(),
                    })
                    .build(),
            ),
            ..Default::default()
        },
    )
    .await;
    // A launch that holds no environment: provisioning began and its backing is unaccounted for.
    engine(&daemon).vessels.request(&convoy.metadata.name, "work", "host-test");
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
    engine_environments(&daemon);
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
    // A standing convoy's environments are those its vessels hold.
    let mut standing = Vec::new();
    for name in ["quartermaster", "purser"] {
        let convoy = launch_standing(
            &daemon,
            name,
            flotilla_resources::ConvoyStatus {
                provisioning: Some(ConvoyProvisioningState::Started { started_at: Utc::now() }),
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .crew(vec![flotilla_engine_convoys_testkit::agent("coder")])
                        .build()],
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                }),
                ..Default::default()
            },
        )
        .await;
        standing.push(convoy);
    }
    let [convoy, unrealised]: [_; 2] = standing.try_into().expect("two convoys");
    let environments = engine_environments(&daemon);
    let spec = docker_environment_spec("host-test", BTreeSet::new());
    let vessels = &engine(&daemon).vessels;
    // A launch realised under its container, after a failed attempt on the daemon's side.
    let quartermaster = vessels.request(&convoy.metadata.name, "work", "host-test");
    environments.request("quartermaster-work", &spec, Some(&quartermaster));
    environments.retry("quartermaster-work", "provider registry unavailable");
    environments.ready("quartermaster-work", "test-interior", Default::default());
    // A launch whose only attempt failed before it was realised has no container identity.
    let purser = vessels.request(&unrealised.metadata.name, "work", "host-test");
    environments.request("purser-work", &spec, Some(&purser));
    environments.retry("purser-work", "provider registry unavailable");
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
    engine_environments(&daemon);
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

    let refusal = state.verify_backing_dead(&unrealised).await.expect_err("missing container identity must hold standing teardown");
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
        let backend = flotilla_store_memory::engine_home_backend();
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
        // The environment is requested bound to `instance` where one is named.
        let host_ref = first.local_host_id().expect("local host identity").to_string();
        let mut spec = docker_environment_spec(&host_ref, BTreeSet::new());
        spec.provider_address = instance.map(|instance| {
            flotilla_address::Address::root(flotilla_address::Kind::Host, &host_ref)
                .and_then(|place| place.child(flotilla_address::Kind::Provider, instance))
                .expect("address")
        });
        engine_environments(&first).put_ready(env_id.as_str(), &spec, None, "test-interior", Default::default());
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
        engine_environments(&restarted);
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
    engine_environments(&daemon);
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(Vec::new()),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("reconcile missing backing");

    // The probe tells the Environment controller the backing is absent, and why; the controller
    // ends the launch Lost with that reason.
    assert_eq!(
        observed_backings(&state),
        [BackingEvent {
            name: env_id.to_string(),
            observed: Backing::Absent { reason: Some("Docker container missing-container is not running".to_string()) },
        }]
    );
    assert!(daemon.environment_registry_for_environment(&env_id).is_none());
}

#[tokio::test]
async fn a_failed_environment_names_the_engine_checkouts_made_in_it() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"failed-environment-checkouts-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let env_id = EnvironmentId::new("env-failed");
    create_ready_docker_environment(&daemon, env_id.as_str(), "failed-container", BTreeSet::new()).await;
    // A checkout made in the environment, and one made elsewhere.
    let checkouts = runtime_checkouts(&daemon);
    let worktree = |env_ref: &str, path: &str| {
        CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: RepositoryKey("test-repository".into()),
            env_ref: env_ref.to_string(),
            r#ref: "fix/kept".to_string(),
            base_ref: None,
            target_path: path.to_string(),
            clone_ref: "clone-abc#1".to_string(),
        })
    };
    checkouts.put_ready("kept-work", &worktree(env_id.as_str(), "/hulls/kept"), None);
    checkouts.put_ready("elsewhere", &worktree("env-other", "/hulls/elsewhere"), None);
    engine_environments(&daemon);
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(Vec::new()),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    mark_unavailable_environment(&state, &env_id, BackingDisposition::Failed, "no container identity").await.expect("mark failed");

    let observed = observed_backings(&state);
    let [BackingEvent { observed: Backing::Failed { reason }, .. }] = observed.as_slice() else {
        panic!("the failed backing is reported with its reason: {observed:?}");
    };
    assert_eq!(reason, "no container identity; recover work from retained checkout(s): /hulls/kept");
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
    engine_environments(&daemon);
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
    assert_eq!(observed_backings(&state), [], "an uncertain probe tells the Environment controller nothing");
    let environment = engine_environments(&daemon).environments.find(env_id.as_str()).expect("read").expect("environment record");
    assert!(environment.is_ready());
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
    engine_environments(&daemon).put_ready(
        orphaned_id.as_str(),
        &docker_environment_spec("deleted-host", BTreeSet::new()),
        None,
        "orphaned-container",
        flotilla_resources::environment_records::LaunchFacts {
            image_ref: Some("contained-image".to_string()),
            local_image_id: Some("sha256:contained".to_string()),
            ..Default::default()
        },
    );
    let handle = |id: EnvironmentId| {
        Arc::new(TestInteriorEnvironment {
            id,
            image: ImageId::new("contained-image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            destroyed: Arc::new(AtomicBool::new(false)),
        }) as EnvironmentHandle
    };
    engine_environments(&daemon);
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        adoption_registry(vec![handle(bad_id.clone()), handle(good_id.clone())]),
        None,
        daemon.local_host_id().expect("local host identity").to_string(),
        "host-direct-test".to_string(),
    ));

    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("fail one environment without aborting its sibling");

    let observed = observed_backings(&state);
    assert!(
        matches!(observed.as_slice(), [BackingEvent { name, observed: Backing::Failed { reason } }]
            if name == bad_id.as_str() && reason.starts_with("environment adoption failed")),
        "the bad environment alone is reported failed to the Environment controller: {observed:?}"
    );
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
    engine_environments(&daemon);
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
    let environments = engine_environments(&daemon);
    for handle in &handles {
        let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(serde_json::json!({
            "provider_address": address.to_string(), "cloud_sandbox": {
                "image": "cloud", "size": {"cpus":1,"memory_bytes":1024,"disk_bytes":1024}, "env": {} }
        }))
        .unwrap();
        environments.put_ready(handle.id().as_str(), &spec, None, handle.backing_id(), Default::default());
        state
            .provisioned_environments
            .lock()
            .await
            .insert(handle.id().to_string(), ActiveProvisionedEnvironment { handle: handle.clone() });
    }
    assert!(reconcile_provisioned_environments(&state, NAMESPACE).await.unwrap_err().contains("unknown"));
    // Only the gone sandbox is reported absent to the Environment controller: the unknown one
    // waits for a retry, and the degraded one is logged, never recorded.
    assert_eq!(
        observed_backings(&state),
        [BackingEvent {
            name: "b-gone".into(),
            observed: Backing::Absent { reason: Some("cloud sandbox gone: LifetimeCapReached".into()) }
        }]
    );
    for name in ["a-unknown", "c-stale"] {
        assert!(environments.environments.find(name).unwrap().unwrap().is_ready(), "{name} stays ready");
    }
}

// Behaviour (C): at startup, before the Environment controller runs, adoption finds every
// backing its vendor no longer holds lost at once, however many there are; none waits out
// the adoption timeout.
#[tokio::test(start_paused = true)]
async fn startup_adoption_reports_every_absent_backing_lost_at_once() {
    use crate::runtime::environments::ENVIRONMENT_ADOPTION_TIMEOUT;
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("config")).unwrap();
    fs::write(temp.path().join("config/daemon.toml"), r#"machine_id = "absent-backings-test""#).unwrap();
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
    engine_environments(&daemon);
    let mut state = ControllerRuntimeState::new(
        daemon.clone(),
        config,
        Arc::new(ProviderRegistry::new()),
        None,
        daemon.local_host_id().unwrap().to_string(),
        "direct".into(),
    );
    // The vendor holds none of them.
    state.environment_providers.insert(address.clone(), Arc::new(AdoptionEnvironmentProvider { handles: Vec::new() })).unwrap();
    let state = Arc::new(state);
    let environments = engine_environments(&daemon);
    let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(serde_json::json!({
        "provider_address": address.to_string(), "cloud_sandbox": {
            "image": "cloud", "size": {"cpus":1,"memory_bytes":1024,"disk_bytes":1024}, "env": {} }
    }))
    .unwrap();
    let names = (0..200).map(|index| format!("env-{index:03}")).collect::<Vec<_>>();
    for name in &names {
        environments.put_ready(name, &spec, None, &format!("backing-{name}"), Default::default());
    }
    tokio::time::timeout(ENVIRONMENT_ADOPTION_TIMEOUT / 4, reconcile_provisioned_environments(&state, NAMESPACE))
        .await
        .expect("adoption judges absent backings without waiting")
        .expect("absent backings are an observation, not an error");
    let mut lost = observed_backings(&state)
        .into_iter()
        .filter(|event| matches!(event.observed, Backing::Absent { .. }))
        .map(|event| event.name)
        .collect::<Vec<_>>();
    lost.sort();
    assert_eq!(lost, names, "every absent backing is reported lost");
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

// A restarted flotillad readopts a running Docker vessel and serves its agent forward
// again, so the crews already aboard resume delivering hooks over HTTP.
#[tokio::test]
async fn docker_readoption_restores_the_daemon_forward() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"daemon-forward-readoption-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let env_id = EnvironmentId::new("env-forwarded");
    create_ready_docker_environment(&daemon, env_id.as_str(), "test-interior", BTreeSet::new()).await;
    let forwards = Arc::default();
    let handle: EnvironmentHandle = Arc::new(CachedInterior {
        inner: TestInteriorEnvironment {
            id: env_id.clone(),
            image: ImageId::new("contained-image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            destroyed: Arc::new(AtomicBool::new(false)),
        },
        cache: Arc::default(),
        forwards: Arc::clone(&forwards),
    });
    engine_environments(&daemon);
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            adoption_registry(vec![handle]),
            None,
            daemon.local_host_id().expect("local host identity").to_string(),
            "host-direct-test".to_string(),
        )
        .with_environment_tools(EnvironmentToolProvisioner::new(Vec::new())),
    );

    reconcile_provisioned_environments(&state, NAMESPACE).await.expect("readopt the running vessel");

    assert!(daemon.environment_registry_for_environment(&env_id).is_some());
    let sockets = state.environment_sockets.as_ref().expect("socket server");
    let host = sockets.prepare_agent_forward(env_id).await.expect("forwarded socket");
    let guest = PathBuf::from(flotilla_environment::environment::CONTAINED_DAEMON_FORWARD_SOCKET);
    let expected = (guest, Some(flotilla_environment::environment::CONTAINED_DAEMON_FORWARD_TCP), host.as_path().to_path_buf());
    assert_eq!(*forwards.lock().expect("forwards"), [expected]);
}

// #4163: a cloud backing whose provider gives no descriptor fails its provision
// before the vessel layer, and the backing this provision created is destroyed.
// A cloud sandbox takes its provider's path even beside a Docker input, never
// the Docker path, which would record it Ready as a local backing.
#[tokio::test]
async fn cloud_backing_without_a_descriptor_is_discarded() {
    let image = format!("example.test/crew@sha256:{}", "a".repeat(64));
    let docker = flotilla_resources::DockerEnvironmentSpec {
        host_ref: String::new(),
        image: image.clone(),
        image_build_ref: None,
        declared_agent_adapters: Default::default(),
        required_agent_adapters: Default::default(),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: Default::default(),
    };
    for docker in [None, Some(docker)] {
        let spec = flotilla_resources::EnvironmentSpec {
            provider_address: Some("account:cloud/provider:cloud".parse().unwrap()),
            cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
                probe_needs: Default::default(),
                image: image.clone(),
                image_build_ref: None,
                size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1024, disk_bytes: 1024 },
                lifetime_cap_seconds: None,
                env: Default::default(),
            }),
            docker,
            ..Default::default()
        };
        undescribed_cloud_backing_is_discarded(spec).await;
    }
}

async fn undescribed_cloud_backing_is_discarded(spec: flotilla_resources::EnvironmentSpec) {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("config")).unwrap();
    fs::write(temp.path().join("config/daemon.toml"), r#"machine_id = "cloud-descriptor-test""#).unwrap();
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        config.clone(),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("controller"),
        Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    let destroyed = Arc::new(AtomicBool::new(false));
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("undescribed"),
        image: ImageId::new(format!("example.test/crew@sha256:{}", "a".repeat(64))),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::clone(&destroyed),
    });
    let address = spec.provider_address.clone().unwrap();
    engine_environments(&daemon);
    let mut state = ControllerRuntimeState::new(
        daemon.clone(),
        config,
        Arc::new(ProviderRegistry::new()),
        None,
        daemon.local_host_id().unwrap().to_string(),
        "direct".into(),
    );
    state
        .environment_providers
        .insert(address.clone(), Arc::new(TestInteriorEnvironmentProvider { handle: tokio::sync::Mutex::new(Some(handle)) }))
        .unwrap();
    // A cloud sandbox beside a Docker input is refused at request, so it can never take the
    // Docker path; the cloud sandbox alone takes its provider's.
    if spec.docker.is_some() {
        let refusal = engine_environments(&daemon).environments.request("undescribed", &spec, None).expect_err("two backings");
        assert!(refusal.contains("exactly one of docker, host_direct and cloud_sandbox"), "{refusal}");
        return;
    }
    engine_environments(&daemon).request("undescribed", &spec, None);
    let error =
        DockerControllerRuntime { state: Arc::new(state) }.provision_environment("undescribed", &spec).await.expect_err("no descriptor");
    assert!(
        matches!(&error, flotilla_environment::environment::EnvironmentProvisionError::Failed(message) if message.contains("no provider descriptor")),
        "{error:?}"
    );
    assert!(destroyed.load(Ordering::SeqCst), "the backing this provision created is destroyed");
}
