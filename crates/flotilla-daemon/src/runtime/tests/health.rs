use super::*;

#[tokio::test]
async fn reconcile_now_wakes_convoy_controller_resource() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("teardown"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
        .await
        .expect("create convoy");

    assert_eq!(wake_controller_resource::<Convoy>(&backend, NAMESPACE, "teardown").await.expect("wake convoy"), "Convoy/teardown woken");
    assert_ne!(convoys.get("teardown").await.expect("updated convoy").metadata.resource_version, convoy.metadata.resource_version);
}

#[tokio::test]
async fn connected_peer_with_replicable_resources_and_zero_cursors_is_degraded() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("feta");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"feta-degraded-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let feta = in_memory_daemon(Vec::new(), config).await;
    let kiwi_node = NodeId::new("kiwi-root");
    let kiwi = NodeInfo::new(kiwi_node.clone(), "kiwi");
    feta.set_configured_peers(vec![kiwi.clone()]).await;
    feta.publish_peer_summary(
        HostSummary::builder()
            .environment_id(EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("kiwi-host")))
            .host_name(flotilla_protocol::HostName::new("kiwi"))
            .node(kiwi.clone())
            .system(flotilla_protocol::SystemInfo::default())
            .providers(Vec::new())
            .build(),
    )
    .await;
    feta.publish_peer_connection_status(&kiwi, PeerConnectionState::Connected).await;

    let kiwi_store = ResourceBackend::InMemory(Default::default());
    let kiwi_hosts = kiwi_store.using::<Host>(NAMESPACE);
    kiwi_hosts
        .create(
            &empty_meta("kiwi-host"),
            &HostSpec { display_name: "kiwi".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("kiwi holds a replicable Host");

    let degraded = resource_replication_content_condition(&feta, NAMESPACE)
        .await
        .expect("diagnose empty follower replica store")
        .expect("zero cursors must be degraded");
    assert_eq!(degraded.condition_type, "ResourceReplication");
    assert_eq!(degraded.reason, "ReplicaCursorsMissing");
    assert!(degraded.message.contains("zero replica cursors"));

    let feta_host_id = feta.local_host_id().expect("feta Host identity").to_string();
    let profile = manual_profile(&feta_host_id, false);
    ensure_host_exists(&feta.resource_backend(), NAMESPACE, &feta_host_id, "feta").await.expect("register feta Host");
    apply_host_heartbeat(&feta, NAMESPACE, &profile, &test_health_identity()).await.expect("publish degraded heartbeat");
    let fleet = feta.fleet_health_internal().await.expect("feta fleet health");
    let feta_row = fleet.hosts.iter().find(|host| host.is_local).expect("local feta fleet row");
    assert!(
        feta_row.degraded_conditions.iter().any(|condition| condition.contains("zero replica cursors")),
        "fleet diagnosis must expose the empty replica store: {:?}",
        feta_row.degraded_conditions
    );

    feta.resource_backend()
        .replica_writer::<Host>(kiwi_node, NAMESPACE)
        .replace(&kiwi_hosts.list().await.expect("list kiwi Hosts"), Utc::now())
        .await
        .expect("bootstrap one replica cursor");

    assert!(
        resource_replication_content_condition(&feta, NAMESPACE).await.expect("diagnose bootstrapped follower").is_none(),
        "the zero-cursor diagnosis must clear once resource replication bootstraps"
    );
    apply_host_heartbeat(&feta, NAMESPACE, &profile, &test_health_identity()).await.expect("publish healthy heartbeat");
    let fleet = feta.fleet_health_internal().await.expect("healthy feta fleet");
    let feta_row = fleet.hosts.iter().find(|host| host.is_local).expect("local feta fleet row");
    assert!(
        feta_row.degraded_conditions.iter().all(|condition| !condition.contains("replica cursors")),
        "fleet diagnosis must clear after bootstrap: {:?}",
        feta_row.degraded_conditions
    );
}

#[tokio::test]
async fn home_bound_authorship_collision_is_advisory_and_clears_after_removal() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("local");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"collision-local-root\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(config_base))).await;
    let local_root = daemon.node_id().clone();
    let remote_root = NodeId::new("collision-remote-root");
    let local_convoys = daemon.resource_backend().using::<Convoy>(NAMESPACE);
    local_convoys
        .create(&empty_meta("command-builder"), &ConvoySpec::builder().workflow_ref("scratch".to_string()).build())
        .await
        .expect("create local authored convoy");
    let remote_backend = ResourceBackend::InMemory(Default::default()).with_local_root(remote_root.clone());
    let remote_convoys = remote_backend.using::<Convoy>(NAMESPACE);
    remote_convoys
        .create(&empty_meta("command-builder"), &ConvoySpec::builder().workflow_ref("scratch".to_string()).build())
        .await
        .expect("create remote authored convoy");
    let writer = daemon.resource_backend().replica_writer::<Convoy>(remote_root.clone(), NAMESPACE);
    writer.replace(&remote_convoys.list().await.expect("list remote convoys"), Utc::now()).await.expect("inject colliding replica");

    let collision = resource_authorship_collision_condition(&daemon, NAMESPACE)
        .await
        .expect("inspect collision")
        .expect("collision must degrade the host");
    assert_eq!(collision.condition_type, "ResourceReplication/AuthorshipCollision");
    assert_eq!(collision.reason, "HomeBoundRecordAuthoredAtMultipleRoots");
    assert!(collision.message.contains("Convoy/flotilla/command-builder"), "unexpected diagnosis: {}", collision.message);
    assert!(collision.message.contains(local_root.as_str()), "local root missing from diagnosis: {}", collision.message);
    assert!(collision.message.contains(remote_root.as_str()), "remote root missing from diagnosis: {}", collision.message);
    assert!(
        collision.message.contains("natural home") && collision.message.contains("delete the other authored copy"),
        "remediation direction missing from diagnosis: {}",
        collision.message
    );

    let host_id = daemon.local_host_id().expect("local Host identity").to_string();
    let profile = manual_profile(&host_id, false);
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "local").await.expect("register local Host");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("publish degraded heartbeat");
    let degraded = daemon.resource_backend().using::<Host>(NAMESPACE).get(&host_id).await.expect("read degraded Host");
    let status = degraded.status.expect("degraded Host status");
    assert!(status.ready, "an authorship collision must not make the host unavailable for placement");
    assert!(status.conditions.iter().any(|condition| {
        condition.condition_type == "ResourceReplication/AuthorshipCollision"
            && condition.message.contains("Convoy/flotilla/command-builder")
    }));

    writer
        .replace(&ResourceList { items: Vec::new(), resource_version: "2".to_string(), generation: None }, Utc::now())
        .await
        .expect("remove colliding replica");
    assert!(
        resource_authorship_collision_condition(&daemon, NAMESPACE).await.expect("inspect cleared collision").is_none(),
        "removing the replica must clear the diagnosis without restart"
    );
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("publish recovered heartbeat");
    let recovered = daemon.resource_backend().using::<Host>(NAMESPACE).get(&host_id).await.expect("read recovered Host");
    assert!(recovered
        .status
        .expect("recovered Host status")
        .conditions
        .iter()
        .all(|condition| { condition.condition_type != "ResourceReplication/AuthorshipCollision" }));
}

#[tokio::test]
async fn field_ownership_violation_is_exposed_in_fleet_diagnosis() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"field-ownership-violation-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);
    register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("register resources");

    let policies = daemon.resource_backend().using::<PlacementPolicy>(NAMESPACE);
    let policy = policies.get(&profile.host_direct_policy_name()).await.expect("registered policy");
    policies
        .write_spec(
            &flotilla_resources::WriterIdentity::operator(),
            &InputMeta::from(&policy.metadata),
            &policy.metadata.resource_version,
            &PlacementPolicySpec::builder()
                .pool("operator-attempt".to_string())
                .priority(8)
                .host_direct(policy.spec.host_direct.clone().expect("host-direct policy"))
                .build(),
        )
        .await
        .expect("observe-mode operator write");

    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("publish heartbeat");
    let fleet = daemon.fleet_health_internal().await.expect("fleet diagnosis");
    let local = fleet.hosts.iter().find(|host| host.is_local).expect("local host");
    let diagnosis = local.degraded_conditions.join("; ");
    assert!(diagnosis.contains("ResourceStore/FieldOwnership"), "{diagnosis}");
    assert!(diagnosis.contains("spec.pool"), "{diagnosis}");
    assert!(diagnosis.contains("Operator"), "{diagnosis}");
}

#[tokio::test]
async fn clone_controller_failure_is_visible_in_convoy_explain() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"clone-failure-explain-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let backend = daemon.resource_backend();
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.using::<Repository>(NAMESPACE), &repository_key, &repository_spec)
        .await
        .expect("repository");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("clone-failure-convoy".to_string())
                .labels(BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), "clone-failure-convoy".to_string())]))
                .build(),
            &ConvoySpec::builder().workflow_ref("missing-workflow".to_string()).build(),
        )
        .await
        .expect("convoy");
    backend
        .using::<Clone>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("incorrect-clone-name".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "clone-failure-convoy".to_string())]))
                .build(),
            &CloneSpec {
                repo_ref: repository_key,
                url: "https://github.com/flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-test".to_string(),
                path: temp.path().join("clone").display().to_string(),
            },
        )
        .await
        .expect("clone intent");

    let controller = tokio::spawn(
        ControllerLoop {
            primary: backend.using::<Clone>(NAMESPACE),
            secondaries: Vec::new(),
            reconciler: CloneReconciler::new(
                Arc::new(CloneControllerRuntime::new(
                    Arc::new(ProcessCommandRunner),
                    Some(test_controller_vcs(Arc::new(ProcessCommandRunner), &temp.path().join("clone").to_string_lossy())),
                    Arc::new(CloneFlights::default()),
                    vec![],
                )),
                backend.using::<Repository>(NAMESPACE),
            ),
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
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

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let events = backend.using::<flotilla_resources::Event>(NAMESPACE).list().await.expect("events").items;
            if events.iter().any(|event| event.spec.reason == "CloneFailed")
                && events.iter().any(|event| event.spec.reason == "WorkflowTemplateNotFound")
            {
                break;
            }
            tokio::task::yield_now().await;
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
    assert!(explanation
        .recent_events
        .iter()
        .any(|event| { event.reason == "CloneFailed" && event.message.contains("clone name mismatch") }));
    assert!(explanation
        .recent_events
        .iter()
        .any(|event| { event.reason == "WorkflowTemplateNotFound" && event.message.contains("missing-workflow") }));
    controller.abort();
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
    let host_id = daemon.local_host_id().expect("local host id").to_string();
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
        &manual_profile(&host_id, false),
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
        register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("register placement");
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
    let backend = ResourceBackend::InMemory(Default::default());
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
    let backend = ResourceBackend::InMemory(Default::default());
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
    let host_id = daemon.local_host_id().expect("local host").to_string();
    let profile = manual_profile(&host_id, false);
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
        ResourceBackend::InMemory(Default::default()),
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

#[tokio::test]
async fn bounded_operation_releases_the_loop_after_a_blocked_pass() {
    let timed_out = Arc::new(AtomicBool::new(false));
    let first = run_bounded_operation(Duration::from_millis(20), std::future::pending::<()>(), {
        let timed_out = Arc::clone(&timed_out);
        move || timed_out.store(true, Ordering::SeqCst)
    })
    .await;
    assert!(first.is_none());
    assert!(timed_out.load(Ordering::SeqCst));

    let second = run_bounded_operation(Duration::from_millis(20), async { 42 }, || panic!("completed pass must not time out")).await;
    assert_eq!(second, Some(42), "a subsequent pass must run after the blocked pass times out");
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

#[test]
fn fleet_diagnosis_surfaces_event_decode_quarantines() {
    let mut diagnostics = flotilla_resources::ResourceStoreDiagnostics::default();
    diagnostics.event_decode_quarantines.push(
        flotilla_resources::ResourceEventDecodeQuarantine::builder()
            .kind("CredentialSpec".to_string())
            .namespace("flotilla".to_string())
            .name("forgejo-token".to_string())
            .event_version(17)
            .error("missing field `username`".to_string())
            .quarantined_at(Utc::now())
            .build(),
    );

    let condition = resource_decode_quarantine_condition(Some(&diagnostics)).expect("quarantine should degrade fleet health");
    assert_eq!(condition.condition_type, "ResourceStore/DecodeQuarantine");
    assert_eq!(condition.reason, "StoredEventDecodeFailed");
    assert!(condition.message.contains("CredentialSpec/forgejo-token@17"), "unexpected diagnosis: {}", condition.message);
    assert!(condition.message.contains("missing field `username`"), "unexpected diagnosis: {}", condition.message);
}

#[test]
fn field_ownership_violation_degrades_without_blocking_admission() {
    let now = Utc::now();
    let mut diagnostics = flotilla_resources::ResourceStoreDiagnostics::default();
    diagnostics.field_ownership_violations.push(
        flotilla_resources::FieldOwnershipViolation::builder()
            .kind("PlacementPolicy".to_string())
            .namespace("flotilla".to_string())
            .name("docker".to_string())
            .writer(flotilla_resources::WriterIdentity::operator())
            .field("spec.docker_per_vessel.host_ref".to_string())
            .attempted_value(serde_json::json!("other-host"))
            .rule("spec.docker_per_vessel.host_ref is owned by ReconcileLoop".to_string())
            .observed_at(now)
            .build(),
    );
    let condition = resource_field_ownership_condition(Some(&diagnostics)).expect("violation must be diagnosed");
    assert_eq!(condition.value, ConditionValue::False);
    assert!(!condition.blocks_readiness(), "historical rejected writes must not block admission");
}

#[tokio::test]
async fn daemon_boot_quarantines_every_undecodable_replicated_kind_and_reports_them() {
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
    let health = daemon.fleet_health_internal().await.expect("daemon should serve fleet health");
    let local = health.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    let diagnosis = local.degraded_conditions.join("; ");
    for identity in [
        "Project/poisoned-project",
        "Convoy/poisoned-convoy",
        "CredentialGrant/poisoned-credential-grant",
        "CredentialSpec/poisoned-credential-spec",
        "Host/poisoned-host",
        "Vessel/poisoned-vessel",
        "TerminalSession/poisoned-terminal-session",
    ] {
        assert!(diagnosis.contains(identity), "fleet diagnosis should name {identity}: {diagnosis}");
    }

    runtime.shutdown();
}

#[tokio::test]
async fn deleting_the_last_quarantined_object_recovers_host_readiness_without_restart() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"quarantine-recovery-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let sqlite_path = config.state_dir().join("resources.sqlite");
    drop(SqliteBackend::open(&sqlite_path).expect("initialize sqlite store"));
    let connection = rusqlite::Connection::open(&sqlite_path).expect("open raw sqlite store");
    insert_undecodable_resource::<ConvoyEnsure>(&connection, "governor");
    drop(connection);

    let daemon = sqlite_daemon(Vec::new(), Arc::clone(&config)).await;
    let runtime = DaemonRuntime::start_with_options(
        Arc::clone(&daemon),
        config,
        None,
        RuntimeOptions {
            heartbeat_interval: Duration::from_millis(10),
            controller_resync_interval: Duration::from_secs(300),
            start_controllers: false,
            ..RuntimeOptions::default()
        },
    )
    .await
    .expect("daemon should boot with an undecodable stored resource");

    let health = daemon.fleet_health_internal().await.expect("daemon should serve fleet health");
    let local = health.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    assert!(
        local.degraded_conditions.iter().any(|condition| condition.contains("ConvoyEnsure/governor")),
        "fleet diagnosis should identify the quarantined resource: {:?}",
        local.degraded_conditions
    );
    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE).list().await.expect("local host resource should exist");
    let [host] = hosts.items.as_slice() else { panic!("expected exactly one local host resource, got {:?}", hosts.items) };
    let host_id = host.metadata.name.clone();
    assert!(
        !host.status.as_ref().expect("startup heartbeat should publish host status").ready,
        "decode quarantine should degrade readiness"
    );

    delete_resource_kind(&daemon.resource_backend(), NAMESPACE, ConvoyEnsure::API_PATHS.kind, "governor")
        .await
        .expect("operator delete should resolve the quarantined identity");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let host = daemon
                .resource_backend()
                .using::<Host>(NAMESPACE)
                .get(&host_id)
                .await
                .expect("local host resource should remain available");
            if host.status.is_some_and(|status| status.ready) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host readiness should recover after the final quarantine is deleted");

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

    let health = daemon.fleet_health_internal().await.expect("fleet health");
    let local = health.hosts.iter().find(|host| host.is_local).expect("local fleet row");
    assert!(
        local.degraded_conditions.iter().any(|condition| condition.contains("daemon restarted 3× after abnormal exits in 30m")),
        "fleet diagnosis should surface the restart window: {:?}",
        local.degraded_conditions
    );
    let host_id = daemon.local_host_id().expect("local Host identity");
    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id.as_str()).await.expect("local Host");
    assert!(host.status.expect("local Host status").ready, "historical abnormal exits must not block placement after recovery");

    runtime.shutdown();
}
