use super::*;

pub(super) enum LaunchObservationRecord {
    Present,
    Missing,
    MissingEnvironment,
}

/// Bind a requester's Claude subscription to `credential`, as admission does (#3907).
pub(super) async fn bind_account(backend: &ResourceBackend, convoy: &str, role: &str, credential: &str, account: &str) {
    use flotilla_resources::account_access::{
        binding_name, requester_address, AccessName, AccountBindingSpec, BindingState, CredentialName, EpochEntry, GrantName, GrantScope,
        Selection,
    };
    let access = AccessName(flotilla_agent::account_access::CLAUDE_SUBSCRIPTION.to_string());
    let now = flotilla_schema::Timestamp::from_datetime(&chrono::Utc::now());
    let mut epochs = flotilla_schema::Records::new();
    epochs.append(1, EpochEntry { epoch: "1".into(), observed_at: now }).expect("epoch");
    let spec = AccountBindingSpec {
        requester: requester_address(Some("flotilla"), convoy, role).expect("requester"),
        item: "claude-code".into(),
        requirement: access.clone(),
        credential: CredentialName(credential.into()),
        account: Some(account.into()),
        grants: BTreeMap::from([(GrantName("claude-max-contained".into()), GrantScope { scope: "account".into() })]),
        candidates: BTreeSet::from([CredentialName(credential.into())]),
        exclusions: BTreeMap::new(),
        selection: Selection::Pin { pinned_by: "test".into() },
        epochs,
        opened_at: now,
        state: flotilla_schema::State::new(BindingState::Open, now),
    };
    let binding = flotilla_resources::account_access::PendingBinding { name: binding_name(convoy, role, &access), spec };
    flotilla_resources::account_access::AccountRecords::open(
        &flotilla_store::OldStoreAccounts::new(backend.clone()),
        NAMESPACE,
        vec![binding],
    )
    .await
    .expect("account binding");
}

/// As [`bind_account`], but the binding is a replica from another home, opened `ahead` later on
/// that home's clock than this host's.
pub(super) async fn bind_account_from_home(
    backend: &ResourceBackend,
    home: &str,
    convoy: &str,
    role: &str,
    credential: &str,
    ahead: chrono::Duration,
) {
    let source = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    bind_account(&source, convoy, role, credential, "box-a@example.com").await;
    let bindings = source.using::<flotilla_resources::AccountBinding>(NAMESPACE);
    let mut binding = bindings.list().await.expect("bindings").items.remove(0);
    binding.spec.opened_at = flotilla_schema::Timestamp::from_datetime(&(chrono::Utc::now() + ahead));
    bindings
        .update(
            &InputMeta::builder().name(binding.metadata.name.clone()).labels(binding.metadata.labels.clone()).build(),
            &binding.metadata.resource_version,
            &binding.spec,
        )
        .await
        .expect("skewed opening");
    backend
        .replica_writer::<flotilla_resources::AccountBinding>(flotilla_protocol::NodeId::new(home), NAMESPACE)
        .replace(&bindings.list().await.expect("bindings"), chrono::Utc::now())
        .await
        .expect("replicate binding");
}

/// Freeze one crew aboard Vessel `vessel_ref` (within-convoy name `work`) in convoy
/// `convoy`'s workflow snapshot, as admission does: the crew's frozen skills are what
/// its access layer delivers.
pub(super) async fn freeze_crew(
    backend: &ResourceBackend,
    convoy: &str,
    vessel_ref: &str,
    role: &str,
    adapter: &str,
    skills: Vec<flotilla_resources::SkillCatalogEntry>,
) {
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let created = convoys
        .create(&empty_meta(convoy), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
        .await
        .expect("convoy");
    let crew = CrewSpec::builder()
        .role(role.to_string())
        .skills(flotilla_resources::ResolvedSkills { selected: skills, provenance: Vec::new() })
        .source(CrewSource::Agent {
            selector: Selector { capability: "code".into(), adapter: Some(adapter.into()), model: None },
            prompt: None,
            brief_template: None,
        })
        .build();
    convoys
        .update_status(
            convoy,
            &created.metadata.resource_version,
            &flotilla_resources::ConvoyStatus {
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![crew]).build()],
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("frozen workflow");
    backend
        .using::<Vessel>(NAMESPACE)
        .create(
            &empty_meta(vessel_ref),
            &flotilla_resources::VesselSpec {
                convoy_ref: convoy.into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
}

/// A frozen selection of `name` from the generation's `mattpocock-skills` source.
/// The token prefix a bound Claude launch starts with.
pub(super) fn claude_token_prefix(home: &str) -> String {
    format!("CLAUDE_CODE_OAUTH_TOKEN=\"$(cat -- '{home}/.flotilla-oauth-token')\" ")
}

pub(super) async fn assert_contained_claude_invocation_home(observation: LaunchObservationRecord) {
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
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
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
                    cloud_sandbox: None,
                    provider_address: None,
                    host_direct: None,
                    docker: Some(flotilla_resources::DockerEnvironmentSpec {
                        image_build_ref: None,
                        memory_policy: Default::default(),
                        host_ref: "host-test".into(),
                        image: "contained-image".into(),
                        declared_agent_adapters: BTreeSet::from(["claude-code".into()]),
                        required_agent_adapters: BTreeSet::from(["claude-code".into()]),
                        pull_policy: Default::default(),
                        mounts: Vec::new(),
                        env: BTreeMap::new(),
                    }),
                },
            )
            .await
            .expect("environment");
    }
    // #3907: the requester's bound home, keyed by convoy and role.
    let crew_config_dir = PathBuf::from("/home/crew/flotilla/agent-homes/requesters/demo/coder/claude");
    let runner: Arc<dyn CommandRunner> = Arc::new(CredentialInteriorRunner(
        DiscoveryMockRunner::builder()
            .tool_exists("claude", true)
            .on_run("mkdir", &["-p", &crew_config_dir.to_string_lossy()], Ok(String::new()))
            .build(),
        Some(Arc::new(VesselDisk::new(&crew_config_dir))),
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
        .with_agent_material(Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([] as [(&str, &str); 0])))))
        // #3247: flotillad listens for hooks, but a vessel session keeps command hooks
        // until its loopback forwarder (#3485).
        .with_hook_endpoint(Some(crate::server::HookEndpoint::listening_at("http://127.0.0.1:7878"))),
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
    freeze_crew(&backend, "demo", "demo-work", "coder", "claude-code", Vec::new()).await;
    let terminal_meta = empty_meta("terminal-demo-work-coder");
    if matches!(observation, LaunchObservationRecord::Present) {
        backend.using::<TerminalSession>(NAMESPACE).create(&terminal_meta, &spec).await.expect("terminal context");
    }
    bind_account(&backend, "demo", "coder", "claude-max", "test@example.com").await;
    let tags: [flotilla_terminal::TerminalSessionTag; 0] = [];

    let launched = TerminalControllerRuntime { state }
        .ensure_session("terminal-demo-work-coder", (&spec).into(), &tags)
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
        !launch.env_vars.iter().any(|(name, _)| name == flotilla_agent::CLAUDE_CODE_HOOK_TOKEN_ENV),
        "a vessel session gets no hook token, so its overlay keeps command hooks"
    );
    // #3907: the token reaches the process only from the plan's token file, read as it starts.
    assert!(
        !launch.env_vars.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN"),
        "the session environment carries no token; the plan's file is the only source"
    );
    assert!(launch.env_vars.iter().any(|(name, value)| name == flotilla_agent::account_access::CLAUDE_OAUTH_TOKEN_FILE_ENV
        && value == &crew_config_dir.join(".flotilla-oauth-token").display().to_string()));
    assert!(launch.command.contains(&claude_token_prefix(&crew_config_dir.display().to_string())), "{}", launch.command);
    assert!(
        launch.env_vars.iter().any(|(name, value)| name == "CLAUDE_CONFIG_DIR" && value == crew_config_dir.display().to_string().as_str()),
        "the contained Claude process must receive the config directory owned by its adapter"
    );
    // The first turn includes the assignment and its capability card, never token material.
    assert!(launch.command.contains("Implement the issue."));
    assert!(launch.command.contains("## Your capabilities"));
    if matches!(observation, LaunchObservationRecord::MissingEnvironment) {
        assert!(launch.command.contains("Capabilities are currently unavailable"));
        assert!(launch.command.contains("flotilla crew capabilities"));
    } else {
        assert!(launch.command.contains("Account for `claude:subscription`: credential `claude-max` (account test@example.com)"));
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
