use super::*;

#[tokio::test(start_paused = true)]
async fn controller_loop_watchdog_reports_and_clears_a_stale_heartbeat() {
    let health = RuntimeHealth::default();
    let heartbeat = Arc::new(AtomicU64::new(1));
    let watchdog = spawn_controller_loop_watchdog("TestPrimary", Arc::clone(&heartbeat), Duration::from_millis(10), health.clone());
    tokio::task::yield_now().await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
    assert!(health.conditions().await.iter().any(|condition| condition.condition_type == "ControllerLoop/TestPrimary"));
    heartbeat.fetch_add(1, Ordering::Relaxed);
    tokio::time::advance(Duration::from_millis(10)).await;
    tokio::task::yield_now().await;
    assert!(!health.conditions().await.iter().any(|condition| condition.condition_type == "ControllerLoop/TestPrimary"));
    watchdog.abort();
}

#[tokio::test]
async fn heartbeat_publishes_ambient_claude_expiry_metadata_without_material() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create claude dir");
    std::fs::write(
            home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-material","refreshToken":"sk-ant-ort01-material","expiresAt":1753000000000,"refreshTokenExpiresAt":1753900000000}}"#,
        )
        .expect("write ambient credentials");
    let config_base = temp.path().join("config");
    std::fs::create_dir_all(&config_base).expect("config directory");
    std::fs::write(config_base.join("daemon.toml"), "machine_id = \"ambient-claude-expiry-test\"\n").expect("daemon config");
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

    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(&host_id).await.expect("host resource");
    let status = host.status.expect("host status");
    let expiry = status.credential_expiry().expect("decode credential expiry capability");
    let ambient = expiry.get(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE).expect("ambient claude entry");
    assert_eq!(ambient.expires_at, chrono::DateTime::from_timestamp_millis(1_753_000_000_000));
    assert_eq!(ambient.refresh_expires_at, chrono::DateTime::from_timestamp_millis(1_753_900_000_000));
    let encoded = serde_json::to_string(&status).expect("serialize host status");
    assert!(!encoded.contains("sk-ant"), "credential material leaked into host status: {encoded}");
}

#[tokio::test]
async fn spawned_fulfilment_probe_task_publishes_facts_after_heartbeat() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"probe-task-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration");
    let kind_name = "host-direct-probe-task";
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(
            &empty_meta(kind_name),
            &FulfilmentKindSpec::builder()
                .host_ref(host_id.clone())
                .pool("passthrough".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("fulfilment kind");
    let runtime_health = RuntimeHealth::default();
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &test_health_identity(), &runtime_health)
        .await
        .expect("first heartbeat");
    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    assert!(hosts.get(&host_id).await.expect("host").status.expect("status").fulfilment_facts.is_empty());

    let task = spawn_local_fulfilment_probe_task(
        Arc::clone(&daemon),
        NAMESPACE.to_string(),
        profile,
        detection_registry(daemon.discovery_runtime().runner.clone()),
        temp.path().join("probe-cwd"),
    );
    wait_until_with_timeout(Duration::from_secs(5), || {
        let hosts = hosts.clone();
        let host_id = host_id.clone();
        async move {
            hosts
                .get(&host_id)
                .await
                .ok()
                .and_then(|host| host.status)
                .is_some_and(|status| status.fulfilment_facts.contains_key(kind_name))
        }
    })
    .await;
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn heartbeat_does_not_wait_for_fulfilment_probe_and_publishes_later_facts() {
    struct GatedProbeRunner {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl CommandRunner for GatedProbeRunner {
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            Err("unused".to_string())
        }

        async fn run_output(&self, cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            if cmd == "mkdir" {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) })
            } else if cmd == "rustc" {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(CommandOutput { stdout: "rustc 1.94.1".to_string(), stderr: String::new(), exit_code: Some(0) })
            } else {
                Err("tool unavailable".to_string())
            }
        }
    }

    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"async-facts-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration");
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(
            &empty_meta("host-direct-async-facts-test"),
            &FulfilmentKindSpec::builder()
                .host_ref(host_id.clone())
                .pool("passthrough".to_string())
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("fulfilment kind");
    let health = test_health_identity();
    let runtime_health = RuntimeHealth::default();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let backend = daemon.resource_backend();
    let probe_host = host_id.clone();
    let probe_pools = profile.available_pools.clone();
    let probe_runner = Arc::new(GatedProbeRunner { entered: Arc::clone(&entered), release: Arc::clone(&release) });
    let probe = tokio::spawn(async move {
        observe_fulfilment_facts(
            &backend,
            NAMESPACE,
            &probe_host,
            &probe_pools,
            &BTreeMap::new(),
            FulfilmentProbeContext {
                providers: &detection_registry(probe_runner.clone()),
                runner: probe_runner.as_ref(),
                env: &TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]),
                scratch: Path::new("/tmp/flotilla-probe-test"),
            },
            &mut ModelProbeState::default(),
        )
        .await
        .expect("probe facts")
    });
    entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(2),
        apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &health, &runtime_health),
    )
    .await
    .expect("heartbeat must not wait for probe")
    .expect("publish heartbeat");
    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    let status = hosts.get(&host_id).await.expect("host").status.expect("status");
    assert!(status.ready);
    assert_eq!(status.daemon_generation, health.generation);
    assert!(status.fulfilment_facts.is_empty());

    release.notify_one();
    let observed_facts = probe.await.expect("probe task");
    flotilla_store::apply_status_patch(
        &hosts,
        &host_id,
        &HostStatusPatch::FulfilmentFacts { facts: observed_facts, model_probes: ModelProbeState::default() },
    )
    .await
    .expect("publish independently observed facts");
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &health, &runtime_health)
        .await
        .expect("heartbeat preserves independently observed facts");
    let status = hosts.get(&host_id).await.expect("host").status.expect("status");
    assert_eq!(status.fulfilment_facts["host-direct-async-facts-test"].toolchains["rustc"], "rustc 1.94.1");

    let mut model_probes = ModelProbeState { total_requests: 2, ..ModelProbeState::default() };
    flotilla_store::apply_status_patch(
        &hosts,
        &host_id,
        &HostStatusPatch::FulfilmentFacts { facts: status.fulfilment_facts.clone(), model_probes: model_probes.clone() },
    )
    .await
    .expect("publish model probe count");
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &health, &runtime_health)
        .await
        .expect("heartbeat preserves model probe count");
    assert_eq!(hosts.get(&host_id).await.expect("host").status.expect("status").model_probes.total_requests, 2);

    let mut previous = status.fulfilment_facts;
    previous.get_mut("host-direct-async-facts-test").expect("prior fact").observed_at = Utc::now() - chrono::Duration::minutes(10);
    let failing_runner = Arc::new(DiscoveryMockRunner::builder().build());
    let after_failure = observe_fulfilment_facts(
        &daemon.resource_backend(),
        NAMESPACE,
        &host_id,
        &profile.available_pools,
        &previous,
        FulfilmentProbeContext {
            providers: &detection_registry(failing_runner.clone()),
            runner: failing_runner.as_ref(),
            env: &TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]),
            scratch: Path::new("/tmp/flotilla-probe-test"),
        },
        &mut model_probes,
    )
    .await
    .expect("observation retains prior facts");
    assert_eq!(after_failure, previous);
}

// #1496: a replicated description must produce a HostSnapshot for live
// surfaces without a query or peer-summary message, including while offline.
#[tokio::test]
async fn host_description_watch_publishes_offline_replica_updates() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-watch-observer\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let remote = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new("remote-root"));
    let hosts = remote.using::<Host>(NAMESPACE);
    let created = hosts.create(&empty_meta("remote-host"), &HostSpec::default()).await.expect("remote host");
    let summary = HostSummary::builder()
        .environment_id(EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("remote-host")))
        .host_name(flotilla_protocol::HostName::new("remote"))
        .node(flotilla_protocol::NodeInfo::new(NodeId::new("remote-node"), "remote"))
        .system(flotilla_protocol::SystemInfo { os: Some("linux".into()), ..Default::default() })
        .build();
    hosts
        .update_status(
            "remote-host",
            &created.metadata.resource_version,
            &HostStatus { description: Some(summary.clone()), heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .expect("remote description");
    let mut events = daemon.subscribe();
    let projection = spawn_host_description_projection_task(Arc::clone(&daemon), NAMESPACE.to_string(), Duration::from_secs(3600));
    daemon
        .resource_backend()
        .replica_writer::<Host>(NodeId::new("remote-root"), NAMESPACE)
        .replace(&hosts.list().await.expect("remote list"), Utc::now())
        .await
        .expect("replicate");
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let flotilla_protocol::DaemonEvent::HostSnapshot(snapshot) = events.recv().await.expect("event") {
                if snapshot.environment_id == summary.environment_id {
                    break snapshot;
                }
            }
        }
    })
    .await
    .expect("watch publishes replicated description");
    assert_eq!(snapshot.summary, summary);
    assert_eq!(snapshot.connection_status, flotilla_protocol::PeerConnectionState::Disconnected);
    let current = hosts.get("remote-host").await.expect("current host");
    let mut changed = summary.clone();
    changed.system.cpu_count = Some(8);
    hosts
        .update_status(
            "remote-host",
            &current.metadata.resource_version,
            &HostStatus { description: Some(changed.clone()), heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .expect("change remote description");
    daemon
        .resource_backend()
        .replica_writer::<Host>(NodeId::new("remote-root"), NAMESPACE)
        .replace(&hosts.list().await.expect("updated list"), Utc::now())
        .await
        .expect("replicate update");
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let flotilla_protocol::DaemonEvent::HostSnapshot(snapshot) = events.recv().await.expect("event") {
                if snapshot.environment_id == summary.environment_id && snapshot.summary == changed {
                    break snapshot;
                }
            }
        }
    })
    .await
    .expect("watch publishes offline update");
    assert!(updated.seq > snapshot.seq);
    assert_eq!(updated.connection_status, flotilla_protocol::PeerConnectionState::Disconnected);
    projection.abort();
    let _ = projection.await;
}

#[tokio::test]
async fn heartbeat_task_updates_host_status_without_socket_server() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"heartbeat-no-socket-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);

    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration should succeed");
    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    flotilla_store::apply_status_patch(
        &hosts,
        &host_id,
        &flotilla_resources::HostStatusPatch::SleepInhibition {
            health: flotilla_protocol::SleepInhibitionHealth::Failed { consecutive_failures: 3, message: "polkit denied".to_string() },
            observed_at: Utc::now(),
        },
    )
    .await
    .expect("seed sleep inhibition health");
    let heartbeat =
        spawn_heartbeat_task(Arc::clone(&daemon), NAMESPACE.to_string(), profile, test_health_identity(), Duration::from_millis(20));
    let blob_store = Arc::new(TieredBlobStore::new(temp.path(), vec![("test-fleet".to_string(), Arc::new(MemoryBlobStore::default()))]));
    blob_store.put(b"pending host status blob").await.expect("local blob put");
    let blob_status_task =
        spawn_blob_sync_status_task(Arc::clone(&blob_store), daemon.resource_backend(), NAMESPACE.to_string(), host_id.clone());

    wait_until_with_timeout(Duration::from_secs(30), || {
        let hosts = hosts.clone();
        let host_id = host_id.clone();
        async move { hosts.get(&host_id).await.ok().and_then(|host| host.status).is_some_and(|status| status.heartbeat_at.is_some()) }
    })
    .await;
    wait_until_with_timeout(Duration::from_secs(30), || {
        let hosts = hosts.clone();
        let host_id = host_id.clone();
        async move {
            hosts
                .get(&host_id)
                .await
                .ok()
                .and_then(|host| host.status)
                .and_then(|status| status.blob_sync)
                .is_some_and(|sync| sync.pending_count == 1)
        }
    })
    .await;
    let status = hosts.get(&host_id).await.expect("get host").status.expect("host status");
    assert_eq!(status.blob_sync.as_ref().expect("blob sync status").pending_count, 1);
    let local_environment_id = daemon.local_host_summary().await.environment_id;
    let host_status = daemon.get_host_status_internal(&local_environment_id).await.expect("query host status");
    assert_eq!(host_status.blob_sync.as_ref().expect("host query blob sync").pending_count, 1);
    assert!(!status.ready, "sleep inhibition failure should keep the host degraded");
    assert_eq!(status.agent_adapters().expect("valid agent adapter capability"), BTreeSet::new());
    assert_eq!(status.capabilities.get("docker"), Some(&json!(false)));
    assert_eq!(status.capabilities.get("terminal_pools"), Some(&json!(["passthrough"])));
    assert_eq!(status.daemon_generation.as_deref(), Some("test-generation"));
    assert_eq!(status.daemon_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    // Publish the exact handshake fingerprint, independently of restart identity.
    assert_eq!(status.protocol_fingerprint.as_deref(), Some(flotilla_protocol::PROTOCOL_FINGERPRINT));
    assert!(status.daemon_started_at.is_some());
    assert!(status.disk_free_bytes.is_some());
    assert!(status.daemon_rss_bytes.is_some_and(|bytes| bytes > 0));
    assert!(matches!(status.sleep_inhibition, flotilla_protocol::SleepInhibitionHealth::Failed { consecutive_failures: 3, .. }));
    assert_eq!(status.conditions.len(), 1);
    assert_eq!(status.conditions[0].condition_type, flotilla_resources::SLEEP_INHIBITION_CONDITION_TYPE);
    assert!(status.conditions[0].message.contains("polkit denied"));
    assert!(
        status.resource_store.expect("heartbeat should publish resource store diagnostics").event_log_within_retention(),
        "heartbeat should report a bounded resource event log"
    );
    let fleet = daemon.fleet_health_internal().await.expect("query fleet health");
    let local = fleet.hosts.iter().find(|host| host.is_local).expect("local fleet-health row");
    assert_eq!(local.blob_sync.as_ref().expect("fleet list blob sync").pending_count, 1);
    assert!(local.daemon_rss_bytes.is_some_and(|bytes| bytes > 0));
    assert!(
        local.degraded_conditions.iter().any(|condition| condition.contains("SleepInhibition") && condition.contains("polkit denied")),
        "fleet health should expose the sleep-inhibition condition: {:?}",
        local.degraded_conditions
    );

    heartbeat.abort();
    let _ = heartbeat.await;
    blob_status_task.abort();
    let _ = blob_status_task.await;
}

#[tokio::test]
async fn peer_summary_does_not_author_transitional_host_or_policy_rows() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"peer-summary-no-materialization-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), config).await;
    let peer_node = flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new("feta-node"), "feta");
    daemon
        .publish_peer_summary(
            HostSummary::builder()
                .environment_id(EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("feta-host")))
                .host_name(flotilla_protocol::HostName::new("feta"))
                .node(peer_node.clone())
                .system(flotilla_protocol::SystemInfo::default())
                .providers(Vec::new())
                .build(),
        )
        .await;
    daemon.publish_peer_connection_status(&peer_node, flotilla_protocol::PeerConnectionState::Connected).await;

    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    assert!(matches!(hosts.get("feta-host").await, Err(ResourceError::NotFound { .. })));
    let policies = daemon.resource_backend().using::<PlacementPolicy>(NAMESPACE);
    assert!(matches!(policies.get("host-direct-feta-host").await, Err(ResourceError::NotFound { .. })));
}

// Subprocess boundary stand-in: unresolved provider selection must not invoke a
// command against a guessed endpoint, including scratch creation.
struct UnselectedProbeRunner;

#[async_trait]
impl CommandRunner for UnselectedProbeRunner {
    async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        panic!("unselected provider must not launch a process")
    }
    async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        panic!("unselected provider must not launch a process")
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        panic!("unselected provider must not discover an endpoint")
    }
}

// Missing or ambiguous providers publish fresh unknown facts rather than omit a
// kind or reuse stale affirmative evidence. Generate both runtime kinds, zero
// and two matching instances, present/absent prior facts and pool availability.
#[hegel::test]
fn unresolved_fulfilment_provider_publishes_unknown(tc: hegel::TestCase) {
    use flotilla_core::providers::environment::{command_provider, EnvironmentKind};
    let docker = tc.draw(hegel::generators::booleans());
    let ambiguous = tc.draw(hegel::generators::booleans());
    let prior = tc.draw(hegel::generators::booleans());
    let pool = tc.draw(hegel::generators::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        ensure_host_exists(&backend, NAMESPACE, "host", "test").await.expect("host");
        let realisation =
            if docker { FulfilmentRealisation::DockerPerVessel { image: "crew:test".into() } } else { FulfilmentRealisation::HostDirect };
        backend
            .clone()
            .using::<FulfilmentKind>(NAMESPACE)
            .create(
                &empty_meta("kind"),
                &FulfilmentKindSpec::builder().host_ref("host".into()).pool("pool".into()).realisation(realisation).build(),
            )
            .await
            .expect("kind");
        let runner = Arc::new(UnselectedProbeRunner);
        let mut providers = ProviderRegistry::new();
        let provider_kind = if docker { EnvironmentKind::Docker } else { EnvironmentKind::HostDirect };
        if ambiguous {
            for instance in ["cache-a", "cache-b"] {
                providers.environment_providers.insert(
                    instance,
                    ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, instance),
                    command_provider(provider_kind, runner.clone()),
                );
            }
        }
        let prior_time = Utc::now() - chrono::Duration::minutes(1);
        let previous = if prior {
            BTreeMap::from([(
                "kind".into(),
                FulfilmentFacts {
                    image: docker.then(|| "crew:test".to_string().into()),
                    image_present: Some(true),
                    observed_at: prior_time,
                    ..Default::default()
                },
            )])
        } else {
            BTreeMap::new()
        };
        let pools = if pool { vec!["pool".into()] } else { Vec::new() };
        let mut model_probes = ModelProbeState::default();
        let observed = observe_fulfilment_facts(
            &backend,
            NAMESPACE,
            "host",
            &pools,
            &previous,
            FulfilmentProbeContext {
                providers: &providers,
                runner: runner.as_ref(),
                env: &TestEnvVars::default(),
                scratch: Path::new("/tmp/probe"),
            },
            &mut model_probes,
        )
        .await
        .expect("observation");
        assert_eq!(observed.len(), 1);
        let facts = observed.get("kind").expect("unknown facts remain visible");
        assert_eq!(facts.image.as_ref().map(|image| image.image_ref.as_str()), docker.then_some("crew:test"));
        assert_eq!(facts.image_present, None);
        assert!(facts.harnesses.is_empty() && facts.toolchains.is_empty());
        assert!(facts.observed_at > prior_time);
        assert_eq!(facts.free_vessel_slots, (!pool).then_some(0));
        assert_eq!(model_probes.total_requests, 0);
    });
}
