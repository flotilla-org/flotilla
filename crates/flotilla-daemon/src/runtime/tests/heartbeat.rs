use super::*;
use crate::runtime::tasks::LocalFulfilmentProbe;
use crate::testkits::engine::{admit_convoy, engine};
use flotilla_provisioning_testkit::EngineBackend;
use flotilla_resources::FulfilmentGrant;

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
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi", None).await.expect("host registration");
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
        runtime_health.host_observations.clone(),
        FULFILMENT_CHANGE_CHECK_INTERVAL,
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
    assert!(temp.path().join("probe-cwd").is_dir(), "image probes run in the probe directory, which the task creates");
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

        async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            if cmd == "rm" {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) })
            } else if cmd == "env" && args.last() == Some(&"--version") && args.get(args.len() - 2) == Some(&"rustc") {
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
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi", None).await.expect("host registration");
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
    let cache_runner = probe_runner.clone();
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
                observation: &Default::default(),
                harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                pinned_harnesses: &Default::default(),
                listings: &mut ProbeListings::default(),
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

    // The engine Host keeps no model probe state (`host_import.rs`); the probe's own count is
    // what a later probe reads.
    let mut model_probes = ModelProbeState { total_requests: 2, ..ModelProbeState::default() };

    // Recently cached observations retain independent availability, but neither
    // old verdict from another account can be republished as model evidence.
    let mut cached = status.fulfilment_facts.clone();
    let observation = cached.get_mut("host-direct-async-facts-test").expect("cached fact");
    observation.harnesses.insert(
        "claude-code".into(),
        flotilla_resources::HarnessFacts {
            version: "2.1.300".into(),
            models: [("yes", true), ("no", false)]
                .into_iter()
                .map(|(model, usable)| {
                    (model.into(), flotilla_resources::ModelFact { usable, source: flotilla_resources::ModelFactSource::Probe })
                })
                .collect(),
        },
    );
    for (model, usable) in [("yes", true), ("no", false)] {
        model_probes.entries.insert(
            model.into(),
            flotilla_resources::CachedModelProbe {
                observed_at: Utc::now(),
                fact: Some(flotilla_resources::ModelFact { usable, source: flotilla_resources::ModelFactSource::Probe }),
            },
        );
    }
    let cached_facts = observe_fulfilment_facts(
        &daemon.resource_backend(),
        NAMESPACE,
        &host_id,
        &profile.available_pools,
        &cached,
        FulfilmentProbeContext {
            providers: &detection_registry(cache_runner.clone()),
            runner: cache_runner.as_ref(),
            env: &TestEnvVars::default(),
            scratch: Path::new("/tmp/flotilla-probe-test"),
            observation: &Default::default(),
            harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            pinned_harnesses: &Default::default(),
            listings: &mut ProbeListings::default(),
        },
        &mut model_probes,
    )
    .await
    .expect("reuse recent availability without another command");
    let clean = &cached_facts["host-direct-async-facts-test"];
    assert!(clean.harnesses["claude-code"].models.is_empty());
    assert_eq!(clean.harnesses["claude-code"].version, "2.1.300");
    assert_eq!(clean.toolchains, cached["host-direct-async-facts-test"].toolchains);
    assert_eq!(clean.free_vessel_slots, cached["host-direct-async-facts-test"].free_vessel_slots);
    assert_eq!(model_probes, ModelProbeState::default());

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
            observation: &Default::default(),
            harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            pinned_harnesses: &Default::default(),
            listings: &mut ProbeListings::default(),
        },
        &mut model_probes,
    )
    .await
    .expect("observation retains prior facts");
    assert!(after_failure["host-direct-async-facts-test"].toolchains.is_empty());
    assert_eq!(model_probes, ModelProbeState::default());
}

// #1496: a replicated description must produce a HostSnapshot for live
// surfaces without a query or peer-summary message, including while offline.
// The engine Host carries only the description's architecture (`host_import.rs`), so that
// is what the remote home changes.
#[tokio::test]
async fn host_description_watch_publishes_offline_replica_updates() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"host-watch-observer\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let target = daemon.decided_store().expect("decided store");
    let remote = flotilla_provisioning_testkit::other_home_for(&target, "remote-root");
    let environment_id = EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("remote-host"));
    let described = |arch: &str| HostStatus {
        description: Some(
            HostSummary::builder()
                .environment_id(environment_id.clone())
                .node(flotilla_protocol::NodeInfo::new(NodeId::new("remote-root"), "remote"))
                .system(flotilla_protocol::SystemInfo { arch: Some(arch.into()), ..Default::default() })
                .build(),
        ),
        heartbeat_at: Some(Utc::now()),
        ..Default::default()
    };
    remote.host("remote-host", &HostSpec::default(), Some(&described("amd64"))).await;
    let mut events = daemon.subscribe();
    let projection = spawn_host_description_projection_task(Arc::clone(&daemon), NAMESPACE.to_string(), Duration::from_secs(3600));
    remote.replicate_to(&target);
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let flotilla_protocol::DaemonEvent::HostSnapshot(snapshot) = events.recv().await.expect("event") {
                if snapshot.environment_id == environment_id {
                    break snapshot;
                }
            }
        }
    })
    .await
    .expect("watch publishes replicated description");
    assert_eq!(snapshot.summary.system.arch.as_deref(), Some("amd64"));
    assert_eq!(snapshot.connection_status, flotilla_protocol::PeerConnectionState::Disconnected);
    remote.host("remote-host", &HostSpec::default(), Some(&described("arm64"))).await;
    remote.replicate_to(&target);
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let flotilla_protocol::DaemonEvent::HostSnapshot(snapshot) = events.recv().await.expect("event") {
                if snapshot.environment_id == environment_id && snapshot.summary.system.arch.as_deref() == Some("arm64") {
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
    // Fleet health reads the engine's records.
    engine(&daemon);
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = own_host_profile(&daemon, false);

    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, &profile.display_name, None)
        .await
        .expect("host registration should succeed");
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
    // The engine Host keeps what decisions read: generation, adapters and the conditions that
    // block readiness. Display fields and metrics (blob sync, memory, store figures, version,
    // fingerprint, disk figures) are dropped (`host_import.rs`).
    let heartbeat = spawn_heartbeat_task_with_credentials(
        Arc::clone(&daemon),
        NAMESPACE.to_string(),
        profile,
        Arc::new(None),
        test_health_identity(),
        RuntimeHealth::default(),
        Duration::from_millis(20),
    );

    wait_until_with_timeout(Duration::from_secs(30), || {
        let hosts = hosts.clone();
        let host_id = host_id.clone();
        async move {
            hosts
                .get(&host_id)
                .await
                .ok()
                .and_then(|host| host.status)
                .is_some_and(|status| status.heartbeat_at.is_some() && status.daemon_generation.is_some())
        }
    })
    .await;
    let status = hosts.get(&host_id).await.expect("get host").status.expect("host status");
    assert!(!status.ready, "sleep inhibition failure should keep the host degraded");
    assert_eq!(status.agent_adapters().expect("valid agent adapter capability"), BTreeSet::new());
    assert_eq!(status.daemon_generation.as_deref(), Some("test-generation"));
    assert_eq!(status.conditions.len(), 1);
    assert_eq!(status.conditions[0].condition_type, flotilla_resources::SLEEP_INHIBITION_CONDITION_TYPE);
    assert!(status.conditions[0].message.contains("polkit denied"));
    let fleet = daemon.fleet_health_internal().await.expect("query fleet health");
    let local = fleet.hosts.iter().find(|host| host.is_local).expect("local fleet-health row");
    assert!(
        local.degraded_conditions.iter().any(|condition| condition.contains("SleepInhibition") && condition.contains("polkit denied")),
        "fleet health should expose the sleep-inhibition condition: {:?}",
        local.degraded_conditions
    );

    heartbeat.abort();
    let _ = heartbeat.await;
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
    let kinds = daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE);
    assert!(matches!(kinds.get("host-direct-feta-host").await, Err(ResourceError::NotFound { .. })));
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
// kind or reuse stale affirmative evidence. Every combination of both runtime
// kinds, zero and two matching instances, present/absent prior facts and pool
// availability.
#[tokio::test]
async fn unresolved_fulfilment_provider_publishes_unknown() {
    use flotilla_environment::environment::{command_provider, EnvironmentInput};
    for docker in [false, true] {
        for ambiguous in [false, true] {
            for prior in [false, true] {
                for pool in [false, true] {
                    let home = EngineBackend::open_in("host", NAMESPACE);
                    let backend = home.backend.clone();
                    ensure_host_exists(&backend, NAMESPACE, "host", "test", None).await.expect("host");
                    let realisation = if docker {
                        FulfilmentRealisation::DockerPerVessel { image: Some("crew:test".into()) }
                    } else {
                        FulfilmentRealisation::HostDirect
                    };
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
                    let provider_kind = if docker { EnvironmentInput::Docker } else { EnvironmentInput::HostDirect };
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
                            observation: &Default::default(),
                            harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                            pinned_harnesses: &Default::default(),
                            listings: &mut ProbeListings::default(),
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
                }
            }
        }
    }
}

// #4002: grants come from observation. A host-direct kind stored with a GUI
// session grant, as kinds were derived before, loses it once its host's probe
// sees no display; a host whose probe observes one is granted it.
#[tokio::test]
async fn headless_host_is_not_granted_a_gui_session() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"headless-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "b1", None).await.expect("host registration");
    let mut stored = host_direct_kind_spec(&host_id, "linux");
    stored.grants.insert(FulfilmentGrant::gui_session());
    let kinds = daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE);
    kinds.create(&empty_meta("host-direct-b1"), &stored).await.expect("stored kind");
    let runner = Arc::new(DiscoveryMockRunner::builder().build());
    for (display, granted) in [(None, false), (Some(":0"), true)] {
        let env = TestEnvVars::new(display.map(|display| ("DISPLAY", display.to_string())));
        observe_fulfilment_facts(
            &daemon.resource_backend(),
            NAMESPACE,
            &host_id,
            &["passthrough".to_string()],
            &BTreeMap::new(),
            FulfilmentProbeContext {
                providers: &detection_registry(runner.clone()),
                runner: runner.as_ref(),
                env: &env,
                scratch: Path::new("/tmp/flotilla-probe-test"),
                observation: &Default::default(),
                harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                pinned_harnesses: &Default::default(),
                listings: &mut ProbeListings::default(),
            },
            &mut ModelProbeState::default(),
        )
        .await
        .expect("observe the host");
        let kind = kinds.get("host-direct-b1").await.expect("kind");
        assert_eq!(kind.spec.grants.contains(&FulfilmentGrant::gui_session()), granted, "display {display:?}: {:?}", kind.spec.grants);
        assert!(kind.spec.grants.contains(&FulfilmentGrant::host_account_reach()));
    }
}

// #4093: a host whose crews are given the pinned harnesses (an SSH host) reports each at
// its pin, though it has no `claude` or `codex` of its own on PATH.
#[tokio::test]
async fn a_host_given_pinned_harnesses_reports_their_pins_without_installing_them() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"pinned-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "b1", None).await.expect("host registration");
    let kinds = daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE);
    kinds.create(&empty_meta("host-direct-b1"), &host_direct_kind_spec(&host_id, "linux")).await.expect("stored kind");
    let runner = Arc::new(DiscoveryMockRunner::builder().build());
    let pins = flotilla_resources::stdlib::stdlib_harness_pins().clone();
    assert!(pins.contains_key("claude-code") && pins.contains_key("codex"), "{pins:?}");
    for (pinned, expected) in [(BTreeMap::new(), BTreeMap::new()), (pins.clone(), pins.clone())] {
        let facts = observe_fulfilment_facts(
            &daemon.resource_backend(),
            NAMESPACE,
            &host_id,
            &["passthrough".to_string()],
            &BTreeMap::new(),
            FulfilmentProbeContext {
                providers: &detection_registry(runner.clone()),
                runner: runner.as_ref(),
                env: &TestEnvVars::default(),
                scratch: Path::new("/tmp/flotilla-probe-test"),
                observation: &Default::default(),
                harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                pinned_harnesses: &pinned,
                listings: &mut ProbeListings::default(),
            },
            &mut ModelProbeState::default(),
        )
        .await
        .expect("observe the host");
        let reported = facts["host-direct-b1"]
            .harnesses
            .iter()
            .map(|(adapter, harness)| (adapter.clone(), harness.version.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(reported, expected);
    }
}

// #4002: a daemon host's capped Docker kind publishes the free slots counted
// from every vessel placed on the host, host-direct ones included, and keeps
// them current while a recent probe is reused.
#[tokio::test]
async fn capped_docker_kind_publishes_free_slots_from_the_hosts_vessels() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"docker-cap-test\"\nmax_vessels = 2\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    // The host's vessels are counted from the engine's convoys.
    engine(&daemon);
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "feta", Some(2)).await.expect("host registration");
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(
            &empty_meta("docker-feta"),
            &FulfilmentKindSpec::builder()
                .host_ref(host_id.clone())
                .pool("cleat".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string()), FulfilmentGrant::network("scoped".to_string())]))
                .realisation(FulfilmentRealisation::DockerPerVessel {
                    image: Some(flotilla_resources::PlacementImageSource::Literal("crew:test".into())),
                })
                .build(),
        )
        .await
        .expect("Docker kind");
    let runner = Arc::new(DiscoveryMockRunner::builder().build());
    let observe = |previous: BTreeMap<String, FulfilmentFacts>| {
        let backend = daemon.resource_backend();
        let host_id = host_id.clone();
        let runner = runner.clone();
        async move {
            observe_fulfilment_facts(
                &backend,
                NAMESPACE,
                &host_id,
                &["cleat".to_string()],
                &previous,
                FulfilmentProbeContext {
                    providers: &detection_registry(runner.clone()),
                    runner: runner.as_ref(),
                    env: &TestEnvVars::default(),
                    scratch: Path::new("/tmp/flotilla-probe-test"),
                    observation: &Default::default(),
                    harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                    pinned_harnesses: &Default::default(),
                    listings: &mut ProbeListings::default(),
                },
                &mut ModelProbeState::default(),
            )
            .await
            .expect("observe the host")
        }
    };
    let first = observe(BTreeMap::new()).await;
    assert_eq!(first["docker-feta"].free_vessel_slots, Some(2));

    // A host-direct convoy admitted to the same host holds one of its slots.
    let decision = flotilla_protocol::PlacementDecision {
        provider_instance: None,
        provider_workspace_root: None,
        kind: format!("host-direct-{host_id}"),
        target_host: flotilla_protocol::PlacementTargetHost {
            reference: flotilla_protocol::CanonicalHostId::resolved(host_id.clone()),
            display_name: "feta".to_string(),
        },
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };
    admit_convoy(
        &daemon,
        flotilla_engine_convoys_testkit::convoy(
            NAMESPACE,
            "host-direct-work",
            flotilla_resources::ConvoySpec::builder().workflow_ref("missing".to_string()).build(),
            flotilla_resources::ConvoyStatus {
                placement_decision: Some(decision),
                ..flotilla_engine_convoys_testkit::single_agent("coder")
            },
        ),
    )
    .await;
    let reused = observe(first.clone()).await;
    assert_eq!(reused["docker-feta"].observed_at, first["docker-feta"].observed_at, "the recent probe is reused");
    assert_eq!(reused["docker-feta"].free_vessel_slots, Some(1));

    // Removing the cap from daemon.toml clears it from the Host on the next
    // registration, and the kind is unbounded again.
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "feta", None).await.expect("host re-registration");
    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(&host_id).await.expect("host");
    assert_eq!(host.spec.max_vessels, None);
    assert_eq!(observe(reused).await["docker-feta"].free_vessel_slots, None);
}

// #4002: an unknown observation is not one. A host-direct kind keeps its GUI
// grant when no environment provider can probe its host.
#[tokio::test]
async fn unknown_observation_keeps_a_gui_grant() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"unknown-gui-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi", None).await.expect("host registration");
    let mut observed = host_direct_kind_spec(&host_id, "macos");
    observed.grants.insert(FulfilmentGrant::gui_session());
    let kinds = daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE);
    kinds.create(&empty_meta("host-direct-kiwi"), &observed).await.expect("kind with an observed GUI");
    let runner = Arc::new(DiscoveryMockRunner::builder().build());
    let facts = observe_fulfilment_facts(
        &daemon.resource_backend(),
        NAMESPACE,
        &host_id,
        &["passthrough".to_string()],
        &BTreeMap::new(),
        FulfilmentProbeContext {
            providers: &flotilla_discovery::registry::ProviderRegistry::new(),
            runner: runner.as_ref(),
            env: &TestEnvVars::default(),
            scratch: Path::new("/tmp/flotilla-probe-test"),
            observation: &Default::default(),
            harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            pinned_harnesses: &Default::default(),
            listings: &mut ProbeListings::default(),
        },
        &mut ModelProbeState::default(),
    )
    .await
    .expect("observe without a provider");
    assert!(!facts["host-direct-kiwi"].gui_session_logged_in, "the published fact is unknown");
    let kind = kinds.get("host-direct-kiwi").await.expect("kind");
    assert!(kind.spec.grants.contains(&FulfilmentGrant::gui_session()), "{:?}", kind.spec.grants);
}

/// What `tick` cost: its output, its reads of Hosts by kind, and the entries this home's log
/// gained while it ran.
async fn host_tick_cost<F: std::future::Future>(daemon: &InProcessDaemon, tick: F) -> (F::Output, HostReads, u64) {
    let store = daemon.decided_store().expect("decided store");
    let logged = || store.last_sequence(store.home()).expect("this home's sequence");
    let before = logged();
    // The Hosts' resident records read every earlier write first, so their catching up is not counted.
    daemon.resource_backend().using::<Host>(NAMESPACE).list().await.expect("hosts");
    let (output, reads) = flotilla_store::load_metrics::measure(tick).await;
    let host_reads = reads
        .into_iter()
        .filter(|(kind, _)| kind.starts_with("Host"))
        .map(|(kind, counts)| (kind, counts.keyed + counts.index + counts.whole_kind))
        .filter(|(_, reads)| *reads > 0)
        .collect();
    (output, HostReads(host_reads), logged() - before)
}

/// Reads of the Host kinds, by kind: the port's, and the store's of each record kind.
#[derive(Debug)]
struct HostReads(BTreeMap<String, u64>);

impl HostReads {
    fn total(&self) -> u64 {
        self.0.values().sum()
    }
}

// Behaviour: a heartbeat tick that finds nothing changed since the tick before reads the Hosts
// once and writes nothing.
#[tokio::test]
async fn an_idle_heartbeat_reads_the_hosts_once_and_writes_nothing() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"idle-heartbeat-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);
    let (health, runtime_health) = (test_health_identity(), RuntimeHealth::default());
    let heartbeat = || apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &health, &runtime_health);
    let ((), _, written) = host_tick_cost(&daemon, async { heartbeat().await.expect("first heartbeat") }).await;
    assert!(written > 0, "the first heartbeat registers the Host and writes its observation");

    let ((), reads, written) = host_tick_cost(&daemon, async { heartbeat().await.expect("idle heartbeat") }).await;
    assert!(reads.total() <= 1, "an idle heartbeat read the Hosts more than once: {reads:?}");
    assert_eq!(written, 0, "an idle heartbeat wrote");
}

// Behaviour: a fulfilment probe tick that observes the facts it published before reads the Hosts
// once and writes nothing.
#[tokio::test]
async fn an_idle_fulfilment_probe_reads_the_hosts_once_and_writes_nothing() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"idle-probe-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);
    let runtime_health = RuntimeHealth::default();
    apply_host_heartbeat_with_credentials(&daemon, NAMESPACE, &profile, None, &test_health_identity(), &runtime_health)
        .await
        .expect("heartbeat");
    daemon
        .resource_backend()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(&empty_meta("host-direct-idle-probe"), &host_direct_kind_spec(&host_id, "linux"))
        .await
        .expect("fulfilment kind");
    let probe = LocalFulfilmentProbe::new(
        Arc::clone(&daemon),
        NAMESPACE.to_string(),
        profile,
        detection_registry(daemon.discovery_runtime().runner.clone()),
        temp.path().join("probe-cwd"),
        runtime_health.host_observations.clone(),
    );
    let ((), _, written) = host_tick_cost(&daemon, probe.tick()).await;
    assert!(written > 0, "the first probe publishes the kind's facts");
    let status = daemon.resource_backend().using::<Host>(NAMESPACE).get(&host_id).await.expect("host").status.expect("status");
    assert!(status.fulfilment_facts.contains_key("host-direct-idle-probe"), "{:?}", status.fulfilment_facts);

    let ((), reads, written) = host_tick_cost(&daemon, probe.tick()).await;
    assert!(reads.total() <= 1, "an idle probe read the Hosts more than once: {reads:?}");
    assert_eq!(written, 0, "an idle probe wrote");
}

fn host_direct_kind_spec(host_id: &str, platform: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref(host_id.to_string())
        .pool("passthrough".to_string())
        .grants(BTreeSet::from([
            FulfilmentGrant::platform(platform.to_string()),
            FulfilmentGrant::host_account_reach(),
            FulfilmentGrant::network("host".to_string()),
        ]))
        .realisation(FulfilmentRealisation::HostDirect)
        .build()
}
