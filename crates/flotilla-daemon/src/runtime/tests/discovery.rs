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
    // #3792: vessel agents live in a short runtime directory.
    let runtime_dir = flotilla_test_support::TestSocketDir::new();
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
        host_environment: Default::default(),
        max_vessels: Some(2),
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
        vessel_agent: Err("the test's vessel agents run on the host runner".into()),
    };
    register_agentless_ssh_resources(&daemon.resource_backend(), NAMESPACE, &local_host_id, &profile)
        .await
        .expect("register SSH resources");
    profile
        .fulfilment_facts
        .write()
        .await
        .insert("host-direct-ssh-test-host".into(), flotilla_resources::FulfilmentFacts { observed_at: Utc::now(), ..Default::default() });
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create remote Claude dir");
    std::fs::write(claude_dir.join(".credentials.json"), r#"{"claudeAiOauth":{"expiresAt":1756000000000}}"#)
        .expect("write remote credential metadata");
    apply_agentless_ssh_observation(&daemon, NAMESPACE, &profile, None, None).await.expect("publish SSH observation");
    let host = daemon.resource_backend().using::<Host>(NAMESPACE).get(host_id).await.expect("SSH Host");
    // #3793: the owning daemon declares the configured cap and publishes the
    // free slots it counts for each of the host's kinds.
    assert_eq!(host.spec.max_vessels, Some(2));
    let status = host.status.expect("SSH status");
    assert_eq!(status.fulfilment_facts.get("host-direct-ssh-test-host").and_then(|facts| facts.free_vessel_slots), Some(2));
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
        .with_agentless_vessels(Some(vessels_over(
            RunnerVesselAgents::new(Arc::new(ProcessCommandRunner), Arc::new(ProviderRegistry::new())),
            temp.path(),
        )))
        .with_environment_tools(EnvironmentToolProvisioner::new(Vec::new())),
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
    {
        let commands = commands.lock().expect("command log");
        assert!(commands.iter().any(|command| command == "git"));
    }
    // #3792: only crew sessions run on an agentless host, each in its
    // vessel's agent; a tool session names no vessel and is refused.
    let terminal = TerminalControllerRuntime { state: Arc::clone(&state) };
    let error = terminal
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
        .expect_err("a tool session on an agentless host has no vessel");
    assert!(error.contains("runs only crew sessions"), "{error}");
    assert!(pool.ensured.lock().await.is_empty());
    environments.cleanup(environment_id.as_str()).await.expect("host-direct Environment is released on the SSH host");
}

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

use super::agentless_vessels::{AgentlessVessels, HostPlatform, SshVesselAgents};
use super::vessel_support::{vessels_over, RunnerVesselAgents};

// #3792: a host this controller cannot run vessel agents on is refused at
// placement with the reason: no agent reach, no runtime directory, a platform
// whose tools this generation lacks, or no agent build. A platform never
// probed successfully (an SSH blip) is diagnostic only.
#[tokio::test]
async fn agentless_host_without_vessel_agents_is_refused_at_placement() {
    let system = if cfg!(target_os = "macos") { "Darwin" } else { "Linux" };
    assert_eq!(HostPlatform::from_uname(system, std::env::consts::ARCH), HostPlatform::controller());
    let controller_uname = "Linux x86_64\n";
    let state = TempDir::new().expect("state");
    let build = state.path().join("drydock-agent");
    fs::write(&build, b"agent").expect("agent build");
    let found = build.clone();
    let vessels = AgentlessVessels::new(
        Arc::new(SshVesselAgents::with_agent_build(Box::new(move |_| Ok(found.clone())))),
        vessel_support::held_socket_service(state.path()),
        state.path().join("vessels"),
    );
    let reach = flotilla_environment::environment::agent::host::SshAgentReach {
        address: "b1.example".into(),
        port: 22,
        user: "crew".into(),
        host_key: "ssh-ed25519 AAAA".into(),
        identity_file: state.path().join("key"),
    };
    let profile = |id: &str, uname: Option<&str>, runtime: bool, reach: Result<_, String>| AgentlessSshProfile {
        provisioning: manual_profile(id, false),
        environment_id: EnvironmentId::new(format!("host-direct-{id}")),
        destination: format!("crew@{id}"),
        env_bag: if runtime {
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", "/run/user/1000"))
        } else {
            EnvironmentBag::new()
        }
        .with(EnvironmentAssertion::env_var("HOME", "/home/crew")),
        runner: Arc::new(match uname {
            Some(uname) => DiscoveryMockRunner::builder().on_run("uname", &["-s", "-m"], Ok(uname.into())).build(),
            None => DiscoveryMockRunner::builder().build(),
        }),
        host_environment: Default::default(),
        max_vessels: None,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
        vessel_agent: reach,
    };
    let now = chrono::Utc::now();
    let refused = |conditions: Vec<flotilla_resources::HostCondition>, ready: bool| {
        let status = flotilla_resources::HostStatus { ready, heartbeat_at: Some(now), conditions, ..Default::default() };
        flotilla_admission::support::unready_placement_refusal("host-direct-b1", "b1", Some(&status), now)
    };

    let (conditions, ready) =
        super::discovery::agentless_vessel_conditions(Some(&vessels), &profile("b1", Some(controller_uname), true, Ok(reach.clone())))
            .await;
    assert!(ready, "{conditions:?}");
    assert!(conditions.is_empty());

    let (conditions, ready) =
        super::discovery::agentless_vessel_conditions(Some(&vessels), &profile("b2", Some("Linux riscv64\n"), true, Ok(reach.clone())))
            .await;
    assert!(!ready);
    let refusal = refused(conditions, ready).expect("refused");
    assert!(refusal.contains("NoPlatformBuild") && refusal.contains("linux-riscv64"), "{refusal}");

    let missing = Err("hosts.toml [hosts.b3.vessel_agent] is not set".to_string());
    let (conditions, ready) =
        super::discovery::agentless_vessel_conditions(Some(&vessels), &profile("b3", Some(controller_uname), false, missing)).await;
    assert!(!ready);
    let reasons = conditions.iter().map(|condition| condition.reason.clone()).collect::<Vec<_>>();
    assert_eq!(reasons, ["MissingReach", "NoRuntimeDirectory"]);
    assert!(refused(conditions, ready).expect("refused").contains("vessel_agent"));

    let (conditions, ready) =
        super::discovery::agentless_vessel_conditions(Some(&vessels), &profile("b4", None, true, Ok(reach.clone()))).await;
    assert!(ready);
    assert!(matches!(conditions.as_slice(), [condition] if condition.reason == "PlatformUnknown" && !condition.blocks_readiness()));

    let no_build = AgentlessVessels::new(
        Arc::new(SshVesselAgents::with_agent_build(Box::new(|_| Err("install drydock-agent beside flotillad".into())))),
        vessel_support::held_socket_service(state.path()),
        state.path().join("vessels"),
    );
    let (conditions, ready) =
        super::discovery::agentless_vessel_conditions(Some(&no_build), &profile("b5", Some(controller_uname), true, Ok(reach))).await;
    assert!(!ready);
    assert!(matches!(conditions.as_slice(), [condition] if condition.reason == "NoAgentBuild"), "{conditions:?}");
}
