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
    // #3618: the session's crews reach the daemon through a forward to a short
    // runtime directory, with the controller's CLI delivered beside it.
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
    let own_cli = temp.path().join("controller-bin/flotilla");
    fs::create_dir_all(own_cli.parent().expect("cli dir")).expect("cli dir");
    fs::write(&own_cli, "#!/bin/sh\n").expect("controller cli");
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
    registry.environment_providers.insert(
        "host-direct",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "host-direct"),
        Arc::new(flotilla_environment::environment::host_direct::HostDirectEnvironmentProvider::new(
            Arc::clone(&runner),
            std::collections::HashMap::new(),
        )) as Arc<dyn EnvironmentProvider>,
    );
    daemon.set_direct_environment_registry(&environment_id, Arc::new(registry)).expect("register SSH providers");
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
            .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string()))
            .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
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
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None, None).await.expect("publish SSH observation");
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
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None, None).await.expect("refresh SSH observation");
    let kind =
        daemon.resource_backend().using::<FulfilmentKind>(NAMESPACE).get("host-direct-ssh-test-host").await.expect("refreshed SSH kind");
    assert_eq!(kind.spec.pool, "alternate-pool");
    let mut unreachable = profile.clone();
    unreachable.runner = Arc::new(DiscoveryMockRunner::builder().on_run("rustc", &["--version"], Ok("rustc 9".into())).build());
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &unreachable, None, None).await.expect("publish unreachable SSH observation");
    let unreachable_status =
        daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id).await.expect("SSH Host").status.expect("unreachable SSH status");
    assert!(!unreachable_status.ready);
    assert!(unreachable_status.fulfilment_facts.is_empty(), "unreachable SSH host must not retain or probe facts");
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None, None).await.expect("restore SSH observation");
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            local_host_id.clone(),
            format!("host-direct-{local_host_id}"),
        )
        .with_agentless_ssh(vec![profile])
        .with_agentless_reach(Some(Arc::new(AgentlessReach::new(
            Arc::new(ListeningSsh),
            test_socket_service(temp.path()),
            CliSource::new(Ok(own_cli.clone()), "gen-1"),
            ForwardTiming::default(),
        )))),
    );
    // The SSH host's host-direct Environment resolves to the provider in the
    // SSH environment's registry, not the owner's.
    let environment =
        daemon.resource_backend().using::<Environment>(NAMESPACE).get(environment_id.as_str()).await.expect("SSH host-direct Environment");
    assert_eq!(
        environment.spec.provider_address.as_ref().expect("new SSH environments write an address").to_string(),
        format!("host:{host_id}/provider:host-direct")
    );
    let environments = DockerControllerRuntime { state: Arc::clone(&state) };
    // A labelled preceding-generation record may carry a host display alias.
    // Resolving its label must preserve the SSH place authority and instance.
    let legacy_meta = InputMeta::builder()
        .name("legacy-ssh-alias".into())
        .labels(BTreeMap::from([(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL.into(), "host-direct".into())]))
        .build();
    daemon
        .resource_backend()
        .store()
        .create_raw(
            flotilla_store::ResourceKind::of::<Environment>(),
            NAMESPACE,
            &legacy_meta,
            &json!({"host_direct":{"host_ref":"beaufort", "repo_default_dir":"/work"}}),
        )
        .await
        .expect("legacy SSH record");
    let selected = environments.provider_for_environment("legacy-ssh-alias").await.expect("SSH alias provider");
    let expected = environments.provider_for_environment(environment_id.as_str()).await.expect("SSH provider");
    assert!(Arc::ptr_eq(&selected, &expected), "alias and canonical place must retain the same instance");

    environments
        .provision_environment(environment_id.as_str(), &environment.spec)
        .await
        .expect("host-direct Environment provisions on the SSH host");
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
    let terminal = TerminalControllerRuntime { state: Arc::clone(&state) };
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
    {
        let ensured = pool.ensured.lock().await;
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].cwd.as_path(), worktree_path.as_path());
        let env = |name: &str| ensured[0].env_vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let socket = env("FLOTILLA_DAEMON_SOCKET").expect("forwarded daemon socket");
        assert!(socket.starts_with(&runtime_dir.path().display().to_string()), "{socket}");
        assert_eq!(env("FLOTILLA_ENVIRONMENT_ID").as_deref(), Some(environment_id.as_str()));
        let path = env("PATH").expect("PATH");
        let cli_directory = path.split(':').next().expect("CLI directory");
        assert!(Path::new(cli_directory).join("flotilla").is_file(), "CLI delivered to {cli_directory}");
        let commands = commands.lock().expect("command log");
        assert!(commands.iter().any(|command| command == "git"));
        assert!(commands.iter().any(|command| command == "mv"));
    }
    environments.cleanup(environment_id.as_str()).await.expect("host-direct Environment is released on the SSH host");
}

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

use super::agentless_reach::{AgentlessReach, CliSource, ForwardTiming, HostPlatform};
use crate::server::environment_sockets::{EnvironmentSocketRegistry, EnvironmentSocketService};

/// A real environment-socket service whose accept loop holds connections open.
fn test_socket_service(state: &Path) -> EnvironmentSocketService {
    EnvironmentSocketService::new(
        Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        DaemonHostPath::new(state.join("env")),
        |listener, _, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        },
    )
}

// #3618: a host whose platform has no flotilla CLI in the controller's
// generation is refused at placement with the reason; a host on the
// controller's own platform is served by the controller's CLI.
#[tokio::test]
async fn agentless_host_without_a_platform_cli_is_refused_at_placement() {
    let controller = HostPlatform::controller();
    let controller_uname = format!(
        "{} {}\n",
        if controller.os == "macos" { "Darwin" } else { "Linux" },
        if controller.os == "macos" && controller.arch == "aarch64" { "arm64" } else { controller.arch.as_str() }
    );
    let state = TempDir::new().expect("state");
    let sockets = test_socket_service(state.path());
    let reach = AgentlessReach::new(
        Arc::new(ProcessCommandRunner),
        sockets,
        CliSource::new(Ok(state.path().join("bin/flotilla")), "gen-1"),
        ForwardTiming::default(),
    );
    let profile = |uname: &str| AgentlessSshProfile {
        provisioning: manual_profile("b1", false),
        environment_id: EnvironmentId::new("host-direct-b1"),
        destination: "crew@b1".into(),
        env_bag: EnvironmentBag::new(),
        runner: Arc::new(DiscoveryMockRunner::builder().on_run("uname", &["-s", "-m"], Ok(uname.into())).build()),
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
    };
    let now = chrono::Utc::now();

    let (conditions, cli_available) = super::discovery::agentless_reach_conditions(&reach, &profile("Linux riscv64\n")).await;
    assert!(!cli_available);
    let status = flotilla_resources::HostStatus { ready: cli_available, heartbeat_at: Some(now), conditions, ..Default::default() };
    let refusal = flotilla_admission::support::unready_placement_refusal("host-direct-b1", "b1", Some(&status), now).expect("refused");
    assert!(refusal.contains("NoPlatformBuild") && refusal.contains("linux-riscv64"), "{refusal}");

    let (conditions, cli_available) = super::discovery::agentless_reach_conditions(&reach, &profile(&controller_uname)).await;
    assert!(cli_available, "{conditions:?}");
    assert!(conditions.is_empty(), "no forward has been dialled, so none is reported down");

    // Review #3621: a platform never probed successfully (an ssh blip) is
    // diagnostic and does not refuse placement.
    let mut blip = profile("unused");
    blip.environment_id = EnvironmentId::new("host-direct-b2");
    blip.runner = Arc::new(DiscoveryMockRunner::builder().build());
    let (conditions, cli_available) = super::discovery::agentless_reach_conditions(&reach, &blip).await;
    assert!(cli_available);
    assert!(matches!(conditions.as_slice(), [condition] if condition.reason == "PlatformUnknown" && !condition.blocks_readiness()));
}

// Unlabelled legacy records derive the approved built-in identity. A compatible
// dummy instance must never steal their binding, even when it is the only one.
#[tokio::test]
async fn legacy_runtime_resolution_matches_canonical_writes_with_only_dummy_registered() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"legacy-resolution\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let host = daemon.local_host_id().expect("local host").to_string();
    let mut registry = ProviderRegistry::new();
    registry.environment_providers.insert(
        "dummy",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "dummy"),
        Arc::new(flotilla_environment::environment::dummy::DummyEnvironmentProvider::new(Default::default())),
    );
    let state = ControllerRuntimeState::new(daemon, config, Arc::new(registry), None, host.clone(), format!("host-direct-{host}"));
    for (input, instance) in [
        (json!({"host_direct":{"host_ref":host, "repo_default_dir":"/work"}}), "host-direct"),
        (json!({"docker":{"host_ref":host, "image":"image"}}), "docker"),
    ] {
        let spec: EnvironmentSpec = serde_json::from_value(input).expect("legacy spec");
        let address = state.provider_address("legacy", &spec, None).await.expect("derived address");
        assert_eq!(address.to_string(), format!("host:{host}/provider:{instance}"));
        assert!(state.environment_providers.get(&address).is_err(), "absence must fail closed instead of choosing dummy");
        let written: EnvironmentSpec = serde_json::from_value(serde_json::to_value(&spec).expect("write")).expect("read");
        assert_eq!(written.provider_address, Some(address));
    }
}
