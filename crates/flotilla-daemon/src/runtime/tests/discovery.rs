use super::*;

#[cfg(unix)]
#[tokio::test]
async fn remote_rustc_wrapper_stages_once_in_a_private_directory() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = tempfile::tempdir().expect("tempdir");
    let runner = ProcessCommandRunner;
    let path = super::stage_remote_rustc_wrapper(&runner, temp.path()).await.expect("stage remote wrapper");
    assert_eq!(path.parent().expect("wrapper parent").parent(), Some(temp.path()));
    assert_eq!(fs::metadata(path.parent().expect("wrapper parent")).expect("directory metadata").permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&path).expect("wrapper metadata").permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::read_to_string(&path).expect("wrapper contents"), super::RUSTC_LINKER_WRAPPER);

    let inode = fs::metadata(&path).expect("wrapper metadata").ino();
    let second = super::stage_remote_rustc_wrapper(&runner, temp.path()).await.expect("reuse remote wrapper");
    assert_eq!(second, path);
    assert_eq!(fs::metadata(&path).expect("wrapper metadata").ino(), inode, "unchanged wrapper should not be replaced");
}

#[tokio::test]
async fn bad_agentless_ssh_host_does_not_block_daemon_startup_or_other_hosts() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"ssh-resilience-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let good_home = temp.path().join("good-home");
    let good_repo_dir = good_home.join(DEFAULT_REPO_DIR_SUFFIX);
    let good_repo_dir = good_repo_dir.to_str().expect("UTF-8 test path");

    for (host_id, runner, env_bag) in [
        (
            "bad-ssh-host",
            Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>,
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("HOME", "/unreachable")),
        ),
        (
            "good-ssh-host",
            Arc::new(
                DiscoveryMockRunner::builder()
                    .on_run("mkdir", &["-p", good_repo_dir], Ok(String::new()))
                    .on_run(
                        "df",
                        &["-Pk", good_repo_dir],
                        Ok("Filesystem 1024-blocks Used Available Capacity Mounted on\nremote 1000 100 900 10% /\n".into()),
                    )
                    .build(),
            ) as Arc<dyn CommandRunner>,
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", good_home.display().to_string()))
                .with(EnvironmentAssertion::binary("cleat", "/usr/bin/cleat")),
        ),
    ] {
        let environment_id = EnvironmentId::new(format!("host-direct-{host_id}"));
        daemon
            .register_direct_environment_for_test(
                environment_id.clone(),
                runner,
                env_bag,
                Some(flotilla_protocol::qualified_path::HostId::new(host_id)),
            )
            .expect("register direct environment");
        daemon.set_direct_environment_ssh_destination_for_test(&environment_id, format!("crew@{host_id}")).expect("set SSH destination");
    }

    // This host passes discovery but registration must reject its collision
    // with the daemon's own Host identity.
    let local_id = daemon.local_host_id().expect("local host identity").to_string();
    let collision_environment_id = EnvironmentId::new(format!("host-direct-{local_id}"));
    daemon
        .register_direct_environment_for_test(
            collision_environment_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", "/colliding-host"))
                .with(EnvironmentAssertion::binary("cleat", "/usr/bin/cleat")),
            Some(flotilla_protocol::qualified_path::HostId::new(&local_id)),
        )
        .expect("register colliding direct environment");
    daemon
        .set_direct_environment_ssh_destination_for_test(&collision_environment_id, "crew@colliding-host".to_string())
        .expect("set colliding SSH destination");

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
    .expect("bad SSH preflight must not abort daemon startup");

    let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
    let local = hosts.get(&local_id).await.expect("local host remains registered");
    assert!(local.spec.connection.is_daemon(), "colliding SSH host must not overwrite the daemon Host");
    assert!(local.status.expect("local heartbeat").ready);
    assert!(hosts.get("bad-ssh-host").await.is_err(), "host without a persistent terminal pool must be skipped");
    let good = hosts.get("good-ssh-host").await.expect("other SSH host remains registered");
    assert!(good.status.expect("other SSH observation").ready);
    assert!(daemon.resource_backend().using::<PlacementPolicy>(NAMESPACE).get("host-direct-good-ssh-host").await.is_ok());

    runtime.shutdown();
}

#[tokio::test]
async fn agentless_ssh_host_provisions_clone_and_worktree_through_its_runner() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).expect("config directory");
    fs::write(config_dir.join("daemon.toml"), "machine_id = \"agentless-ssh-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_dir));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let local_host_id = daemon.local_host_id().expect("local host ID").to_string();
    let host_id = "ssh-test-host";
    let environment_id = EnvironmentId::new(format!("host-direct-{host_id}"));
    let commands = Arc::new(StdMutex::new(Vec::new()));
    let runner: Arc<dyn CommandRunner> = Arc::new(SshProvisioningRecordingRunner { commands: Arc::clone(&commands) });
    daemon
        .register_direct_environment_for_test(
            environment_id.clone(),
            Arc::clone(&runner),
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", temp.path().display().to_string()))
                .with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
            Some(flotilla_protocol::qualified_path::HostId::new(host_id)),
        )
        .expect("register SSH environment");
    let pool = Arc::new(FakeTerminalPool::new());
    let mut registry = ProviderRegistry::new();
    registry.terminal_pools.insert(
        "cleat",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "cleat"),
        Arc::clone(&pool) as Arc<dyn TerminalPool>,
    );
    daemon.set_direct_environment_registry(&environment_id, Arc::new(registry)).expect("register SSH terminal pool");
    let profile = AgentlessSshProfile {
        provisioning: LocalProvisioningProfile {
            repo_default_dir: temp.path().join("ssh-repos").display().to_string(),
            display_name: "beaufort".to_string(),
            host_direct_pool: "cleat".to_string(),
            available_pools: vec!["cleat".to_string()],
            ..manual_profile(host_id, false)
        },
        environment_id: environment_id.clone(),
        destination: "crew@beaufort.example".to_string(),
        env_bag: EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", temp.path().display().to_string()))
            .with(EnvironmentAssertion::env_var("FLOTILLA_PROBE_MODELS", ""))
            .with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
        runner,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
    };
    register_agentless_ssh_resources(&daemon.resource_backend(), NAMESPACE, &local_host_id, &profile)
        .await
        .expect("register SSH resources");
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create remote Claude dir");
    std::fs::write(claude_dir.join(".credentials.json"), r#"{"claudeAiOauth":{"expiresAt":1756000000000}}"#)
        .expect("write remote credential metadata");
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None).await.expect("publish SSH observation");
    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id).await.expect("SSH Host");
    let status = host.status.expect("SSH status");
    assert!(status.ready);
    assert!(status.credential_expiry().expect("SSH credential expiry").contains_key(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE));
    assert!(daemon.resource_backend().using::<PlacementPolicy>(NAMESPACE).get("host-direct-ssh-test-host").await.is_ok());
    let policies = daemon.resource_backend().using::<PlacementPolicy>(NAMESPACE);
    let policy = policies.get("host-direct-ssh-test-host").await.expect("SSH policy");
    let mut changed = policy.spec.clone();
    changed.pool = "alternate-pool".to_string();
    policies
        .update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &changed)
        .await
        .expect("edit SSH policy while the daemon runs");
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None).await.expect("refresh SSH observation");
    let kind =
        daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE).get("host-direct-ssh-test-host").await.expect("refreshed SSH kind");
    assert_eq!(kind.spec.pool, "alternate-pool");
    let mut unreachable = profile.clone();
    unreachable.runner = Arc::new(DiscoveryMockRunner::builder().on_run("rustc", &["--version"], Ok("rustc 9".into())).build());
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &unreachable, None).await.expect("publish unreachable SSH observation");
    let unreachable_status =
        daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id).await.expect("SSH Host").status.expect("unreachable SSH status");
    assert!(!unreachable_status.ready);
    assert!(unreachable_status.fulfilment_facts.is_empty(), "unreachable SSH host must not retain or probe facts");
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None).await.expect("restore SSH observation");
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            local_host_id.clone(),
            format!("host-direct-{local_host_id}"),
        )
        .with_agentless_ssh(vec![profile]),
    );
    let clone_runtime = RoutingCloneRuntime { state: Arc::clone(&state) };
    let clone_path = temp.path().join("ssh-repos/base");
    clone_runtime
        .clone_and_inspect_in(
            environment_id.as_str(),
            source.path().to_str().expect("source path"),
            clone_path.to_str().expect("clone path"),
        )
        .await
        .expect("clone over SSH runner");
    let checkout_runtime = RoutingCheckoutRuntime { state: Arc::clone(&state), change_requests: None };
    let worktree_path = temp.path().join("ssh-repos/work");
    checkout_runtime
        .create_worktree_in(
            environment_id.as_str(),
            clone_path.to_str().expect("clone path"),
            "feature/ssh-crew",
            Some("main"),
            worktree_path.to_str().expect("worktree path"),
            "flotilla-managed: ssh-crew/work",
        )
        .await
        .expect("worktree over SSH runner");
    assert!(worktree_path.join(".git").exists());
    let terminal = TerminalControllerRuntime { state };
    terminal
        .ensure_session(
            "ssh-crew",
            &flotilla_resources::TerminalSessionSpec::builder()
                .env_ref(environment_id.to_string())
                .role("shell".to_string())
                .source(TerminalSessionSource::Tool { command: "sh".to_string() })
                .cwd(worktree_path.display().to_string())
                .pool("cleat".to_string())
                .build(),
            &[],
        )
        .await
        .expect("crew terminal pool provisioned through the SSH environment");
    let ensured = pool.ensured.lock().await;
    assert_eq!(ensured.len(), 1);
    assert_eq!(ensured[0].cwd.as_path(), worktree_path.as_path());
    let commands = commands.lock().expect("command log");
    assert!(commands.iter().any(|command| command == "git"));
    assert!(commands.iter().any(|command| command == "mv"));
}
