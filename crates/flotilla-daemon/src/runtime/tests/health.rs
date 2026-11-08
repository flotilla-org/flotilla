use super::*;
use crate::runtime::tasks::AGGREGATOR_CONTROLLER;
use flotilla_resources::clone_records::CloneRecords as _;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;

#[tokio::test]
async fn reconcile_now_wakes_convoy_controller_resource() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("teardown"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
        .await
        .expect("create convoy");

    assert_eq!(wake_controller_resource::<Convoy>(&backend, NAMESPACE, "teardown").await.expect("wake convoy"), "Convoy/teardown woken");
    assert_ne!(convoys.get("teardown").await.expect("updated convoy").metadata.resource_version, convoy.metadata.resource_version);
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
    daemon.install_lifecycle_reads(engine.reads.clone()).expect("engine reads");
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
    let convoy = backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("clone-failure-convoy".to_string())
                .labels(BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), "clone-failure-convoy".to_string())]))
                .build(),
            &ConvoySpec::builder()
                .workflow_ref("missing-workflow".to_string())
                .adopted_checkout_refs(BTreeMap::from([(repository_key.clone(), "clone-failure-checkout".to_string())]))
                .build(),
        )
        .await
        .expect("convoy");
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
    // The convoy's checkout, held for its engine Convoy, made from the clone.
    let engine_convoy = flotilla_resources::convoy::engine::engine_name(&convoy).expect("engine convoy name");
    engine.checkouts.open_convoy(&engine_convoy);
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
        Some(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(engine_convoy)),
    );
    let convoy_controller = tokio::spawn(
        ControllerLoop {
            primary: backend.using::<Convoy>(NAMESPACE),
            secondaries: Vec::new(),
            reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(NAMESPACE)),
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let gave_up = matches!(
                engine.clones.get(&launch, None).ok().flatten().map(|clone| clone.reading),
                Some(flotilla_protocol::LifecycleReading::GaveUp { .. })
            );
            let events = backend.using::<flotilla_resources::Event>(NAMESPACE).list().await.expect("events").items;
            if gave_up && events.iter().any(|event| event.spec.reason == "WorkflowTemplateNotFound") {
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
                .action(CommandAction::QueryExplainConvoy {
                    namespace: Some(NAMESPACE.to_string()),
                    name: "clone-failure-convoy".to_string(),
                })
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
    assert!(explanation
        .recent_events
        .iter()
        .any(|event| { event.reason == "WorkflowTemplateNotFound" && event.message.contains("missing-workflow") }));
    convoy_controller.abort();
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

// #2736: sequential admissions to one placement succeed without sleeps,
// even when its local aggregator has not projected any admitted convoys.
#[hegel::test]
fn projection_parity_sequential_admissions(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Cover empty batches, one admission, and batches larger than the
    // reported three-dispatch failure; no controller or projection sleeps.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("daemon.toml"), "machine_id = \"projection-admission-test\"\n").expect("config");
        let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
        let host_id = daemon.local_host_id().expect("host identity").to_string();
        let profile = LocalProvisioningProfile { repo_default_dir: temp.path().display().to_string(), ..manual_profile(&host_id, false) };
        let backend = daemon.resource_backend();
        register_startup(&daemon, NAMESPACE, &profile).await.expect("register placement");
        backend
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("parity-workflow"),
                &WorkflowTemplateSpec::builder()
                    .vessels(vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .crew(vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "true".to_string() })
                            .build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("workflow");
        backend
            .using::<Project>(NAMESPACE)
            .create(
                &empty_meta("parity-project"),
                &flotilla_resources::ProjectSpec::builder()
                    .display_name("Parity".to_string())
                    .default_workflow_ref("parity-workflow".to_string())
                    .build(),
            )
            .await
            .expect("project");
        let projection = AggregatorProjectionState::new();
        let health = RuntimeHealth::default();
        let identity = test_health_identity();
        let clock = flotilla_store_testkit::VirtualClock::new(Utc::now());
        for index in 0..count {
            // Store timestamps come from its real clock; pin the decision
            // clock to the newest write so test scheduling cannot age it.
            if let Some(newest) = backend
                .using::<Convoy>(NAMESPACE)
                .list()
                .await
                .expect("convoys")
                .items
                .iter()
                .map(|convoy| convoy.metadata.creation_timestamp)
                .max()
            {
                clock.set(newest);
            }
            health.report_projection_parity(
                projection_parity_condition(&backend, NAMESPACE, &projection, &clock).await.expect("parity before admission"),
            );
            apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &identity, &health).await.expect("publish readiness");
            let mut events = daemon.subscribe();
            let command_id = daemon
                .execute(
                    Command::builder()
                        .action(CommandAction::ConvoyStart {
                            intent: Box::new(
                                flotilla_protocol::ConvoyStartIntent::builder()
                                    .project_ref("parity-project".to_string())
                                    .name(format!("batch-{index}"))
                                    .branch(format!("test/batch-{index}"))
                                    .placement_policy(format!("host-direct-{host_id}"))
                                    .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                                    .build(),
                            ),
                        })
                        .build(),
                )
                .await
                .expect("dispatch");
            let result = wait_for_command_result(&mut events, command_id).await;
            assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "admission {index}: {result:?}");
            assert_eq!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.len(), index + 1);
        }
        if count > 0 {
            let newest = backend
                .using::<Convoy>(NAMESPACE)
                .list()
                .await
                .expect("convoys")
                .items
                .iter()
                .map(|convoy| convoy.metadata.creation_timestamp)
                .max()
                .expect("admitted rows");
            clock.set(newest + PROJECTION_PARITY_GRACE);
            health.report_projection_parity(
                projection_parity_condition(&backend, NAMESPACE, &projection, &clock).await.expect("expired parity"),
            );
            apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &identity, &health)
                .await
                .expect("publish divergence");
            // Projection lag blocks new admission, while existing convoys still
            // need this live daemon to observe their forge completion evidence.
            let host = backend.using::<Host>(NAMESPACE).get(&host_id).await.expect("host");
            let status = host.status.expect("status");
            assert!(!status.ready, "projection lag must still block admission");
            assert!(status.heartbeat_is_fresh(Utc::now()), "observer loss must not be heartbeat expiry");
            let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "test/repo".into() };
            assert_eq!(
                flotilla_observation::forge_observation::source_owner(&backend, NAMESPACE, &source).await.expect("live forge observer"),
                Some(backend.local_root().expect("local root")),
            );
            let mut events = daemon.subscribe();
            let command_id = daemon
                .execute(
                    Command::builder()
                        .action(CommandAction::ConvoyStart {
                            intent: Box::new(
                                flotilla_protocol::ConvoyStartIntent::builder()
                                    .project_ref("parity-project".to_string())
                                    .name("after-bound".to_string())
                                    .branch("test/after-bound".to_string())
                                    .placement_policy(format!("host-direct-{host_id}"))
                                    .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                                    .build(),
                            ),
                        })
                        .build(),
                )
                .await
                .expect("dispatch with divergence");
            let result = wait_for_command_result(&mut events, command_id).await;
            assert!(
                matches!(result, CommandValue::Error { ref message } if message.contains("LocalRowsMissing")),
                "old missing rows must refuse admission: {result:?}"
            );
            assert_eq!(
                backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.len(),
                count,
                "refusal must not write another convoy"
            );
        }
    });
}

// #2736: only rows younger than ten seconds get grace. The boundary is
// inclusive for divergence, and future timestamps fail closed. Each case
// also checks that catching up clears the diagnosis at that same instant.
#[tokio::test]
async fn projection_parity_creation_age_boundaries() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let created = backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("recent"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
        .await
        .expect("create local convoy");
    for age_ms in [-1, 0, 1, 9_999, 10_000, 10_001, 60_000] {
        let clock = flotilla_store_testkit::VirtualClock::new(created.metadata.creation_timestamp + chrono::Duration::milliseconds(age_ms));
        let projection = AggregatorProjectionState::new();
        let condition = projection_parity_condition(&backend, NAMESPACE, &projection, &clock).await.expect("evaluate missing row");
        assert_eq!(condition.is_some(), !(0..10_000).contains(&age_ms), "row age {age_ms}ms");
        if let Some(condition) = condition {
            assert_eq!(condition.reason, "LocalRowsMissing");
            assert_eq!(condition.message, "durable store has 1 convoys but the local aggregator projection has 0; missing: recent");
        }
        let resource = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", NAMESPACE, "recent");
        projection.write().await.local_rows.insert(
            resource.clone(),
            flotilla_protocol::ConvoyRow::builder()
                .resource(resource)
                .name("recent")
                .workflow_ref("workflow")
                .phase(flotilla_protocol::ConvoyPhase::Pending)
                .build(),
        );
        assert!(
            projection_parity_condition(&backend, NAMESPACE, &projection, &clock).await.expect("evaluate caught-up row").is_none(),
            "caught-up row age {age_ms}ms"
        );
    }
}

#[tokio::test]
async fn projection_parity_reports_and_clears_missing_local_convoys() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let meta = InputMeta::builder().name("convoy-a".to_string()).finalizers(vec!["flotilla.work/test-finalizer".to_string()]).build();
    let created =
        convoys.create(&meta, &ConvoySpec::builder().workflow_ref("workflow".to_string()).build()).await.expect("create durable convoy");
    let failed = convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
        )
        .await
        .expect("mark durable convoy failed");
    convoys.delete(&failed.metadata.name).await.expect("begin durable convoy reaping");
    let projection = AggregatorProjectionState::new();
    let clock = flotilla_store_testkit::VirtualClock::new(created.metadata.creation_timestamp + PROJECTION_PARITY_GRACE);

    let degraded = projection_parity_condition(&backend, NAMESPACE, &projection, &clock)
        .await
        .expect("evaluate parity")
        .expect("missing projection should degrade the host");
    assert_eq!(degraded.condition_type, "ProjectionParity");
    assert_eq!(degraded.reason, "LocalRowsMissing");
    assert!(degraded.message.contains("convoy-a"));

    let resource = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", NAMESPACE, "convoy-a");
    projection.write().await.local_rows.insert(
        resource.clone(),
        flotilla_protocol::ConvoyRow::builder()
            .resource(resource)
            .name("convoy-a")
            .workflow_ref("workflow")
            .phase(flotilla_protocol::ConvoyPhase::Pending)
            .build(),
    );
    assert!(
        projection_parity_condition(&backend, NAMESPACE, &projection, &clock).await.expect("evaluate restored parity").is_none(),
        "restored parity should clear the degraded condition"
    );
}

// Fleet health consumes the persisted Host heartbeat. Exhaustion must be
// visible through that public query, and recovery must remove the diagnosis.
#[tokio::test]
async fn controller_exhaustion_alarm_is_visible_in_fleet_health() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"controller-exhaustion-health-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
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
    let task = spawn_resource_controller::<Checkout, _, _>(
        ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()),
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

// Undecodable old-store records never stop a daemon from booting and serving. Store
// diagnostics are not host health (#3108): they are not read into the Host's conditions.
#[tokio::test]
async fn daemon_boots_and_serves_over_undecodable_old_store_records() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"boot-quarantine-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let sqlite_path = config.state_dir().join("resources.sqlite");
    drop(SqliteBackend::open(&sqlite_path).expect("initialize sqlite store"));
    let connection = rusqlite::Connection::open(&sqlite_path).expect("open raw sqlite store");
    insert_undecodable_resource::<Project>(&connection, "poisoned-project");
    insert_undecodable_resource::<Convoy>(&connection, "poisoned-convoy");
    insert_undecodable_resource::<CredentialGrant>(&connection, "poisoned-credential-grant");
    insert_undecodable_resource::<CredentialSpec>(&connection, "poisoned-credential-spec");
    insert_undecodable_resource::<Host>(&connection, "poisoned-host");
    insert_undecodable_resource::<Vessel>(&connection, "poisoned-vessel");
    insert_undecodable_resource::<TerminalSession>(&connection, "poisoned-terminal-session");
    drop(connection);

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
    .expect("daemon should boot with undecodable stored resources");

    daemon.list_projects_internal().await.expect("daemon should serve project queries");
    daemon.fleet_health_internal().await.expect("daemon should serve fleet health");

    runtime.shutdown();
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

    runtime.shutdown();
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
