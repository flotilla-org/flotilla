use super::*;

#[tokio::test]
async fn local_profile_does_not_advertise_docker_without_interior_cleat() {
    use flotilla_core::providers::discovery::{ProviderCategory, ProviderDescriptor};

    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"local-profile-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let mut registry = ProviderRegistry::new();
    registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(CapturingFailingEnvironmentProvider { create_opts: Mutex::new(None), prepared_auth: Mutex::new(None) }),
    );
    registry.terminal_pools.insert(
        "passthrough",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough"),
        Arc::new(flotilla_core::providers::terminal::passthrough::PassthroughTerminalPool),
    );

    let profile = build_local_profile(&daemon, &registry).expect("local profile");

    assert_eq!(profile.docker_pool, "cleat");
    assert!(!profile.docker_available, "a host must not advertise contained placement until it can deliver interior cleat");
}

#[tokio::test]
async fn docker_provisioning_mounts_interior_control_binaries_and_durable_cleat_state() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"cli-mount-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let cleat_state = config.state_dir().join("contained-cleat/contained-work");
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let provider = Arc::new(CapturingFailingEnvironmentProvider { create_opts: Mutex::new(None), prepared_auth: Mutex::new(None) });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::clone(&provider) as Arc<dyn EnvironmentProvider>,
    );
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(with_fourth_tool(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
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
    };

    let error = DockerControllerRuntime { state: Arc::clone(&state) }
        .provision("contained-work", &spec)
        .await
        .expect_err("capture provider should stop provision");

    assert_eq!(error.to_string(), "stop after capturing create options");
    // Glue: an extra enrollment reaches provider delivery without changing CreateOpts.
    let opts = provider.create_opts.lock().await.take().expect("captured create options");
    assert!(opts.provisioned_mounts.is_empty(), "tool assets remain provider-neutral until the environment provider delivers them");
    assert_eq!(
        opts.tokens,
        crew_git_identity_environment(),
        "tool environment belongs to the tool description; crew Git identity is a container baseline"
    );
    assert_eq!(
        opts.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        vec!["flotilla", "cleat", "rust-build-limits", "fourth"]
    );
    // The contained tool delivers both the workspace default and dependency profile shim.
    assert!(opts.tools[2].environment.contains(&EnvironmentVariableUpdate::set(
        "CARGO_PROFILE_DEV_DEBUG",
        "line-tables-only",
        "the Rust workspace debug profile"
    )));
    assert!(opts.tools[2].environment.contains(&EnvironmentVariableUpdate::prepend_path("PATH", CONTAINED_CARGO_SHIM_DIRECTORY)));
    assert!(opts.tools[2].assets.iter().any(|asset| asset.environment_path.as_path() == Path::new(CONTAINED_CARGO_SHIM_PATH)));
    let jobs = opts.cpu_limit.expect("container CPU quota");
    assert!(jobs >= 2);
    assert!(opts.tools[2].environment.contains(&EnvironmentVariableUpdate::set(
        "CARGO_BUILD_JOBS",
        jobs.to_string(),
        "the Rust build share"
    )));
    assert!(opts.tools[2].environment.contains(&EnvironmentVariableUpdate::set(
        "FLOTILLA_LINKER_THREADS",
        jobs.to_string(),
        "the Rust linker cap"
    )));
    assert!(opts.tools[2].environment.contains(&EnvironmentVariableUpdate::set(
        "RUSTC_WORKSPACE_WRAPPER",
        CONTAINED_RUSTC_WRAPPER_PATH,
        "the Rust linker cap"
    )));
    assert_eq!(opts.tools[0].executable.as_path(), Path::new(ENVIRONMENT_FLOTILLA_PATH));
    assert_eq!(opts.tools[0].assets[2].environment_path.as_path(), Path::new(ENVIRONMENT_DAEMON_SOCKET_PATH));
    assert_eq!(
        opts.tools[0].environment[1],
        EnvironmentVariableUpdate::set("FLOTILLA_CONTAINED_HOST_DAEMON", "1", "the contained host-daemon requirement",),
    );
    assert_eq!(opts.tools[1].executable.as_path(), Path::new(ENVIRONMENT_CLEAT_PATH));
    assert_eq!(opts.tools[1].assets[1].environment_path.as_path(), Path::new(ENVIRONMENT_CLEAT_GHOSTTY_LIBRARY_PATH));
    assert_eq!(opts.tools[1].assets[2].host_path.as_path(), cleat_state.as_path());
    assert_eq!(opts.tools[1].assets[2].environment_path.as_path(), Path::new(ENVIRONMENT_CLEAT_RUNTIME_DIR));
    assert_eq!(opts.tools[1].environment[1], EnvironmentVariableUpdate::prepend_path("LD_LIBRARY_PATH", ENVIRONMENT_CLEAT_LIBRARY_DIR),);
    assert!(cleat_state.as_path().is_dir(), "durable tool state must exist before the provider delivers it");
}

#[tokio::test]
async fn docker_provisioning_fails_loudly_without_interior_cleat_assets() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"missing-cleat-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let provider = Arc::new(CapturingFailingEnvironmentProvider { create_opts: Mutex::new(None), prepared_auth: Mutex::new(None) });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::clone(&provider) as Arc<dyn EnvironmentProvider>,
    );
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(EnvironmentToolProvisioner::with_unavailable_cleat(
            DaemonHostPath::new("/opt/flotilla/bin/flotilla"),
            DaemonHostPath::new("/tmp/flotilla.sock"),
            "binary unavailable for contained environment delivery",
        )),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
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
    };

    let error = DockerControllerRuntime { state }
        .provision("contained-work", &spec)
        .await
        .expect_err("provisioning without cleat must fail before creating a container");

    assert_eq!(error.to_string(), "cleat unavailable for environment provisioning: binary unavailable for contained environment delivery");
    assert!(provider.create_opts.lock().await.is_none(), "the incomplete container must not be created");
}

#[tokio::test]
async fn registry_admission_delivers_opaque_auth_without_runtime_preflight_and_refuses_missing_store() {
    for outcome in [RegistryPreflightOutcome::Success, RegistryPreflightOutcome::MissingStore] {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"prepared-registry-auth-test\"\n").expect("daemon config");
        let home = temp.path().join("home");
        let skill_sources = write_test_skill_sources(temp.path());
        // A previous aborted attempt may have left a delivered agent credential.
        // Every refusal, including a missing credential store, must discard it.
        let delivered_auth = home.join(".local/share/flotilla/agent-homes/prepared-auth-environment/codex/auth.json");
        fs::create_dir_all(delivered_auth.parent().expect("credential parent")).expect("agent home");
        fs::write(&delivered_auth, "test credential").expect("delivered credential");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
        let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
        daemon
            .resource_backend()
            .definitions::<CredentialSpec>(NAMESPACE)
            .create(
                &InputMeta::builder().name("private-registry".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::DockerRegistry { registry: "registry.example".to_string(), username: "crew".to_string() },
                    source: CredentialSource::Env { name: "TEST_REGISTRY_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create registry credential");
        let provider = Arc::new(CapturingFailingEnvironmentProvider { create_opts: Mutex::new(None), prepared_auth: Mutex::new(None) });
        let mut local_registry = ProviderRegistry::new();
        local_registry.environment_providers.insert(
            "sandbox",
            flotilla_core::providers::discovery::ProviderDescriptor::named(
                flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
                "sandbox",
            ),
            Arc::clone(&provider) as Arc<dyn EnvironmentProvider>,
        );
        let registry_runner = Arc::new(RegistryPreflightRunner { outcome, calls: AtomicUsize::new(0), directory: Mutex::new(None) });
        let credential_store = Arc::new(CredentialStore::new(
            daemon.resource_backend(),
            NAMESPACE,
            Arc::new(TestEnvVars::new([("HOME", home.display().to_string()), ("TEST_REGISTRY_TOKEN", "registry-secret".to_string())])),
            EnvironmentBag::new(),
            registry_runner.clone(),
            config.state_dir().as_path().to_path_buf(),
        ));
        let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
            ("HOME", home.display().to_string()),
            (FLOTILLA_SKILLS_DIR_ENV, skill_sources.display().to_string()),
        ]))));
        let state = ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))
        .with_agent_material(agent_material);
        let state =
            Arc::new(if outcome == RegistryPreflightOutcome::MissingStore { state } else { state.with_credential_store(credential_store) });
        let spec = flotilla_resources::DockerEnvironmentSpec {
            image_composition: None,
            image_build_ref: None,
            memory_policy: Default::default(),
            host_ref: "host-test".to_string(),
            image: "registry.example/crew:latest".to_string(),
            declared_agent_adapters: BTreeSet::new(),
            required_agent_adapters: BTreeSet::new(),
            pull_policy: Default::default(),
            mounts: Vec::new(),
            env: BTreeMap::from([(
                CREDENTIAL_REFS_ENV.to_string(),
                serde_json::to_string(&BTreeSet::from(["private-registry".to_string()])).expect("encode credential refs"),
            )]),
        };

        let error = DockerControllerRuntime { state }
            .provision("prepared-auth-environment", &spec)
            .await
            .expect_err("capture provider or preflight must stop provision");

        // Glue: admission delivers an opaque handle to the selected provider without
        // running Docker, and refuses creation when the credential store is absent.
        assert!(!delivered_auth.exists(), "refused creation must discard delivered agent credentials");
        let opts = provider.create_opts.lock().await.take();
        if outcome == RegistryPreflightOutcome::Success {
            assert_eq!(error, "stop after capturing create options");
            let auth = opts.expect("provider invoked").prepared_auth.expect("opaque auth admitted");
            auth.validate("registry.example/crew:latest", flotilla_resources::HostImageAction::ImagePull).expect("pull admitted");
            assert_eq!(registry_runner.calls.load(Ordering::SeqCst), 0, "credential minting never invokes Docker");
        } else {
            assert!(opts.is_none(), "provider must not run without successful preflight");
            assert!(
                error.contains(if outcome == RegistryPreflightOutcome::MissingStore {
                    "credential store unavailable"
                } else {
                    "preflight failed"
                }),
                "{error}"
            );
            if let Some(directory) = registry_runner.directory.lock().await.as_ref() {
                assert!(!directory.exists(), "failed preflight must remove its auth artifact");
            }
        }
    }
}

#[tokio::test]
async fn docker_teardown_and_forced_cleanup_archive_persistent_agent_home() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"material-restart-teardown-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let destroyed = Arc::new(AtomicBool::new(false));
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("contained-restarted"),
        image: ImageId::new("contained-image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::clone(&destroyed),
    });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::new(ListingEnvironmentProvider { handle }),
    );
    let home = temp.path().join("home");
    let environment_home = home.join(".local/share/flotilla/agent-homes/contained-restarted");
    fs::create_dir_all(environment_home.join("codex/sessions")).expect("persistent agent home");
    fs::write(environment_home.join("codex/sessions/rollout.jsonl"), "session state").expect("persistent session state");
    let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([("HOME", home.display().to_string())]))));
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            config,
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_agent_material(agent_material),
    );

    let cleat_state = state.config.state_dir().as_path().join("contained-cleat/contained-restarted");
    fs::create_dir_all(&cleat_state).expect("contained-cleat state");
    fs::write(cleat_state.join("session"), "durable session").expect("session state");
    let outside_state = state.config.state_dir().as_path().join("keep");
    fs::create_dir_all(&outside_state).expect("unrelated state");
    fs::write(outside_state.join("sentinel"), "keep").expect("sentinel");
    let runtime = DockerControllerRuntime { state };
    for invalid in ["../keep", "..", "/", ""] {
        runtime.cleanup(invalid).await.expect("unsafe names must not wedge finalization");
    }
    assert!(outside_state.join("sentinel").exists(), "unsafe cleanup must preserve unrelated state");
    runtime.destroy("contained-restarted", "test-interior").await.expect("restart teardown should rediscover and destroy the container");

    assert!(destroyed.load(Ordering::SeqCst), "the restarted daemon must destroy the still-running lease holder");
    assert!(!environment_home.exists(), "durable environment teardown must move its persistent agent home");
    assert_eq!(
        fs::read_to_string(home.join(".local/share/flotilla/session-archive/unowned/contained-restarted/codex/sessions/rollout.jsonl"))
            .unwrap(),
        "session state"
    );
    // Forced teardown can leave no resource records; the admission name still identifies the convoy.
    let convoy = "convoy-0123456789abcdef0123456789abcdef";
    let environment = format!("env-{convoy}-work");
    let forced_home = home.join(".local/share/flotilla/agent-homes").join(&environment);
    fs::create_dir_all(&forced_home).unwrap();
    fs::write(forced_home.join("session.jsonl"), "forced session").unwrap();
    runtime.cleanup(&environment).await.unwrap();
    assert_eq!(
        fs::read_to_string(home.join(".local/share/flotilla/session-archive").join(convoy).join(&environment).join("session.jsonl"))
            .unwrap(),
        "forced session"
    );
    assert!(!cleat_state.exists(), "durable teardown must remove contained-cleat state too");
}

// #2840: a restart can lose the container status after Docker creation.
// Finalization must rediscover the backing rather than deleting only local files.
#[tokio::test]
async fn docker_orphan_cleanup_without_persisted_status_reaps_backing() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"orphan-status-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("dinghy"),
    )
    .await;
    let destroyed = Arc::new(AtomicBool::new(false));
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("env-lost-status"),
        image: ImageId::new("image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::clone(&destroyed),
    });
    let mut registry = ProviderRegistry::new();
    registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::new(AdoptionEnvironmentProvider { handles: vec![Arc::clone(&handle)] }),
    );
    let state =
        Arc::new(ControllerRuntimeState::new(daemon, config, Arc::new(registry), None, "host-test".into(), "host-direct-host-test".into()));
    // Restart discarded both status identity and the in-process handle cache.
    assert!(state.provisioned_environments.lock().await.is_empty());
    let backend = state.daemon.resource_backend();
    let environment = backend
        .using::<Environment>(NAMESPACE)
        .create(
            &empty_meta("env-lost-status"),
            &EnvironmentSpec {
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    host_ref: "host-test".into(),
                    image: "image".into(),
                    image_composition: None,
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    declared_agent_adapters: Default::default(),
                    required_agent_adapters: Default::default(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("environment without saved status");
    EnvironmentReconciler::new(Arc::new(DockerControllerRuntime { state }), backend, NAMESPACE)
        .run_finalizer(&environment)
        .await
        .expect("finalize without status");
    assert!(destroyed.load(Ordering::SeqCst), "missing status must not leave the backing container running");
}

#[tokio::test]
async fn docker_provisioning_reports_unavailable_flotilla_cli() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"cli-unavailable-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let state = Arc::new(ControllerRuntimeState::new(
        daemon,
        config,
        Arc::new(ProviderRegistry::new()),
        Some(DaemonHostPath::new("/tmp/flotilla.sock")),
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    ));
    let spec = flotilla_resources::DockerEnvironmentSpec {
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
    };

    let error = DockerControllerRuntime { state }.provision("contained-work", &spec).await.expect_err("missing CLI should fail provision");

    assert!(
        error.to_string().starts_with("flotilla CLI unavailable for environment provisioning: "),
        "unavailable CLI resolution should retain its clear provisioning context: {error}",
    );
}

#[tokio::test]
async fn docker_provisioning_rejects_mounts_targeting_reserved_tool_assets() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"cli-collision-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let provider = Arc::new(CapturingFailingEnvironmentProvider { create_opts: Mutex::new(None), prepared_auth: Mutex::new(None) });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::clone(&provider) as Arc<dyn EnvironmentProvider>,
    );
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf())),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::new(),
        required_agent_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        mounts: vec![flotilla_resources::EnvironmentMount {
            source_path: "/host/replacement-socket-directory".to_string(),
            target_path: "/run/flotilla-daemon".to_string(),
            mode: flotilla_resources::EnvironmentMountMode::Ro,
        }],
        env: BTreeMap::new(),
    };

    let error = DockerControllerRuntime { state: Arc::clone(&state) }
        .provision("contained-work", &spec)
        .await
        .expect_err("reserved socket directory mount should fail provision");

    assert_eq!(error.to_string(), "mount target /run/flotilla-daemon is reserved for the daemon socket");
    assert!(provider.create_opts.lock().await.is_none(), "reserved mount collisions should fail before invoking the provider");

    let cli_collision_spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        mounts: vec![flotilla_resources::EnvironmentMount {
            source_path: "/host/replacement-flotilla".to_string(),
            target_path: "/usr/local/bin/flotilla".to_string(),
            mode: flotilla_resources::EnvironmentMountMode::Ro,
        }],
        ..spec
    };
    let error = DockerControllerRuntime { state }
        .provision("contained-work", &cli_collision_spec)
        .await
        .expect_err("reserved CLI mount should fail provision");

    assert_eq!(error.to_string(), "mount target /usr/local/bin/flotilla is reserved for the flotilla CLI");
    assert!(provider.create_opts.lock().await.is_none(), "reserved mount collisions should fail before invoking the provider");
}

#[tokio::test]
async fn remotely_placed_docker_shared_clone_provisions_across_two_stores() {
    remote_shared_clone_placement_reaches_running(true).await;
}

#[tokio::test]
async fn provisioned_environment_discovers_and_registers_interior_agent_adapters() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"interior-discovery-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    discovery.host_detectors = Arc::new(flotilla_core::providers::discovery::detectors::default_host_detectors());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let state = ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        passthrough_registry(),
        None,
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    );
    let env_id = EnvironmentId::new("contained-work");
    let runner: Arc<dyn CommandRunner> = Arc::new(
        DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 1.2.3".to_string()))
            .on_run("claude", &["--version"], Ok("1.0.0 (Claude Code)".to_string()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/usr/local/bin/codex\n".to_string()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "claude"], Ok("/usr/local/bin/claude\n".to_string()))
            .build(),
    );
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: env_id.clone(),
        image: ImageId::new("contained-image"),
        runner,
        env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
        destroyed: Arc::new(AtomicBool::new(false)),
    });

    let (bag, registry) = probe_provisioned_environment(&state, &env_id, &handle).await.expect("interior discovery should succeed");

    assert!(bag.find_binary("codex").is_some());
    assert!(bag.find_binary("claude").is_some());
    assert_eq!(bag.find_env_var("HOME"), Some("/home/crew"));
    assert!(registry.agent_adapters.get("codex").is_some());
    assert!(registry.agent_adapters.get("claude-code").is_some());

    daemon
        .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(Arc::clone(&registry)))
        .expect("provisioned environment should register");
    let registered = daemon.environment_registry_for_environment(&env_id).expect("interior registry should be available");
    assert!(registered.agent_adapters.get("codex").is_some());
    assert!(registered.agent_adapters.get("claude-code").is_some());

    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::from(["codex".to_string(), "missing-adapter".to_string()]),
        required_agent_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::new(),
    };
    let error = verify_declared_agent_adapters(&spec, &registry).expect_err("missing declared adapter should fail");
    assert_eq!(error, "image `contained-image` declares agent adapter `missing-adapter`, but interior discovery did not find it");
}

// #2941: a baseline policy must survive real admission's prepared snapshot,
// Vessel creation and Environment preparation; an authored literal tag must refuse.
#[tokio::test]
async fn fresh_contained_admission_prepares_baseline_but_refuses_literal_tag() {
    use flotilla_core::providers::environment::{docker::DockerEnvironmentProvider, EnvironmentProvider, PrepareOpts};
    use flotilla_resources::{CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource};

    // Exhaust baseline sources with/without layers and an authored literal.
    // Timing/interleavings do not affect this immutable admission chain;
    // old live records and routed duplicate admissions have separate coverage.
    for (baseline, layers) in [(true, false), (true, true), (false, false)] {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("daemon.toml"), "machine_id = \"fresh-baseline-admission\"\n").expect("config");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = InProcessDaemon::new(
            Vec::new(),
            config.clone(),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            flotilla_protocol::HostName::new("udder"),
        )
        .await;
        let backend = daemon.resource_backend();
        let selection = if layers {
            use flotilla_resources::{ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec, ImageLayerStage};
            backend
                .definitions::<ImageLayer>(NAMESPACE)
                .apply(
                    &empty_meta("base"),
                    &ImageLayerSpec::builder()
                        .stage(ImageLayerStage::Base)
                        .parent(ImageLayerParent::Image(format!("debian@sha256:{}", "a".repeat(64))))
                        .repository("https://example.test/images".into())
                        .revision("1".repeat(40))
                        .fragment("Dockerfile.base".into())
                        .build(),
                )
                .await
                .expect("base layer");
            Some(ImageLayerSelection::builder().base("base".into()).build())
        } else {
            None
        };
        backend
            .definitions::<CrewImageBaseline>(NAMESPACE)
            .apply(&empty_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:baseline".into(), layers: selection })
            .await
            .expect("baseline");
        let hosts = backend.using::<Host>(NAMESPACE);
        let host = hosts.create(&empty_meta(daemon.local_host_id().expect("host").as_str()), &HostSpec::default()).await.expect("host");
        hosts
            .update_status(
                &host.metadata.name,
                &host.metadata.resource_version,
                &HostStatus {
                    ready: true,
                    heartbeat_at: Some(Utc::now()),
                    admission_free_space_floor_bytes: Some(0),
                    capabilities: BTreeMap::from([("docker".into(), json!(true)), ("os".into(), json!("linux"))]),
                    ..Default::default()
                },
            )
            .await
            .expect("host status");
        let repositories = backend.using::<Repository>(NAMESPACE);
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
        let repository =
            flotilla_resources::ensure_repository(&repositories, &repository_spec.key(), &repository_spec).await.expect("repository");
        repositories
            .update_status(
                &repository.metadata.name,
                &repository.metadata.resource_version,
                &flotilla_resources::RepositoryStatus { default_branch: Some("main".into()), ..Default::default() },
            )
            .await
            .expect("default branch");
        let image = if baseline {
            DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() }
        } else {
            DockerImageSource::Literal("crew:baseline".into())
        };
        backend
            .using::<PlacementPolicy>(NAMESPACE)
            .create(
                &empty_meta("crew-policy"),
                &PlacementPolicySpec::builder()
                    .pool("cleat".into())
                    .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                        legacy_image_baseline_ref: None,
                        host_ref: daemon.local_host_id().expect("host").to_string(),
                        image,
                        pull_policy: flotilla_resources::DockerImagePullPolicy::IfNotPresent,
                        memory_policy: Default::default(),
                        agent_adapters: Default::default(),
                        default_cwd: None,
                        env: Default::default(),
                        checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".into() },
                    })
                    .build(),
            )
            .await
            .expect("policy");
        backend
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("baseline-workflow"),
                &WorkflowTemplateSpec::builder()
                    .vessels(vec![VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![CrewSpec::builder().role("tool".into()).source(CrewSource::Tool { command: "true".into() }).build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("workflow");
        let mut events = daemon.subscribe();
        let id = daemon
            .execute(
                Command::builder()
                    .action(CommandAction::ConvoyCreate {
                        name: "baseline-convoy".into(),
                        workflow_ref: "baseline-workflow".into(),
                        inputs: Vec::new(),
                        repository_url: Some("https://github.com/flotilla-org/flotilla".into()),
                        r#ref: Some("test/baseline".into()),
                        project_ref: None,
                        placement_policy: Some("crew-policy".into()),
                        adopted_checkout: None,
                    })
                    .build(),
            )
            .await
            .expect("admit");
        let result = wait_for_command_result(&mut events, id).await;
        assert!(matches!(result, CommandValue::ConvoyCreated { .. }), "admission: {result:?}");
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let name = convoy_record_name(&backend, "baseline-convoy").await;
        let convoy_reconciler =
            ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(NAMESPACE)).with_vessels(backend.using::<Vessel>(NAMESPACE));
        let mut created_vessel = None;
        for _ in 0..4 {
            let convoy = convoys.get(&name).await.expect("convoy");
            let prepared = convoy_reconciler.prepare(&convoy).await.expect("prepare convoy");
            let outcome = convoy_reconciler.reconcile(&convoy, &prepared, Utc::now());
            if let Some((meta, spec)) = outcome.actuations.into_iter().find_map(|actuation| match actuation {
                Actuation::CreateVessel { meta, spec } => Some((meta, spec)),
                _ => None,
            }) {
                created_vessel = Some(backend.using::<Vessel>(NAMESPACE).create(&meta, &spec).await.expect("vessel"));
                break;
            }
            let patch = outcome.patch.expect("convoy must advance toward Vessel");
            flotilla_resources::apply_status_patch(&convoys, &name, &patch).await.expect("advance convoy");
        }
        let vessel = created_vessel.expect("convoy must create a Vessel within four transitions");
        assert!(vessel.spec.placement_policy_ref.starts_with("placement-snapshot-"));
        let snapshot = backend.using::<PlacementPolicy>(NAMESPACE).get(&vessel.spec.placement_policy_ref).await.expect("snapshot");
        let docker = snapshot.spec.docker_per_vessel.expect("docker snapshot");
        assert_eq!(docker.legacy_image_baseline_ref.as_deref(), baseline.then_some("fleet-crew"));
        assert_eq!(matches!(docker.image, DockerImageSource::Composition { .. }), layers);
        // Mutating the live baseline after admission must not change the frozen image.
        backend
            .definitions::<CrewImageBaseline>(NAMESPACE)
            .apply(&empty_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:changed".into(), layers: None })
            .await
            .expect("change live baseline");
        let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
        let prepared = reconciler.prepare(&vessel).await.expect("prepare vessel");
        let outcome = reconciler.reconcile(&vessel, &prepared, Utc::now());
        let (meta, spec) = outcome
            .actuations
            .into_iter()
            .find_map(|actuation| match actuation {
                Actuation::CreateEnvironment { meta, spec } => Some((meta, spec)),
                _ => None,
            })
            .expect("Vessel must create Environment");
        assert_eq!(spec.docker.as_ref().expect("docker").image, "crew:baseline");
        assert_eq!(meta.labels.get("flotilla.work/legacy-image-baseline").map(String::as_str), baseline.then_some("fleet-crew"));
        let environment = backend.using::<Environment>(NAMESPACE).create(&meta, &spec).await.expect("environment");
        let state = Arc::new(ControllerRuntimeState::new(
            daemon.clone(),
            config,
            passthrough_registry(),
            None,
            daemon.local_host_id().expect("host").to_string(),
            "host-direct-test".into(),
        ));
        let runtime = DockerControllerRuntime { state };
        let legacy_baseline = runtime.legacy_baseline_for_environment(&environment.metadata.name).await.expect("provenance");
        let provider = DockerEnvironmentProvider::new(Arc::new(HeldBaselineRunner { image: "crew:baseline".into() }));
        let result = provider.prepare(&environment.spec, &PrepareOpts { legacy_baseline, ..Default::default() }).await;
        if baseline {
            assert!(result.is_ok(), "baseline prepare: {:?}", result.err());
        } else {
            assert_eq!(
                result.err().expect("literal must refuse"),
                "Docker environments require a pinned digest; mutable tags are unsupported"
            );
        }
        assert_eq!(legacy_baseline, baseline);
        if baseline {
            // Recovery also works if an Environment lacks the new label:
            // its owning Vessel still names the immutable prepared snapshot.
            let mut meta = InputMeta::from(&environment.metadata);
            meta.labels.remove("flotilla.work/legacy-image-baseline");
            backend
                .using::<Environment>(NAMESPACE)
                .update(&meta, &environment.metadata.resource_version, &environment.spec)
                .await
                .expect("unlabeled snapshot-backed Environment");
            let legacy_baseline = runtime.legacy_baseline_for_environment(&environment.metadata.name).await.expect("snapshot provenance");
            provider
                .prepare(&environment.spec, &PrepareOpts { legacy_baseline, ..Default::default() })
                .await
                .expect("snapshot provenance prepares without Environment label");
        }
    }
}

// Pre-#2731 records have no provenance label; the owning Vessel's baseline
// policy admits them. A literal policy or an unowned tag is never admitted.
#[tokio::test]
async fn legacy_baseline_admission_recovers_old_records_and_preserves_frozen_provenance() {
    use flotilla_core::providers::environment::{docker::DockerEnvironmentProvider, EnvironmentProvider, PrepareOpts};
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"baseline-admission-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        config.clone(),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        flotilla_protocol::HostName::new("udder"),
    )
    .await;
    let backend = daemon.resource_backend();
    let policy_spec = PlacementPolicySpec::builder()
        .pool("cleat".into())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            legacy_image_baseline_ref: None,
            host_ref: "udder".into(),
            image: flotilla_resources::DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() },
            pull_policy: Default::default(),
            memory_policy: Default::default(),
            agent_adapters: Default::default(),
            default_cwd: None,
            env: Default::default(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".into() },
        })
        .build();
    let policies = backend.using::<PlacementPolicy>(NAMESPACE);
    let policy = policies.create(&empty_meta("live-policy"), &policy_spec).await.expect("policy");
    backend
        .using::<Vessel>(NAMESPACE)
        .create(
            &empty_meta("live-vessel"),
            &flotilla_resources::VesselSpec {
                convoy_ref: "live-convoy".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "live-policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    create_ready_docker_environment(&daemon, "live-environment", "backing", Default::default()).await;
    let environments = backend.using::<Environment>(NAMESPACE);
    let mut environment = environments.get("live-environment").await.expect("record");
    let docker = environment.spec.docker.as_mut().expect("docker");
    docker.image = "forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-10-02.d9e59d8c.dbd6e440".into();
    docker.image_build_ref = None;
    docker.image_composition = None;
    let mut meta = InputMeta::from(&environment.metadata);
    meta.owner_references = vec![flotilla_resources::OwnerReference {
        api_version: "flotilla.work/v1".into(),
        kind: Vessel::API_PATHS.kind.into(),
        name: "live-vessel".into(),
        controller: true,
    }];
    let environment = environments.update(&meta, &environment.metadata.resource_version, &environment.spec).await.expect("legacy record");
    let state = Arc::new(ControllerRuntimeState::new(
        daemon.clone(),
        config,
        passthrough_registry(),
        None,
        daemon.local_host_id().expect("host").to_string(),
        "host-direct-test".into(),
    ));
    let runtime = DockerControllerRuntime { state };
    assert!(runtime.legacy_baseline_for_environment("live-environment").await.expect("baseline provenance"));
    let provider = DockerEnvironmentProvider::new(Arc::new(HeldBaselineRunner {
        image: environment.spec.docker.as_ref().expect("docker").image.clone(),
    }));
    provider
        .prepare(
            &environment.spec,
            &PrepareOpts {
                legacy_baseline: runtime.legacy_baseline_for_environment("live-environment").await.expect("old provenance"),
                ..Default::default()
            },
        )
        .await
        .expect("unlabeled live baseline prepares");
    assert!(!runtime.legacy_baseline_for_environment("unknown").await.expect("absent record"));
    let mut literal = policy.spec.clone();
    literal.docker_per_vessel.as_mut().expect("docker").image = flotilla_resources::DockerImageSource::Literal("crew:tag".into());
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &literal).await.expect("literal policy");
    assert!(!runtime.legacy_baseline_for_environment("live-environment").await.expect("literal is not baseline"));
    assert_eq!(
        provider
            .prepare(
                &environment.spec,
                &PrepareOpts {
                    legacy_baseline: runtime.legacy_baseline_for_environment("live-environment").await.expect("literal provenance"),
                    ..Default::default()
                }
            )
            .await
            .err()
            .expect("literal refuses"),
        "Docker environments require a pinned digest; mutable tags are unsupported"
    );
    let mut meta = InputMeta::from(&environment.metadata);
    meta.labels.insert("flotilla.work/legacy-image-baseline".into(), "fleet-crew".into());
    environments.update(&meta, &environment.metadata.resource_version, &environment.spec).await.expect("freeze baseline provenance");
    assert!(runtime.legacy_baseline_for_environment("live-environment").await.expect("frozen baseline survives policy changes"));
    provider
        .prepare(
            &environment.spec,
            &PrepareOpts {
                legacy_baseline: runtime.legacy_baseline_for_environment("live-environment").await.expect("frozen provenance"),
                ..Default::default()
            },
        )
        .await
        .expect("labeled live baseline prepares after policy change");
}

#[tokio::test]
async fn provisioned_environment_is_destroyed_when_declared_adapter_is_missing() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"interior-rejection-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    discovery.host_detectors = Arc::new(flotilla_core::providers::discovery::detectors::default_host_detectors());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let destroyed = Arc::new(AtomicBool::new(false));
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("contained-rejected"),
        image: ImageId::new("contained-image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
        destroyed: Arc::clone(&destroyed),
    });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::new(TestInteriorEnvironmentProvider { handle: Mutex::new(Some(handle)) }),
    );
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf())),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
        required_agent_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::new(),
    };

    let error = DockerControllerRuntime { state }
        .provision("contained-rejected", &spec)
        .await
        .expect_err("missing declared adapter should reject the environment");

    assert_eq!(error.to_string(), "image `contained-image` declares agent adapter `codex`, but interior discovery did not find it");
    assert!(destroyed.load(Ordering::SeqCst), "rejected environment should be destroyed");
}
