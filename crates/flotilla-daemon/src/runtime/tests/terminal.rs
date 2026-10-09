use super::*;

#[test]
fn missing_tool_session_is_stopped_but_a_dead_generation_is_lost() {
    let tool = TerminalSessionSource::Tool { command: "cargo test".into() };
    assert_eq!(terminal_liveness_for_source(&tool, TerminalSessionLiveness::Absent), TerminalLiveness::Stopped);
    assert_eq!(
        terminal_liveness_for_source(&tool, TerminalSessionLiveness::Lost("daemon generation is dead".into())),
        TerminalLiveness::Lost("daemon generation is dead".into())
    );
}

#[tokio::test]
async fn agent_launch_reads_grants_from_its_own_vessel_pin() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let kinds = backend.using::<FulfilmentKind>(NAMESPACE);
    let broad = BTreeSet::from([flotilla_resources::FulfilmentGrant::host_account_reach()]);
    let restricted = BTreeSet::from([flotilla_resources::FulfilmentGrant::platform("linux".to_string())]);
    for (name, grants) in [("broad", broad.clone()), ("restricted", restricted.clone())] {
        kinds
            .create(
                &empty_meta(name),
                &FulfilmentKindSpec {
                    host_ref: "host".to_string(),
                    pool: "test".to_string(),
                    cost_class: Default::default(),
                    grants,
                    realisation: FulfilmentRealisation::HostDirect,
                },
            )
            .await
            .expect("fulfilment kind");
    }
    let decision = |kind: &str| {
        PlacementDecision::builder()
            .policy_name(kind.to_string())
            .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("host".to_string()), display_name: "host".to_string() })
            .build()
    };
    let pins = BTreeMap::from([
        (
            "work".to_string(),
            flotilla_resources::VesselPlacementPin { policy_ref: "snapshot-work".to_string(), decision: decision("broad") },
        ),
        (
            "ops".to_string(),
            flotilla_resources::VesselPlacementPin { policy_ref: "snapshot-ops".to_string(), decision: decision("restricted") },
        ),
    ]);
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &InputMeta::builder()
                .name("demo".to_string())
                .annotations(BTreeMap::from([(
                    flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION.to_string(),
                    serde_json::to_string(&pins).expect("pins serialize"),
                )]))
                .build(),
            &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status(
            "demo",
            &convoy.metadata.resource_version,
            &ConvoyStatus { placement_decision: Some(decision("broad")), ..ConvoyStatus::default() },
        )
        .await
        .expect("convoy status");
    backend
        .using::<Vessel>(NAMESPACE)
        .create(
            &empty_meta("demo-ops"),
            &VesselSpec {
                convoy_ref: "demo".to_string(),
                vessel_name: "ops".to_string(),
                placement_policy_ref: "snapshot-ops".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("ops vessel");
    let context = flotilla_resources::TerminalCrewContext {
        namespace: NAMESPACE.to_string(),
        convoy: "demo".to_string(),
        vessel_ref: "demo-ops".to_string(),
    };
    assert_eq!(fulfilment_grants_for_terminal(&backend, &context).await, Some(restricted));
}

#[tokio::test]
async fn contained_terminal_session_never_falls_back_from_the_requested_interior_pool() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"interior-pool-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    let env_id = EnvironmentId::new("contained-work");
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: env_id.clone(),
        image: ImageId::new("contained-image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        destroyed: Arc::new(AtomicBool::new(false)),
    });
    daemon
        .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), Some(passthrough_registry()))
        .expect("register contained environment");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            passthrough_registry(),
            None,
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )),
    };
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: env_id.to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Tool { command: "sleep infinity".to_string() },
        cwd: "/workspace".to_string(),
        env: Default::default(),
        pool: "cleat".to_string(),
    };

    let error = runtime
        .ensure_session("terminal-contained-work-coder", &spec, &[])
        .await
        .expect_err("a missing interior cleat must fail instead of launching through passthrough");

    assert_eq!(error, "terminal pool cleat unavailable for environment contained-work");
}

#[tokio::test(start_paused = true)]
async fn convoy_holds_and_recovers_when_terminal_pool_is_transiently_unavailable_after_daemon_restart() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"transient-pool-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let backend = ResourceBackend::InMemory(Default::default());
    let pool = Arc::new(TransientTerminalPool::new());

    let initial = daemon_with_transient_terminal_pool(Arc::clone(&config), backend.clone(), Arc::clone(&pool)).await;
    let registry = probe_local_provider_registry(&initial, &config).await.expect("initial provider registry");
    let profile = build_local_profile(&initial, &registry).expect("initial profile");
    register_startup_resources(&initial, NAMESPACE, &profile).await.expect("initial startup resources");

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("transient-pool-convoy"), &ConvoySpec::builder().workflow_ref("transient-pool-workflow".to_string()).build())
        .await
        .expect("create convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() },
        )
        .await
        .expect("mark convoy active");

    let session_name = "terminal-transient-pool-convoy-implement-coder";
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name(session_name.to_string())
            .status(TerminalStatus::Running)
            .command("cargo test".to_string())
            .working_directory(ExecutionEnvironmentPath::new("/workspace"))
            .build()])
        .await;
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let session = sessions
        .create(
            &InputMeta::builder()
                .name(session_name.to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "transient-pool-convoy".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: profile.host_direct_environment_name(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: profile.host_direct_pool.clone(),
            },
        )
        .await
        .expect("create running terminal resource");
    let mut running = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: session_name.to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut running);
    sessions.update_status(session_name, &session.metadata.resource_version, &running).await.expect("mark terminal running");
    drop(initial);

    pool.set_available(false);
    let restarted = daemon_with_transient_terminal_pool(Arc::clone(&config), backend.clone(), Arc::clone(&pool)).await;
    let restarted_registry = probe_local_provider_registry(&restarted, &config).await.expect("restarted provider registry");
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&restarted),
        config,
        restarted_registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: sessions.clone(),
            secondaries: Vec::new(),
            reconciler: TerminalSessionReconciler::new(Arc::new(TerminalControllerRuntime { state }), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );

    for delay in [0, 60, 120, 240, 480] {
        if delay > 0 {
            tokio::time::advance(Duration::from_secs(delay)).await;
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    let held = sessions.get(session_name).await.expect("held terminal").status.expect("held terminal status");
    assert_eq!(held.phase, TerminalSessionPhase::Running);
    assert!(held
        .degraded
        .as_ref()
        .is_some_and(|condition| { condition.consecutive_failures == 5 && condition.message.contains("temporarily unavailable") }));
    assert_eq!(
        convoys.get("transient-pool-convoy").await.expect("held convoy").status.expect("held convoy status").phase,
        ConvoyPhase::Active
    );

    pool.set_available(true);
    tokio::time::advance(Duration::from_secs(15 * 60)).await;
    for _ in 0..40 {
        tokio::task::yield_now().await;
        if sessions.get(session_name).await.ok().and_then(|session| session.status).is_some_and(|status| status.degraded.is_none()) {
            break;
        }
    }

    let recovered = sessions.get(session_name).await.expect("recovered terminal").status.expect("recovered terminal status");
    assert_eq!(recovered.phase, TerminalSessionPhase::Running);
    assert_eq!(recovered.degraded, None);
    assert_eq!(
        convoys.get("transient-pool-convoy").await.expect("recovered convoy").status.expect("recovered convoy status").phase,
        ConvoyPhase::Active
    );

    loop_task.abort();
    let _ = loop_task.await;
}

#[tokio::test]
async fn contained_claude_launch_uses_granted_invocation_environment_without_ambient_config() {
    assert_contained_claude_invocation_home(false, LaunchObservationRecord::Present).await;
}

#[tokio::test]
async fn contained_claude_launch_selects_its_crew_home() {
    // Issue #2672: new environments launch in the role home with the same
    // granted OAuth wiring. The other test covers legacy environments.
    assert_contained_claude_invocation_home(true, LaunchObservationRecord::Present).await;
}

// Advisory observation failures must not discard the inline card or prevent launch.
#[tokio::test]
async fn contained_claude_launch_keeps_inline_card_without_session_observation() {
    assert_contained_claude_invocation_home(false, LaunchObservationRecord::Missing).await;
}

// Capability context and endpoint reads are advisory: the assignment and
// delivered credentials must still reach the harness when those reads fail.
#[tokio::test]
async fn contained_claude_launch_survives_capability_context_read_failures() {
    for observation in [LaunchObservationRecord::MissingConvoy, LaunchObservationRecord::MissingEnvironment] {
        assert_contained_claude_invocation_home(false, observation).await;
    }
}

#[tokio::test]
async fn starting_agent_replaces_a_pool_session_left_by_a_previous_daemon_runtime() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
    let current_socket = temp.path().join("current-daemon.sock");
    daemon.set_daemon_socket_path(current_socket.clone()).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
    let state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        local_registry,
        None,
        profile.host_id.clone(),
        profile.host_direct_environment_name(),
    ));
    let session_name = "terminal-demo-implement-coder";
    pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession {
        session_name: session_name.to_string(),
        status: flotilla_protocol::TerminalStatus::Running,
        command: Some("old codex process with stale crew identity".to_string()),
        working_directory: Some(ExecutionEnvironmentPath::new("/repo")),
        screen_activity: None,
    }])
    .await;
    let runtime = TerminalControllerRuntime { state };
    let session_cwd = temp.path().join("session-cwd");
    std::fs::create_dir_all(&session_cwd).expect("session cwd");
    let durable_checkout = temp.path().join("durable-checkout");
    std::fs::create_dir_all(&durable_checkout).expect("durable checkout dir");
    // Production crew roots are Git checkouts. The exclusion guarantee
    // must also hold when replacing an adopted terminal after restart.
    for root in [&session_cwd, &durable_checkout] {
        assert!(ProcessCommand::new("git").args(["init", "-q"]).current_dir(root).status().expect("git init crew checkout").success());
    }
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: profile.host_direct_environment_name(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Implement the issue.".to_string(),
                copies: vec![durable_checkout.display().to_string()],
            },
            context: Box::new(flotilla_resources::TerminalCrewContext {
                namespace: NAMESPACE.to_string(),
                convoy: "demo".to_string(),
                vessel_ref: "demo-implement".to_string(),
            }),
            message: None,
        },
        cwd: session_cwd.display().to_string(),
        env: BTreeMap::from([
            ("FLOTILLA_DAEMON_SOCKET".into(), "/unrelated-daemon.sock".into()),
            ("DECLARED_VALUE".into(), "hello declared crew".into()),
        ]),
        pool: "fake-terminals".to_string(),
    };

    let launched = runtime.ensure_session(session_name, &spec, &[]).await.expect("replace stale session");

    assert_eq!(pool.killed.lock().await.as_slice(), &[session_name.to_string()]);
    assert_eq!(pool.ensured.lock().await.len(), 1, "the fresh agent command must actually be launched");
    assert!(launched.crew.is_some(), "the replacement gets a fresh crew identity");
    // #2706: restored crews get the current daemon's declared socket and
    // session values, regardless of a stale inherited or supplied socket.
    let ensured = pool.ensured.lock().await;
    assert!(ensured[0]
        .env_vars
        .iter()
        .any(|(key, value)| key == "FLOTILLA_DAEMON_SOCKET" && value == &current_socket.display().to_string()));
    assert!(ensured[0].env_vars.iter().any(|(key, value)| key == "DECLARED_VALUE" && value == "hello declared crew"));
    assert!(!ensured[0].env_vars.iter().any(|(_, value)| value == "/unrelated-daemon.sock"));
    drop(ensured);
    assert_eq!(
        std::fs::read_to_string(durable_checkout.join(".flotilla/briefs/coder.md")).expect("durable brief copy"),
        "Implement the issue."
    );

    runtime.cleanup_session_artifacts(&spec).await.expect("cleanup generated briefs");
    assert!(!session_cwd.join(".flotilla/briefs/coder.md").exists(), "session brief should be removed");
    assert!(!durable_checkout.join(".flotilla/briefs/coder.md").exists(), "durable brief copy should be removed");
    assert!(!durable_checkout.join(".flotilla/briefs").exists(), "empty durable brief directory should be removed");
}

#[tokio::test]
async fn terminal_teardown_kills_a_persisted_session_after_runtime_restart() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            local_registry,
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )),
    };
    let session_name = "terminal-demo-implement-coder";
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: profile.host_direct_environment_name(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/repo".to_string(),
        env: Default::default(),
        pool: "fake-terminals".to_string(),
    };
    pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession::builder()
        .session_name(session_name.to_string())
        .status(TerminalStatus::Running)
        .command("codex".to_string())
        .working_directory(ExecutionEnvironmentPath::new("/repo"))
        .build()])
        .await;

    runtime.kill_session(session_name, &spec).await.expect("teardown should resolve the persisted session pool");

    assert_eq!(pool.killed.lock().await.as_slice(), &[session_name.to_string()]);
}

#[tokio::test]
async fn codex_interactive_prompt_is_observed_as_needing_input() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let (daemon, pool) = crew_daemon(Arc::clone(&config)).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
    let central = temp.path().join(".config/flotilla/credentials/codex-central/auth.json");
    std::fs::create_dir_all(central.parent().expect("central credential directory")).expect("central credential directory");
    std::fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-one\"}}").expect("central auth");
    std::fs::set_permissions(&central, std::fs::Permissions::from_mode(0o600)).expect("protect central auth");
    let material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
        ("HOME", temp.path().to_string_lossy().into_owned()),
        (FLOTILLA_SKILLS_DIR_ENV, write_test_skill_sources(temp.path()).display().to_string()),
    ]))));
    material
        .prepare(&profile.host_direct_environment_name(), &BTreeSet::from(["codex".to_string()]), &BTreeMap::new())
        .await
        .expect("deliver the central credential");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(
            ControllerRuntimeState::new(
                Arc::clone(&daemon),
                config,
                local_registry,
                None,
                profile.host_id.clone(),
                profile.host_direct_environment_name(),
            )
            .with_agent_material(Arc::clone(&material)),
        ),
    };
    let session_name = "terminal-demo-work-coder";
    pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession {
        session_name: session_name.to_string(),
        status: TerminalStatus::Running,
        command: Some("codex".to_string()),
        working_directory: Some(ExecutionEnvironmentPath::new("/workspace")),
        screen_activity: Some(ScreenActivity::Stable),
    }])
    .await;
    pool.set_captured_screen(
        session_name,
        "Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, quit\n\nPress enter to continue",
    )
    .await;
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: profile.host_direct_environment_name(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
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
        cwd: "/workspace".to_string(),
        env: Default::default(),
        pool: "fake-terminals".to_string(),
    };

    let observation = runtime.observe_attention(session_name, &spec).await.expect("observe prompt").expect("attention observation");
    assert_eq!(observation.attention.expect("attention").state, TerminalAttentionState::NeedsInput);

    pool.set_captured_screen(session_name, "› Ask Codex to do anything\n\ngpt-5.6-sol high · /workspace").await;
    let observation =
        runtime.observe_attention(session_name, &spec).await.expect("observe normal composer").expect("attention observation");
    assert_eq!(observation.attention.expect("attention").state, TerminalAttentionState::Idle);

    // #2560: stable pixels alone do not end a Codex turn. An interrupt
    // returning to the composer is a boundary even without a notify hook.
    pool.set_captured_screen(session_name, "• Working (2h 12m • esc to interrupt)\n\n› Ask Codex to do anything").await;
    let observation = runtime.observe_attention(session_name, &spec).await.expect("observe quiet working turn").expect("observation");
    assert_eq!(observation.attention.expect("attention").state, TerminalAttentionState::Working);
    pool.set_captured_screen(session_name, "■ Conversation interrupted\n\n› Ask Codex to do anything").await;
    let observation = runtime.observe_attention(session_name, &spec).await.expect("observe interrupted prompt").expect("observation");
    assert_eq!(observation.attention.expect("attention").state, TerminalAttentionState::Idle);
    pool.set_captured_screen(session_name, "Loading unknown screen").await;
    let observation = runtime.observe_attention(session_name, &spec).await.expect("observe unknown screen").expect("observation");
    assert_eq!(observation.attention.expect("attention").state, TerminalAttentionState::Unobservable);

    pool.set_captured_screen(
        session_name,
        "codex_apps failed: HTTP 401 token_expired\nYour access token could not be refreshed. Please log out and sign in again.",
    )
    .await;
    let failure = runtime.observe_failure(session_name, &spec).await.expect("observe auth failure").expect("fatal auth failure");
    assert!(failure.contains("token_expired"));
    assert!(
        failure.contains(&central.display().to_string()),
        "a static credential names the central login an operator must re-authenticate: {failure}"
    );
    material
        .prepare("replacement-environment", &BTreeSet::from(["codex".to_string()]), &BTreeMap::new())
        .await
        .expect("a failed crew never blocks another crew from the same central login");
}

#[tokio::test]
async fn terminal_teardown_skips_a_session_absent_from_the_pool() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config");
    std::fs::create_dir_all(&config_path).expect("config dir");
    std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_path));
    let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
    let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
    let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            local_registry,
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )),
    };
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: profile.host_direct_environment_name(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/repo".to_string(),
        env: Default::default(),
        pool: "fake-terminals".to_string(),
    };

    runtime.kill_session("terminal-demo-implement-coder", &spec).await.expect("missing sessions should be idempotent");

    assert!(pool.killed.lock().await.is_empty(), "teardown must not invoke the pool for an already-gone session");
}
