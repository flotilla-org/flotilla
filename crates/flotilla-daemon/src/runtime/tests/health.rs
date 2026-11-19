use super::*;
use crate::runtime::tasks::AGGREGATOR_CONTROLLER;
use crate::testkits::engine::{admit_convoy, engine};
use flotilla_engine_convoys_testkit::single_agent;
use flotilla_resources::clone_records::CloneRecords as _;

// Behaviour (#3108): reconcile-now on a convoy queues its launch at the Convoy controller and
// writes nothing; a launch this home does not hold is refused as not found.
#[tokio::test]
async fn reconcile_now_queues_the_launch_at_the_convoy_controller() {
    use flotilla_executor::executor_actions::ExecutorActionPort;

    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("config directory");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"reconcile-now-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(config_base))).await;
    engine(&daemon);
    let launch = admit_convoy(
        &daemon,
        flotilla_engine_convoys_testkit::convoy(
            NAMESPACE,
            "teardown",
            ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("workflow".to_string()).build(),
            single_agent("coder"),
        ),
    )
    .await;
    let convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    let admitted = convoys.get(&launch).await.expect("admitted convoy");
    let (sender, mut queue) = flotilla_store::controller::WorkQueueSender::channel();
    let _watch = tokio::spawn(daemon.convoy_wake_watch().spawn(daemon.resource_backend(), NAMESPACE.to_owned(), sender));

    assert_eq!(daemon.reconcile_now(NAMESPACE, "Convoy", &launch).await.expect("wake convoy"), format!("Convoy/{launch} woken"));
    let queued = tokio::time::timeout(Duration::from_secs(5), queue.recv()).await.expect("queued in time").expect("queue open");
    assert_eq!(queued, launch.to_string());
    assert_eq!(convoys.get(&launch).await.expect("convoy").metadata.resource_version, admitted.metadata.resource_version);

    let missing = daemon.reconcile_now(NAMESPACE, "Convoy", "never-admitted").await.expect_err("an unknown launch");
    assert!(missing.contains("not found"), "{missing}");
    assert!(queue.try_recv().is_err(), "a refused wake queues nothing");
}

/// Refuses every clone as a forge that has no such repository.
struct MissingRepositoryCloneRuntime;

#[async_trait]
impl CloneRuntime for MissingRepositoryCloneRuntime {
    async fn clone_and_inspect(&self, _: &str, _: &str) -> Result<Option<String>, String> {
        Err("authentication failed: repository not found".into())
    }

    async fn inspect_existing(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

// Behaviour (#3108): a clone the Clone controller gives up on shows its cause in convoy explain,
// on the checkout made from it, beside the convoy controller's own events.
#[tokio::test]
async fn clone_controller_failure_is_visible_in_convoy_explain() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"clone-failure-explain-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let engine = flotilla_provisioning_testkit::clones::RunningClones::start("local", Arc::new(MissingRepositoryCloneRuntime));
    // The clones' readings, with the vessels explain reads beside them, on the same store.
    let vessels = flotilla_provisioning_testkit::VesselsFixture::over_service(&engine.service);
    let reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> = Arc::new(
        flotilla_provisioning::engine::reads::LifecycleReadPort::new(
            flotilla_provisioning::engine::reads::StoreReads::new(engine.service.store()),
            "local".to_owned(),
        )
        .with_environments(Arc::clone(&engine.environments.environments) as _)
        .with_clones(Arc::clone(&engine.clones) as _)
        .with_checkouts(Arc::clone(&engine.checkouts.checkouts) as _)
        .with_terminals(Arc::clone(&engine.terminals.terminals) as _)
        .with_vessels(Arc::clone(&vessels.vessels) as _),
    );
    daemon.install_lifecycle_reads(Arc::clone(&reads)).expect("engine reads");
    // Explain reads the convoy's messages too.
    let clone_path = temp.path().join("clone").display().to_string();
    engine.environments.put_ready(
        "host-direct-test",
        &EnvironmentSpec {
            cloud_sandbox: None,
            provider_address: None,
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: clone_path.clone() }),
            docker: None,
        },
        None,
        &clone_path,
        Default::default(),
    );
    let backend = daemon.resource_backend();
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.using::<Repository>(NAMESPACE), &repository_key, &repository_spec)
        .await
        .expect("repository");
    // The convoy, admitted with its adopted checkout.
    let status = ConvoyStatus {
        observed_workflow_ref: Some("missing-workflow".into()),
        workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: vec![VesselRequirement::builder()
                .name("work".into())
                .repository_refs(vec![repository_key.clone()])
                .crew(vec![CrewSpec::builder()
                    .role("shell".into())
                    .promises(vec![flotilla_resources::TemplatePromise::decision_ledger("shell")])
                    .source(CrewSource::Tool { command: "true".into() })
                    .build()])
                .build()],
        }),
        ..Default::default()
    };
    let spec = ConvoySpec::builder()
        .project_ref(NAMESPACE.into())
        .workflow_ref("missing-workflow".to_string())
        .adopted_checkout_refs(BTreeMap::from([(repository_key.clone(), "clone-failure-checkout".to_string())]))
        .build();
    // The convoys are on the clones' store too, so the checkout's holder is open where it is held.
    let convoys = flotilla_engine_convoys_testkit::EngineConvoysHome::over(
        &engine.service,
        NAMESPACE,
        &backend,
        Arc::new(flotilla_resources::SystemClock),
        Arc::clone(&reads),
    );
    let convoy = convoys.launch(flotilla_engine_convoys_testkit::convoy(NAMESPACE, "clone-failure-convoy", spec, status)).await;
    let launch = engine
        .clones
        .request(
            &repository_spec,
            &CloneSpec {
                repo_ref: repository_key.clone(),
                url: "https://github.com/flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-test".to_string(),
                path: clone_path.clone(),
            },
            false,
        )
        .expect("clone request");
    // The convoy's checkout, held for its launch, made from the clone.
    engine.checkouts.request(
        "clone-failure-checkout",
        &CheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
            repo_ref: repository_key,
            env_ref: "host-direct-test".to_string(),
            r#ref: "main".to_string(),
            base_ref: None,
            target_path: temp.path().join("checkout").display().to_string(),
            clone_ref: launch.clone(),
        }),
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy.clone())),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                engine.clones.get(&launch, None).ok().flatten().map(|clone| clone.reading),
                Some(flotilla_protocol::LifecycleReading::GaveUp { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("clone failure should reach explain");
    let result = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some(NAMESPACE.to_string()), name: convoy.clone() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain convoy");
    let CommandValue::ConvoyExplanation(explanation) = result else {
        panic!("explain should return a convoy explanation, got {result:?}");
    };
    let checkout = explanation.checkouts.iter().find(|checkout| checkout.name == "clone-failure-checkout").expect("explained checkout");
    let clone = checkout.clone.as_ref().expect("the checkout's clone is explained");
    assert_eq!(clone.name, launch);
    assert!(
        matches!(&clone.reading, flotilla_protocol::LifecycleReading::GaveUp { cause } if cause.contains("repository not found")),
        "explain shows the clone's cause: {:?}",
        clone.reading
    );
}

#[tokio::test]
async fn fleet_health_surfaces_an_expired_ambient_claude_login_without_any_crew_dispatched() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create claude dir");
    std::fs::write(
        home.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-material","expiresAt":1577836800000,"refreshTokenExpiresAt":1580515200000}}"#,
    )
    .expect("write expired ambient credentials");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("config directory");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"expired-ambient-claude-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    engine(&daemon);
    let credential_store = CredentialStore::new(
        daemon.resource_backend(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("HOME", home.display().to_string())])),
        EnvironmentBag::new(),
        Arc::new(ProcessCommandRunner),
        config.state_dir().as_path().to_path_buf(),
    );
    apply_host_heartbeat_with_credentials(
        &daemon,
        NAMESPACE,
        &own_host_profile(&daemon, false),
        Some(&credential_store),
        &test_health_identity(),
        &RuntimeHealth::default(),
    )
    .await
    .expect("publish heartbeat");

    let health = daemon.fleet_health_internal().await.expect("fleet health");
    let local = health.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    assert_eq!(
        local.credential_attention,
        vec![flotilla_protocol::CredentialAttention {
            severity: flotilla_protocol::CredentialAttentionSeverity::Expired,
            message: "ambient claude login expired on 2020-02-01".to_string(),
        }]
    );
}

// Fleet health consumes the persisted Host heartbeat. Exhaustion must be
// visible through that public query, and recovery must remove the diagnosis.
#[tokio::test]
async fn controller_exhaustion_alarm_is_visible_in_fleet_health() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"controller-exhaustion-health-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    engine(&daemon);
    let profile = own_host_profile(&daemon, false);
    let health = RuntimeHealth::default();
    health.report_restart_budget_exhausted(RestartBudgetExhausted {
        controller: "Convoy",
        error: ResourceError::other("read deadline exceeded"),
        attempts: 10,
    });
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &test_health_identity(), &health)
        .await
        .expect("publish degraded heartbeat");
    let fleet = daemon.fleet_health_internal().await.expect("fleet health");
    let local = fleet.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    assert!(local.degraded_conditions.iter().any(|condition| condition.contains("Convoy") && condition.contains("read deadline exceeded")));
    health.failures.lock().expect("health lock").remove("Controller/Convoy");
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &test_health_identity(), &health)
        .await
        .expect("publish recovered heartbeat");
    let fleet = daemon.fleet_health_internal().await.expect("recovered fleet health");
    let local = fleet.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    assert!(!local.degraded_conditions.iter().any(|condition| condition.contains("Convoy")));
}

// Exhaustion remains visible during long backoff, retries automatically,
// and clears only after the restarted controller survives the health window.
#[tokio::test(start_paused = true)]
async fn resource_controller_restart_budget_exhaustion_recovers() {
    let runtime_health = RuntimeHealth::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let (recovered, recovery) = tokio::sync::oneshot::channel();
    let signal = Arc::new(StdMutex::new(Some(recovered)));
    let task = spawn_named_controller(
        Checkout::API_PATHS.kind,
        flotilla_store_memory::engine_home_backend(),
        NAMESPACE.to_string(),
        ControllerSupervision {
            max_consecutive_failures: 1,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            success_reset_after: Duration::from_millis(20),
            recovery_backoff: Duration::from_millis(100),
        },
        runtime_health.clone(),
        {
            let attempts = Arc::clone(&attempts);
            move |_, _| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let signal = Arc::clone(&signal);
                async move {
                    if attempt == 0 {
                        Err(ResourceError::other("pending privacy prompt"))
                    } else {
                        if let Some(signal) = signal.lock().expect("signal lock").take() {
                            let _ = signal.send(());
                        }
                        std::future::pending().await
                    }
                }
            }
        },
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    let conditions = runtime_health.conditions().await;
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].condition_type, format!("Controller/{}", Checkout::API_PATHS.kind));
    assert_eq!(conditions[0].reason, "RestartBudgetExhausted");
    assert!(conditions[0].message.contains("pending privacy prompt"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "long backoff prevents a busy restart loop");
    let retry = tokio::time::timeout(Duration::from_secs(1), recovery).await;
    if retry.is_err() {
        task.abort();
    }
    retry.expect("retry without daemon restart").expect("recovery signal");
    assert!(!runtime_health.conditions().await.is_empty(), "starting a run does not prove recovery");
    tokio::time::sleep(Duration::from_millis(40)).await;
    task.abort();
    assert!(runtime_health.conditions().await.is_empty(), "healthy run clears alarm");
}

// #4310: an overrunning standing convoy pass is reported, but never cut short: a pass dropped
// between admitting a generation and recording it would lose that generation's failure.
#[tokio::test(start_paused = true)]
async fn an_overrunning_pass_is_reported_and_still_runs_to_completion() {
    let timed_out = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let output = run_watched_operation(
        Duration::from_millis(20),
        {
            let (timed_out, finished) = (Arc::clone(&timed_out), Arc::clone(&finished));
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(timed_out.load(Ordering::SeqCst), "the overrun is reported while the pass still runs");
                finished.store(true, Ordering::SeqCst);
                42
            }
        },
        {
            let timed_out = Arc::clone(&timed_out);
            move || timed_out.store(true, Ordering::SeqCst)
        },
    )
    .await;
    assert_eq!(output, 42);
    assert!(finished.load(Ordering::SeqCst), "the overrunning pass ran to its end");

    let prompt = run_watched_operation(Duration::from_millis(20), async { 7 }, || panic!("a prompt pass is not reported")).await;
    assert_eq!(prompt, 7);
}

#[tokio::test]
async fn convoy_ensure_timeout_degrades_runtime_health_until_recovery() {
    let runtime_health = RuntimeHealth::default();
    runtime_health.report_convoy_ensure_timeout(Duration::from_secs(60));

    let conditions = runtime_health.conditions().await;
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].condition_type, CONVOY_ENSURE_CONDITION_TYPE);
    assert_eq!(conditions[0].reason, "ReconciliationPassTimedOut");

    runtime_health.clear_convoy_ensure_timeout();
    assert!(runtime_health.conditions().await.is_empty(), "a completed pass should restore fleet health");
}

#[tokio::test]
async fn issue_budget_backoff_surfaces_a_nonblocking_host_condition() {
    let runtime_health = RuntimeHealth::default();
    let reset = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    runtime_health.issue_polling.note(&format!("github rate limited (budget=REST search, identity=host gh login, reset_at={reset})"));
    assert!(runtime_health.conditions().await.is_empty(), "search has a separate rate limit");
    runtime_health
        .issue_polling
        .note(&format!("github rate limited (budget=REST core remaining 75, identity=host gh login, reset_at={reset})"));

    let conditions = runtime_health.conditions().await;
    let condition = conditions.iter().find(|condition| condition.condition_type == "Forge/GitHubRateBudget").expect("host condition");
    assert_eq!(condition.reason, "LowRemainingBudget");
    assert!(!condition.blocks_readiness);
    assert!(condition.message.contains(&reset));
}

#[tokio::test]
async fn daemon_reports_recent_abnormal_restart_frequency_in_fleet_health() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"abnormal-restart-frequency-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let now = Utc::now();
    fs::write(
        config.state_dir().join("flotillad-abnormal-exits.json"),
        serde_json::to_vec(&json!({
            "abnormal_exits": [
                now - chrono::Duration::minutes(3),
                now - chrono::Duration::minutes(8),
                now - chrono::Duration::minutes(21),
                now - chrono::Duration::minutes(45),
            ]
        }))
        .expect("serialize restart history"),
    )
    .expect("seed restart history");

    let daemon = sqlite_daemon(Vec::new(), Arc::clone(&config)).await;
    let runtime = DaemonRuntime::start_with_options(
        Arc::clone(&daemon),
        config,
        None,
        RuntimeOptions {
            heartbeat_interval: Duration::from_secs(300),
            controller_resync_interval: Duration::from_secs(300),
            start_controllers: false,
            ..RuntimeOptions::default()
        },
    )
    .await
    .expect("daemon should start");

    // The restart window is a diagnostic condition, which the engine Host does not keep
    // (`host_import.rs`), so fleet health no longer carries it; placement is what it must
    // leave alone.
    let host_id = daemon.local_host_id().expect("local Host identity");
    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id.as_str()).await.expect("local Host");
    assert!(host.status.expect("local Host status").ready, "historical abnormal exits must not block placement after recovery");

    runtime.shutdown().await;
}

// Losing an agent adapter blocks placement, while the same live daemon can
// still observe forges and finish the work already admitted.
#[tokio::test]
async fn capability_regression_keeps_live_forge_observer() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"capability-observer-test\"\n").expect("config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host").to_string();
    let health = RuntimeHealth::default();
    let previous = HostStatus { agent_adapter_baseline: Some(["codex".to_string()].into()), ..Default::default() };
    let regression =
        assess_agent_adapter_capabilities(Some(&previous), &BTreeSet::new(), &test_health_identity()).regression.expect("regression");
    health.report_capability_regression(regression);
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &manual_profile(&host_id, false), None, &test_health_identity(), &health)
        .await
        .expect("heartbeat");
    let backend = daemon.resource_backend();
    let status = backend.using::<Host>(NAMESPACE).get(&host_id).await.expect("host").status.expect("status");
    assert!(!status.ready);
    assert!(status.heartbeat_is_fresh(Utc::now()));
    let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "test/repo".into() };
    assert_eq!(
        flotilla_observation::forge_observation::source_owner(&backend, NAMESPACE, &source).await.expect("live forge observer"),
        Some(backend.local_root().expect("root"))
    );
    // Exhausted forge-refresh scheduling explicitly withdraws an otherwise
    // live daemon; unrelated adapter faults alone do not do so.
    health.report_restart_budget_exhausted(RestartBudgetExhausted {
        controller: AGGREGATOR_CONTROLLER,
        error: ResourceError::other("refresh loop failed"),
        attempts: 10,
    });
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &manual_profile(&host_id, false), None, &test_health_identity(), &health)
        .await
        .expect("heartbeat with observation fault");
    assert_eq!(
        flotilla_observation::forge_observation::source_owner(&backend, NAMESPACE, &source).await.unwrap_err(),
        flotilla_observation::forge_observation::NO_READY_FORGE_OBSERVER
    );
}
