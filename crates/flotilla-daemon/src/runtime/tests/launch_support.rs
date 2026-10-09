use super::*;

pub(super) enum LaunchObservationRecord {
    Present,
    Missing,
    MissingConvoy,
    MissingEnvironment,
}

pub(super) async fn assert_contained_claude_invocation_home(private_home: bool, observation: LaunchObservationRecord) {
    let temp = TempDir::new().expect("tempdir");
    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    fs::create_dir_all(config.base_path()).expect("config directory");
    fs::write(config.base_path().join("daemon.toml"), "machine_id = \"contained-claude-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let backend = daemon.resource_backend();
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "test@example.com".to_string() },
                source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("Claude credential declaration");

    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let env_id = EnvironmentId::new("contained-claude");
    if !matches!(observation, LaunchObservationRecord::MissingEnvironment) {
        backend
            .using::<Environment>(NAMESPACE)
            .create(
                &empty_meta(env_id.as_str()),
                &EnvironmentSpec {
                    host_direct: None,
                    docker: Some(flotilla_resources::DockerEnvironmentSpec {
                        image_composition: None,
                        image_build_ref: None,
                        memory_policy: Default::default(),
                        host_ref: "host-test".into(),
                        image: "contained-image".into(),
                        declared_agent_adapters: BTreeSet::from(["claude-code".into()]),
                        required_agent_adapters: BTreeSet::from(["claude-code".into()]),
                        pull_policy: Default::default(),
                        mounts: Vec::new(),
                        env: if private_home {
                            BTreeMap::from([("FLOTILLA_CREW_SKILLS".into(), "{\"coder\":[],\"reviewer\":[]}".into())])
                        } else {
                            BTreeMap::new()
                        },
                    }),
                },
            )
            .await
            .expect("environment");
    }
    // Credential preflight runs through the contained runner, so its
    // scratch directory must be inside the container user's writable
    // world rather than derived from daemon-host paths (#1508).
    let preflight_config_dir = PathBuf::from("/home/crew/flotilla/credentials/claude-max/claude-preflight");
    let crew_config_dir = PathBuf::from("/home/crew/flotilla/credentials/claude-max/claude");
    let runner: Arc<dyn CommandRunner> = Arc::new(CredentialInteriorRunner(
        DiscoveryMockRunner::builder()
            .tool_exists("claude", true)
            .on_run("mkdir", &["-p", &preflight_config_dir.to_string_lossy()], Ok(String::new()))
            .on_run("mkdir", &["-p", &crew_config_dir.to_string_lossy()], Ok(String::new()))
            .build(),
        None,
    ));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", env_id.to_string()))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
    assert!(bag.find_env_var("CLAUDE_CONFIG_DIR").is_none(), "the contained discovery environment must reproduce udder");
    let pool = Arc::new(FakeTerminalPool::new());
    let mut contained_registry = ProviderRegistry::new();
    contained_registry.agent_adapters = AgentAdapterRegistry::discover(&bag, Arc::clone(&runner));
    contained_registry.terminal_pools.insert(
        "fake-terminals",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "fake-terminals"),
        pool.clone(),
    );
    let contained_registry = Arc::new(contained_registry);
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: env_id.clone(),
        image: ImageId::new("contained-image"),
        runner: Arc::clone(&runner),
        env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
        destroyed: Arc::new(AtomicBool::new(false)),
    });
    daemon
        .register_provisioned_environment(env_id.clone(), handle, bag.clone(), Some(Arc::clone(&contained_registry)))
        .expect("register contained environment");

    let credential_store = Arc::new(CredentialStore::new(
        backend,
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        bag,
        Arc::clone(&runner),
        config.state_dir().as_path().to_path_buf(),
    ));
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            Arc::new(ProviderRegistry::new()),
            None,
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_credential_store(credential_store)
        .with_agent_material(Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([] as [(&str, &str); 0]))))),
    );
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: env_id.to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Implement the issue.".to_string(),
                copies: Vec::new(),
            },
            context: Box::new(flotilla_resources::TerminalCrewContext {
                namespace: NAMESPACE.to_string(),
                convoy: "demo".to_string(),
                vessel_ref: "demo-work".to_string(),
            }),
            message: None,
        },
        cwd: workspace.display().to_string(),
        env: BTreeMap::from([
            ("DECLARED_VALUE".into(), "hello declared crew".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "superseded-spec-token".into()),
        ]),
        pool: "fake-terminals".to_string(),
    };
    // Managed launch cards read durable context, just as the live command does.
    let backend = daemon.resource_backend();
    if !matches!(observation, LaunchObservationRecord::MissingConvoy) {
        backend
            .using::<Convoy>(NAMESPACE)
            .create(&empty_meta("demo"), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("convoy");
    }
    backend
        .using::<Vessel>(NAMESPACE)
        .create(
            &empty_meta("demo-work"),
            &flotilla_resources::VesselSpec {
                convoy_ref: "demo".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    let mut terminal_meta = empty_meta("terminal-demo-work-coder");
    terminal_meta.annotations.insert(flotilla_resources::CREDENTIAL_REFS_ANNOTATION.into(), "[\"claude-max\"]".into());
    if matches!(observation, LaunchObservationRecord::Present) {
        backend.using::<TerminalSession>(NAMESPACE).create(&terminal_meta, &spec).await.expect("terminal context");
    }
    let tags = [flotilla_resources::TerminalSessionTag::new(CREDENTIAL_REF_SESSION_TAG, "claude-max")];

    let launched = TerminalControllerRuntime { state }
        .ensure_session("terminal-demo-work-coder", &spec, &tags)
        .await
        .expect("launch contained Claude with its granted credential");

    let ensured = pool.ensured.lock().await;
    let [launch] = ensured.as_slice() else {
        panic!("expected exactly one contained Claude launch");
    };
    // #2706: session declarations survive, and granted credentials override conflicts.
    assert!(launch.env_vars.iter().any(|(name, value)| name == "DECLARED_VALUE" && value == "hello declared crew"));
    assert!(!launch.env_vars.iter().any(|(_, value)| value == "superseded-spec-token"));
    assert!(
        launch.env_vars.iter().any(|(name, value)| name == "CLAUDE_CODE_OAUTH_TOKEN" && value == "oauth-secret-material"),
        "the contained Claude process must receive its OAuth token"
    );
    assert!(
        launch.env_vars.iter().any(|(name, value)| name == "CLAUDE_CONFIG_DIR"
            && value
                == if private_home {
                    crew_config_dir.join("crews/coder").display().to_string()
                } else {
                    crew_config_dir.display().to_string()
                }
                .as_str()),
        "the contained Claude process must receive the config directory owned by its adapter"
    );
    // The first turn includes the assignment and its capability card, never token material.
    assert!(launch.command.contains("Implement the issue."));
    assert!(launch.command.contains("## Your capabilities"));
    if matches!(observation, LaunchObservationRecord::MissingConvoy | LaunchObservationRecord::MissingEnvironment) {
        assert!(launch.command.contains("Capabilities are currently unavailable"));
        assert!(launch.command.contains("flotilla crew capabilities"));
    } else {
        assert!(launch.command.contains("claude-max"));
    }
    assert!(!launch.command.contains("oauth-secret-material"));
    assert!(!launch.command.contains("superseded-spec-token"));
    assert_eq!(launch.initial_size, Some(CREW_SESSION_SIZE));
    let crew_id = &launched.crew.expect("contained crew identity").id;
    for (name, expected) in [
        ("FLOTILLA_CREW_ID", crew_id.as_str()),
        ("FLOTILLA_CONVOY", "demo"),
        ("FLOTILLA_VESSEL", "demo-work"),
        ("FLOTILLA_CREW_ROLE", "coder"),
        ("FLOTILLA_NAMESPACE", NAMESPACE),
        ("FLOTILLA_TERMINAL_SESSION", "terminal-demo-work-coder"),
        ("GIT_AUTHOR_NAME", "flotilla-crew[bot]"),
        ("GIT_AUTHOR_EMAIL", "309902803+flotilla-crew[bot]@users.noreply.github.com"),
        ("GIT_COMMITTER_NAME", "flotilla-crew[bot]"),
        ("GIT_COMMITTER_EMAIL", "309902803+flotilla-crew[bot]@users.noreply.github.com"),
    ] {
        assert!(launch.env_vars.iter().any(|(key, value)| key == name && value == expected), "contained launch missing {name}");
    }
}
