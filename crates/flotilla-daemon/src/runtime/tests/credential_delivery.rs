use super::*;

#[tokio::test]
async fn credential_refresh_retry_waits_until_deadline_without_rewriting_status() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    environments
        .create(
            &empty_meta("refresh-work"),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: "/tmp".into() }),
                docker: None,
            },
        )
        .await
        .expect("create environment");
    let failure = CredentialRefreshError {
        environment_ref: "refresh-work".into(),
        credential_name: Some("github-app".into()),
        message: "provider unavailable".into(),
        should_surface: true,
    };
    record_credential_refresh_dispositions(&backend, NAMESPACE, std::slice::from_ref(&failure)).await.expect("record first failure");
    let first = environments.get("refresh-work").await.expect("environment after first failure");
    let retry = first.status.expect("status").credential_refresh_retry.expect("retry record");
    assert_eq!(retry.attempts, 1);
    record_credential_refresh_dispositions(&backend, NAMESPACE, std::slice::from_ref(&failure))
        .await
        .expect("observe failure again before deadline");
    let second = environments.get("refresh-work").await.expect("environment after repeated failure");
    assert_eq!(second.metadata.resource_version, first.metadata.resource_version);
    assert_eq!(second.status.expect("status").credential_refresh_retry, Some(retry));
    record_credential_refresh_dispositions(&backend, NAMESPACE, &[]).await.expect("clear on recovery");
    assert!(environments
        .get("refresh-work")
        .await
        .expect("environment after recovery")
        .status
        .expect("status")
        .credential_refresh_retry
        .is_none());
}

#[tokio::test]
async fn credential_refresh_attention_appears_and_clears_on_recovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"credential-refresh-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let backend = daemon.resource_backend();
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("credential-convoy"), &ConvoySpec::builder().workflow_ref("test".to_string()).build())
        .await
        .expect("create convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .credential_refs(BTreeSet::from(["github-app".to_string()]))
                        .crew(Vec::new())
                        .build()],
                }),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("set convoy status");
    let vessels = backend.using::<Vessel>(NAMESPACE);
    let vessel = vessels
        .create(
            &empty_meta("credential-vessel"),
            &VesselSpec {
                convoy_ref: "credential-convoy".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("create vessel");
    vessels
        .update_status(
            &vessel.metadata.name,
            &vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some("standing-vessel".to_string()),
                ..VesselStatus::default()
            },
        )
        .await
        .expect("place vessel");
    let transient = CredentialRefreshError {
        environment_ref: "standing-vessel".to_string(),
        credential_name: Some("github-app".to_string()),
        message: "credential `github-app`: outage; expires in 180 seconds".to_string(),
        should_surface: false,
    };
    reconcile_credential_refresh_attention(&daemon, NAMESPACE, &[transient]).await.expect("ignore transient failure");
    let demands = backend.using::<Demand>(NAMESPACE);
    assert!(demands.list().await.expect("list demands").items.is_empty());

    let persistent = CredentialRefreshError {
        environment_ref: "standing-vessel".to_string(),
        credential_name: Some("github-app".to_string()),
        message: "credential `github-app`: outage; expires in 120 seconds".to_string(),
        should_surface: true,
    };
    reconcile_credential_refresh_attention(&daemon, NAMESPACE, &[persistent]).await.expect("raise attention");
    let raised = demands.list().await.expect("list demands").items;
    assert_eq!(raised.len(), 1);
    assert!(raised[0].metadata.annotations[CREDENTIAL_REFRESH_REASON_ANNOTATION].contains("expires in 120 seconds"));
    assert_eq!(raised[0].spec.originating_work_ref.name, "credential-convoy");
    flotilla_store::apply_status_patch(
        &demands,
        &raised[0].metadata.name,
        &flotilla_resources::DemandStatusPatch::Acknowledge { as_of: chrono::Utc::now(), authority: "operator".to_string() },
    )
    .await
    .expect("acknowledge demand");
    reconcile_credential_refresh_attention(
        &daemon,
        NAMESPACE,
        &[CredentialRefreshError {
            environment_ref: "standing-vessel".to_string(),
            credential_name: Some("github-app".to_string()),
            message: "credential `github-app`: outage; expires in 60 seconds".to_string(),
            should_surface: true,
        }],
    )
    .await
    .expect("keep persistent failure raised");
    assert_eq!(
        demands.list().await.expect("list demands").items[0]
            .status
            .as_ref()
            .map_or(flotilla_resources::DemandState::Raised, |status| status.state),
        flotilla_resources::DemandState::Raised
    );
    reconcile_credential_refresh_attention(
        &daemon,
        NAMESPACE,
        &[CredentialRefreshError {
            environment_ref: "standing-vessel".to_string(),
            credential_name: Some("github-app".to_string()),
            message: "credential `github-app`: outage; expired 5 seconds ago".to_string(),
            should_surface: false,
        }],
    )
    .await
    .expect("retain alert during retry after restart");
    assert_eq!(demands.list().await.expect("list demands").items.len(), 1);
    reconcile_credential_refresh_attention(&daemon, NAMESPACE, &[]).await.expect("clear on recovery");
    assert!(demands.list().await.expect("list demands").items.is_empty());
}

#[test]
fn docker_environment_metadata_decodes_non_empty_credential_scopes() {
    let repository = flotilla_resources::RepositoryKey("github.com-flotilla-org-flotilla".to_string());
    let expected = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository]))]);
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-a".to_string(),
        image: "crew:latest".to_string(),
        declared_agent_adapters: BTreeSet::new(),
        required_agent_adapters: BTreeSet::new(),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::from([(CREDENTIAL_SCOPES_ENV.to_string(), serde_json::to_string(&expected).expect("encode credential scopes"))]),
    };

    assert_eq!(credential_scopes_from_environment(&spec).expect("decode credential scopes"), expected);
}

// #2577: misspelled mappings surface at host level, while a valid Forge
// needs no checkout. Late declarations and config corrections clear them.
#[tokio::test]
async fn daemon_forgejo_credential_mapping_heartbeat_lifecycle() {
    let temp = TempDir::new().expect("tempdir");
    credential_mapping_config(temp.path(), Some("lba"));
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let backend = daemon.resource_backend();
    declare_mapping_forge(&backend, NAMESPACE).await;
    let host_id = daemon.local_host_id().expect("host id").to_string();
    let profile = manual_profile(&host_id, false);
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("heartbeat");
    let status = backend.using::<Host>(NAMESPACE).get(&host_id).await.expect("host").status.expect("status");
    let condition = status
        .conditions
        .iter()
        .find(|condition| condition.condition_type == "DaemonForgejoCredentials")
        .expect("misspelled mapping diagnosis");
    assert_eq!(condition.reason, "UnresolvedForgeMappings");
    assert!(condition.message.contains("`lba`"));
    assert!(condition.message.contains(&format!("namespaces `{NAMESPACE}`")));
    assert!(!condition.blocks_readiness());
    assert!(status.ready, "an unresolved mapping must not block unrelated placement");
    let fleet = daemon.fleet_health_internal().await.expect("fleet health");
    assert!(fleet
        .hosts
        .iter()
        .find(|host| host.is_local)
        .expect("local host")
        .degraded_conditions
        .iter()
        .any(|message| message.contains("`lba`")));

    credential_mapping_config(temp.path(), Some("lab"));
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("corrected heartbeat");
    let status = backend.using::<Host>(NAMESPACE).get(&host_id).await.expect("host").status.expect("status");
    assert!(status.conditions.iter().all(|condition| condition.condition_type != "DaemonForgejoCredentials"));
    assert!(backend.using::<Checkout>(NAMESPACE).list().await.expect("checkouts").items.is_empty());

    backend.using::<Forge>(NAMESPACE).delete("lab").await.expect("delete Forge");
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("unresolved heartbeat");
    assert!(backend
        .using::<Host>(NAMESPACE)
        .get(&host_id)
        .await
        .expect("host")
        .status
        .expect("status")
        .conditions
        .iter()
        .any(|condition| condition.condition_type == "DaemonForgejoCredentials"));
    declare_mapping_forge(&backend, NAMESPACE).await;
    apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("late declaration heartbeat");
    assert!(backend
        .using::<Host>(NAMESPACE)
        .get(&host_id)
        .await
        .expect("host")
        .status
        .expect("status")
        .conditions
        .iter()
        .all(|condition| condition.condition_type != "DaemonForgejoCredentials"));

    credential_mapping_config(temp.path(), Some("lba"));
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("diagnosis").is_some());
    credential_mapping_config(temp.path(), None);
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("removed mapping").is_none());
}

// Advisory diagnosis errors must not propagate into the heartbeat. The
// wrapper's Option result enforces that boundary even for malformed config.
#[tokio::test]
async fn daemon_forgejo_credential_mapping_errors_are_advisory_and_retry() {
    let temp = TempDir::new().expect("tempdir");
    credential_mapping_config(temp.path(), None);
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    fs::write(temp.path().join("daemon.toml"), "[credentials.forgejo\n").expect("malformed config");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.is_err());
    assert!(daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.is_none());
    credential_mapping_config(temp.path(), Some("lba"));
    assert!(daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.is_some(), "later heartbeats retry the diagnosis");
}

// An unqualified host-local mapping can serve a source-addressed request
// whose Forge is replicated into a namespace with no local checkout.
#[tokio::test]
async fn daemon_forgejo_credential_mapping_resolves_replica_in_another_namespace() {
    let temp = TempDir::new().expect("tempdir");
    credential_mapping_config(temp.path(), Some("lab"));
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    assert!(daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.is_some());
    let authority = ResourceBackend::InMemory(Default::default());
    declare_mapping_forge(&authority, "other").await;
    daemon
        .resource_backend()
        .replica_writer::<Forge>(NodeId::new("forge-authority"), "other")
        .replace(&authority.using::<Forge>("other").list().await.expect("Forge snapshot"), Utc::now())
        .await
        .expect("replicate Forge outside runtime namespace");
    assert!(daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.is_none());
    assert!(daemon.resource_backend().using::<Checkout>("other").list().await.expect("checkouts").items.is_empty());
}

// #2577: startup manifests must finish a pass before absence is diagnosed;
// their late Forge declarations satisfy the mapping without a checkout.
#[tokio::test]
async fn daemon_forgejo_credential_mapping_waits_for_manifest_pass() {
    let temp = TempDir::new().expect("tempdir");
    credential_mapping_config(temp.path(), Some("lab"));
    let mut config = fs::read_to_string(temp.path().join("daemon.toml")).expect("config");
    config.push_str("[manifests]\ndir = \"/test/manifests\"\nsource = \"test-source\"\nreconciler_root = \"credential-mapping-test\"\n");
    fs::write(temp.path().join("daemon.toml"), config).expect("manifest config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let backend = daemon.resource_backend();
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("pending source").is_none());
    let root = materialize_manifest_root(&backend, NAMESPACE, Path::new("/test/manifests"), "test-source", "credential-mapping-test")
        .await
        .expect("manifest root");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("pending pass").is_none());
    backend
        .using::<ManifestRoot>(NAMESPACE)
        .update_status(&root.metadata.name, &root.metadata.resource_version, &flotilla_resources::ManifestRootStatus::default())
        .await
        .expect("completed empty pass");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("settled source").is_some());
    declare_mapping_forge(&backend, NAMESPACE).await;
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("late declaration").is_none());
}

// #2577: an unrelated replica cursor cannot establish Forge absence. The
// Forge snapshot can be empty or contain the declaration after startup.
#[tokio::test]
async fn daemon_forgejo_credential_mapping_waits_for_forge_replication() {
    let temp = TempDir::new().expect("tempdir");
    credential_mapping_config(temp.path(), Some("lab"));
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let peer_id = NodeId::new("forge-authority");
    let peer = NodeInfo::new(peer_id.clone(), "authority");
    daemon.set_configured_peers(vec![peer.clone()]).await;
    daemon
        .publish_peer_summary(
            HostSummary::builder()
                .environment_id(EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("authority-host")))
                .host_name(flotilla_protocol::HostName::new("authority"))
                .node(peer.clone())
                .system(flotilla_protocol::SystemInfo::default())
                .providers(Vec::new())
                .build(),
        )
        .await;
    daemon.publish_peer_connection_status(&peer, PeerConnectionState::Connected).await;
    let backend = daemon.resource_backend();
    let authority = ResourceBackend::InMemory(Default::default());
    backend
        .replica_writer::<Host>(peer_id.clone(), NAMESPACE)
        .replace(&authority.using::<Host>(NAMESPACE).list().await.expect("host snapshot"), Utc::now())
        .await
        .expect("unrelated replication");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("pending Forge replication").is_none());
    let writer = backend.replica_writer::<Forge>(peer_id, NAMESPACE);
    writer
        .replace(&authority.using::<Forge>(NAMESPACE).list().await.expect("empty Forge snapshot"), Utc::now())
        .await
        .expect("empty Forge replication settled");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("settled absence").is_some());
    declare_mapping_forge(&authority, NAMESPACE).await;
    writer
        .replace(&authority.using::<Forge>(NAMESPACE).list().await.expect("Forge snapshot"), Utc::now())
        .await
        .expect("late Forge replication");
    assert!(resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("replicated declaration").is_none());
}

// #2577: source-addressed requests can use declarations outside the runtime
// namespace. Exercise config removal/correction and declaration arrival/deletion
// in generated order, checking the invariant after every step.
#[hegel::test]
fn daemon_forgejo_credential_mapping_sequences(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Empty map, typo, valid key, declaration present/absent, duplicate ops.
    let operations = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(12)))
        .map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(5)))
        .collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = TempDir::new().expect("tempdir");
        credential_mapping_config(temp.path(), None);
        let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
        let backend = daemon.resource_backend();
        let mut key = None;
        let mut declared = false;
        let mut other_declared = false;
        for op in operations {
            match op {
                0 => key = None,
                1 => key = Some("lba"),
                2 => key = Some("lab"),
                3 if !declared => {
                    declare_mapping_forge(&backend, NAMESPACE).await;
                    declared = true;
                }
                4 if declared => {
                    backend.using::<Forge>(NAMESPACE).delete("lab").await.expect("delete Forge");
                    declared = false;
                }
                5 if !other_declared => {
                    declare_mapping_forge(&backend, "other").await;
                    other_declared = true;
                }
                _ => {}
            }
            credential_mapping_config(temp.path(), key);
            let diagnosis = resolve_daemon_forgejo_credential_condition(&daemon, NAMESPACE).await.expect("diagnosis");
            let resolves = key == Some("lab") && (declared || other_declared);
            assert_eq!(diagnosis.is_some(), key.is_some() && !resolves);
        }
    });
}

#[tokio::test]
async fn agent_environment_is_staged_at_the_runner_writable_config_base() {
    let runner = PersistentPathRecordingRunner::default();

    let path = stage_agent_environment(&runner, Path::new("/host/state"), "export CODEX_HOME='/mounted/codex'\n")
        .await
        .expect("stage agent environment");

    assert_eq!(path, Path::new("/home/crew/flotilla/agent-environment"));
    assert_eq!(
        runner.writes.lock().expect("writes lock").as_slice(),
        &[(PathBuf::from("/home/crew/flotilla/agent-environment"), "export CODEX_HOME='/mounted/codex'\n".to_string())]
    );
    assert!(!path.starts_with("/run/flotilla"));
}

#[tokio::test]
async fn fresh_claude_provisioning_stages_generation_pinned_skills() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"claude-skills-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let backend = daemon.resource_backend();
    let app_id_path = temp.path().join("github-app.id");
    let private_key_path = temp.path().join("github-app.pem");
    fs::write(&app_id_path, "12345\n").expect("write App id");
    fs::write(&private_key_path, GITHUB_APP_TEST_PRIVATE_KEY).expect("write App private key");
    for (name, consumer, source) in [
        ("claude-max", CredentialConsumer::ClaudeOauth { account_email: "test@example.com".to_string() }, "TEST_CLAUDE_TOKEN"),
        ("github-crew-pr", CredentialConsumer::Gh, "TEST_GITHUB_TOKEN"),
    ] {
        backend
            .clone()
            .definitions::<CredentialSpec>(NAMESPACE)
            .create(
                &empty_meta(name),
                &CredentialSpecSpec {
                    consumer,
                    source: CredentialSource::Env { name: source.to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("credential declaration");
    }
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("github-skills-fork"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp {
                    actor_login: None,
                    installation_id: Some(9876),
                    installation_repository: None,
                    permissions: Some(BTreeMap::from([("contents".to_string(), "read".to_string())])),
                },
                source: CredentialSource::GithubApp {
                    app_id_path: app_id_path.to_string_lossy().into_owned(),
                    private_key_path: private_key_path.to_string_lossy().into_owned(),
                },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("skill credential declaration");

    let staged = Arc::new(AtomicBool::new(false));
    let runner: Arc<dyn CommandRunner> =
        Arc::new(CredentialInteriorRunner(DiscoveryMockRunner::builder().tool_exists("claude", true).build(), Some(Arc::clone(&staged))));
    let destroyed = Arc::new(AtomicBool::new(false));
    let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
        id: EnvironmentId::new("contained-claude"),
        image: ImageId::new("contained-image"),
        runner: Arc::clone(&runner),
        env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
        destroyed: Arc::clone(&destroyed),
    });
    let mut local_registry = ProviderRegistry::new();
    local_registry.environment_providers.insert(
        "docker",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"),
        Arc::new(TestInteriorEnvironmentProvider { handle: Mutex::new(Some(handle)) }),
    );
    let host_env = Arc::new(TestEnvVars::new([
        ("HOME", temp.path().join("home").display().to_string()),
        ("TEST_CLAUDE_TOKEN", "claude-secret".to_string()),
        ("TEST_GITHUB_TOKEN", "github-secret".to_string()),
    ]));
    let mint_session = Session::replaying_from_str(
        r#"interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["mattpocock-skills"],"permissions":{"contents":"read"}}'
    status: 201
    response_body: '{"token":"skill-token","expires_at":"2026-08-03T17:00:00Z"}'
"#,
        Masks::new(),
    );
    let credential_store = Arc::new(CredentialStore::new_with_http(
        backend.clone(),
        NAMESPACE,
        host_env.clone(),
        EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude")),
        Arc::clone(&runner),
        Arc::new(ReplayHttpClient::new(mint_session.clone())),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
        ("HOME", temp.path().join("home").display().to_string()),
        (FLOTILLA_SKILLS_DIR_ENV, write_test_credentialed_skill_sources(temp.path()).display().to_string()),
    ]))));
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))
        .with_credential_store(credential_store)
        .with_agent_material(agent_material),
    );
    let credential_refs = BTreeSet::from(["claude-max".to_string(), "github-crew-pr".to_string()]);
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::new(),
        required_agent_adapters: BTreeSet::from(["claude-code".to_string()]),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        // Intended: provisioning mints only the explicitly selected source's token.
        env: BTreeMap::from([
            (CREDENTIAL_REFS_ENV.to_string(), serde_json::to_string(&credential_refs).expect("encode credential refs")),
            (
                "FLOTILLA_RESOLVED_SKILLS".to_string(),
                serde_json::to_string(&vec![flotilla_resources::SkillCatalogEntry {
                    source: "mattpocock-skills".to_string(),
                    repository: "flotilla-org/mattpocock-skills".to_string(),
                    revision: "1".repeat(40),
                    name: "research".to_string(),
                    path: "skills/research".to_string(),
                }])
                .expect("encode frozen skill selection"),
            ),
        ]),
    };

    DockerControllerRuntime { state }.provision("contained-claude", &spec).await.expect("provision Claude vessel");

    assert!(staged.load(Ordering::SeqCst), "provisioning must stage pinned skills before interior discovery");
    assert!(!destroyed.load(Ordering::SeqCst), "successful skill staging must keep the fresh vessel");
    mint_session.assert_complete();
}

#[tokio::test]
async fn codex_adapter_delivers_the_central_credential_in_a_writable_codex_home() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"codex-material-test\"\n").expect("daemon config");
    let home = temp.path().join("home");
    let skill_sources = write_test_skill_sources(temp.path());
    let central = home.join(".config/flotilla/credentials/codex-central/auth.json");
    fs::create_dir_all(central.parent().expect("central credential directory")).expect("central credential directory");
    fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-one\"}}").expect("central auth");
    fs::set_permissions(&central, fs::Permissions::from_mode(0o600)).expect("protect central auth");
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
    let credential_store = Arc::new(CredentialStore::new(
        daemon.resource_backend(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("HOME", home.display().to_string())])),
        EnvironmentBag::new(),
        Arc::new(ProcessCommandRunner),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
        ("HOME", home.display().to_string()),
        (FLOTILLA_SKILLS_DIR_ENV, skill_sources.display().to_string()),
    ]))));
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))
        .with_credential_store(credential_store)
        .with_agent_material(agent_material),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
        required_agent_adapters: BTreeSet::from(["codex".to_string()]),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::new(),
    };

    let error = DockerControllerRuntime { state: Arc::clone(&state) }
        .provision("contained-work", &spec)
        .await
        .expect_err("capture provider should stop provision");

    assert_eq!(error.to_string(), "stop after capturing create options");
    let opts = provider.create_opts.lock().await.take().expect("captured create options");
    assert!(opts.provisioned_mounts.contains(&ProvisionedMount::new(
        home.join(".local/share/flotilla/agent-homes/contained-work/codex"),
        CONTAINER_CODEX_HOME,
        ProvisionedMountMode::Rw,
    )));
    let mut expected_tokens = vec![("CODEX_HOME".to_string(), CONTAINER_CODEX_HOME.to_string())];
    expected_tokens.extend(crew_git_identity_environment());
    assert_eq!(opts.tokens, expected_tokens);

    let mut preconfigured = spec;
    preconfigured.env.insert("CODEX_HOME".to_string(), "/image/codex".to_string());
    DockerControllerRuntime { state }
        .provision("contained-preconfigured", &preconfigured)
        .await
        .expect_err("capture provider should stop provision");
    let opts = provider.create_opts.lock().await.take().expect("captured preconfigured create options");
    let mut expected_tokens = vec![("CODEX_HOME".to_string(), "/image/codex".to_string())];
    expected_tokens.extend(crew_git_identity_environment());
    assert_eq!(opts.tokens, expected_tokens);
    assert!(
        opts.provisioned_mounts.iter().all(|mount| mount.environment_path.as_path() != Path::new(CONTAINER_CODEX_HOME)),
        "a placement-provided CODEX_HOME must not be overwritten with a delivered Codex home"
    );
}

#[tokio::test]
async fn codex_credential_and_agent_material_conflict_before_container_creation() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"codex-material-conflict-test\"\n").expect("daemon config");
    let home = temp.path().join("home");
    let skills = home.join(".codex/skills/pr-shepherd");
    fs::create_dir_all(&skills).expect("skill directory");
    fs::write(skills.join("SKILL.md"), "# PR shepherd\n").expect("skill definition");
    let central = home.join(".config/flotilla/credentials/codex-central/auth.json");
    fs::create_dir_all(central.parent().expect("central credential directory")).expect("central credential directory");
    fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-one\"}}").expect("central auth");
    fs::set_permissions(&central, fs::Permissions::from_mode(0o600)).expect("protect central auth");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
    let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
    daemon
        .resource_backend()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &InputMeta::builder().name("openai".to_string()).build(),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Codex,
                source: CredentialSource::Env { name: "TEST_CODEX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Codex credential");
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
    let env = Arc::new(TestEnvVars::new([("HOME", home.display().to_string()), ("TEST_CODEX_TOKEN", "secret".to_string())]));
    let credential_store = Arc::new(CredentialStore::new(
        daemon.resource_backend(),
        NAMESPACE,
        env.clone(),
        EnvironmentBag::new(),
        Arc::new(ProcessCommandRunner),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agent_material = Arc::new(test_agent_material_registry(env));
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))
        .with_credential_store(credential_store)
        .with_agent_material(agent_material),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "contained-image".to_string(),
        declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
        required_agent_adapters: BTreeSet::from(["codex".to_string()]),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::from([(
            CREDENTIAL_REFS_ENV.to_string(),
            serde_json::to_string(&BTreeSet::from(["openai".to_string()])).expect("encode credential refs"),
        )]),
    };

    let error = DockerControllerRuntime { state }
        .provision("contained-work", &spec)
        .await
        .expect_err("two Codex-home contributors must conflict")
        .to_string();

    assert!(error.contains("CODEX_HOME"), "error must name the target key: {error}");
    assert!(error.contains("agent-material/codex codex-central"), "error must name agent material: {error}");
    assert!(error.contains("credential/codex openai"), "error must name the credential: {error}");
    assert!(provider.create_opts.lock().await.is_none(), "conflicting config must fail before creating a container");
}

#[tokio::test]
async fn an_unprovisioned_central_codex_credential_fails_before_registry_preflight() {
    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"codex-central-missing-test\"\n").expect("daemon config");
    let home = temp.path().join("home");
    let skill_sources = write_test_skill_sources(temp.path());
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
        "docker",
        flotilla_core::providers::discovery::ProviderDescriptor::named(
            flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
            "docker",
        ),
        Arc::clone(&provider) as Arc<dyn EnvironmentProvider>,
    );
    let registry_runner = Arc::new(RejectingRegistryRunner::default());
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
    let state = Arc::new(
        ControllerRuntimeState::new(
            daemon,
            Arc::clone(&config),
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_environment_tools(fixed_environment_tools(config.state_dir().as_path().to_path_buf()))
        .with_credential_store(credential_store)
        .with_agent_material(agent_material),
    );
    let spec = flotilla_resources::DockerEnvironmentSpec {
        image_composition: None,
        image_build_ref: None,
        memory_policy: Default::default(),
        host_ref: "host-test".to_string(),
        image: "registry.example/crew:latest".to_string(),
        declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
        required_agent_adapters: BTreeSet::from(["codex".to_string()]),
        pull_policy: Default::default(),
        mounts: Vec::new(),
        env: BTreeMap::from([(
            CREDENTIAL_REFS_ENV.to_string(),
            serde_json::to_string(&BTreeSet::from(["private-registry".to_string()])).expect("encode credential refs"),
        )]),
    };

    let error = DockerControllerRuntime { state }
        .provision("unprovisioned-environment", &spec)
        .await
        .expect_err("a host with no central Codex login cannot provision a Codex crew");

    assert!(error.contains("codex-central/auth.json"), "the failure must name the central path: {error}");
    assert!(error.contains("not provisioned"), "the failure must say the host lacks the credential: {error}");
    assert_eq!(registry_runner.calls.load(Ordering::SeqCst), 0, "registry login and pull must not run before material is available");
    assert!(provider.create_opts.lock().await.is_none(), "provider create must not run without agent material");
    assert!(
        !config.state_dir().as_path().join("credential-runtime").exists(),
        "a failed provision must not leave a credential cache on disk"
    );
}

// #2487: the actual work-credential pass can stall after environment adoption
// while fleet health remains available, then stage the live crew's material.
#[tokio::test(start_paused = true)]
async fn slow_work_credential_phase_keeps_heartbeat_available_and_completes() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"slow-credentials-test\"\n").expect("daemon identity");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), config.clone()).await;
    let runtime = DaemonRuntime::start_with_options(
        daemon.clone(),
        config.clone(),
        None,
        RuntimeOptions { start_controllers: false, ..RuntimeOptions::default() },
    )
    .await
    .expect("publish initial heartbeat");
    // #2220: runtime installs the ensure controller even when background
    // loops are disabled, so explicit daemon entry points still delegate.
    assert!(daemon.reconcile_convoy_ensures_once(NAMESPACE).await.expect("installed ensure controller").is_empty());
    let backend = daemon.resource_backend();
    backend
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("work-token"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Claude,
                source: CredentialSource::Env { name: "TEST_WORK_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Issued,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("credential declaration");
    let runner = Arc::new(GatedCredentialPreflight::new());
    let store = Arc::new(CredentialStore::new(
        backend.clone(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_WORK_TOKEN", "fake-test-token")])),
        EnvironmentBag::new(),
        runner.clone(),
        temp.path().to_path_buf(),
    ));
    let env_id = EnvironmentId::new("env-work");
    daemon
        .register_provisioned_environment(
            env_id.clone(),
            Arc::new(TestInteriorEnvironment {
                id: env_id.clone(),
                image: ImageId::new("test-image"),
                runner: runner.clone(),
                env_vars: HashMap::new(),
                destroyed: Arc::new(AtomicBool::new(false)),
            }),
            EnvironmentBag::new(),
            Some(passthrough_registry()),
        )
        .expect("adopt test environment");
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(&empty_meta("credential-work"), &ConvoySpec::builder().workflow_ref("test".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .credential_refs(BTreeSet::from(["work-token".to_string()]))
                        .crew(Vec::new())
                        .build()],
                }),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("live convoy");
    let vessels = backend.using::<Vessel>(NAMESPACE);
    let vessel = vessels
        .create(
            &empty_meta("credential-work-vessel"),
            &VesselSpec {
                convoy_ref: "credential-work".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    vessels
        .update_status(
            &vessel.metadata.name,
            &vessel.metadata.resource_version,
            &VesselStatus {
                phase: flotilla_resources::VesselPhase::Ready,
                environment_ref: Some(env_id.to_string()),
                ..VesselStatus::default()
            },
        )
        .await
        .expect("placed vessel");
    create_credential_test_session(&backend, "live-crew", "credential-work", "credential-work-vessel", env_id.as_str()).await;
    let state =
        ControllerRuntimeState::new(daemon.clone(), config, passthrough_registry(), None, "test-host".into(), "host-direct-test".into())
            .with_credential_store(store.clone());
    let log = tempfile::NamedTempFile::new().expect("timing log");
    let subscriber = tracing_subscriber::fmt().json().with_ansi(false).with_writer(log.reopen().expect("writer")).finish();
    // The test uses Tokio's current-thread runtime, so the scoped dispatcher
    // covers both the spawned phase and its cancellation/drop path.
    let _logging = tracing::subscriber::set_default(subscriber);
    let phase_started = tokio::time::Instant::now();
    let task = tokio::spawn(async move { phase("reconcile_work_credentials", reconcile_work_credentials(&state, NAMESPACE)).await });
    tokio::time::timeout(Duration::from_secs(1), runner.entered.notified()).await.expect("work credential preflight starts");
    tokio::time::advance(Duration::from_millis(38_650)).await;
    assert!(!task.is_finished(), "credential preflight is still pending");
    let health = daemon.fleet_health_internal().await.expect("fleet health while staging is blocked");
    assert!(health.hosts.iter().any(|host| host.is_local && host.heartbeat_at.is_some()));
    let before_deadline = fs::read_to_string(log.path()).expect("phase log");
    assert!(!before_deadline.contains("daemon startup phase remains pending"), "no early warning");
    // Reach the phase deadline without depending on earlier virtual-time steps.
    tokio::time::advance(PENDING_WARNING_THRESHOLD.saturating_sub(phase_started.elapsed())).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "warning leaves preflight pending");
    let health = daemon.fleet_health_internal().await.expect("fleet health after warning");
    assert!(health.hosts.iter().any(|host| host.is_local && host.heartbeat_at.is_some()));
    runner.release.add_permits(1);
    task.await.expect("reconciliation task").expect("credential staging completes");
    assert_eq!(store.tracked_work_deliveries().await.get(env_id.as_str()), Some(&BTreeSet::from(["work-token".to_string()])));
    tokio::time::advance(Duration::from_secs(120)).await;
    let records = fs::read_to_string(log.path()).expect("phase log");
    let warnings = records
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON"))
        .filter(|record| record["fields"]["phase"] == "reconcile_work_credentials" && record["level"] == "WARN")
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "one warning, with no stale timer after completion");
    assert_eq!(warnings[0]["fields"]["elapsed_ms"], 60_000);
    let finish = records
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON"))
        .find(|record| record["fields"]["phase"] == "reconcile_work_credentials" && record["fields"]["status"] == "completed")
        .unwrap_or_else(|| panic!("credential phase timing absent from log: {records}"));
    assert!(finish["fields"]["duration_ms"].as_f64().expect("duration") >= 38_650.0);
    runtime.shutdown();
}
