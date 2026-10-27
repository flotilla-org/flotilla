//! #3980: a cloud vessel is reached through drydock's own agent connection. The
//! drydock stand-in is the real main-server runtime with the reference module,
//! whose vessels declare an agent reached over SSH, and drydock's in-process
//! gateway, which serves a real agent under that reach's forwarding rules. The
//! stand-in vessel is this host, so the home lands at the contained base here.
use super::*;
use ::drydock::agent::{stand_in::Gateway, AgentLink, AgentReach};
use flotilla_environment::environment::{
    drydock::DrydockEnvironmentProvider, EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind,
    ProvisionOpts,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BUILD: &[u8] = b"drydock launch scenario agent";

// Acceptance (#3980 contracts 1, 2, 5 and 6): a crew aboard a drydock vessel
// launches through the real launch path with every guest step on drydock's
// agent. Whether the provider's SSH server refuses reverse forwards (Daytona's
// gateway) or permits them, the guest reaches the daemon socket through the
// agent's guest forward, and the crew's home and skills land under the home its
// harness launches with. Explain shows what drydock reports it chose.
#[tokio::test]
async fn cloud_crew_launches_through_the_drydock_agent_over_either_link() {
    for (reverse_forwards, link) in [(false, AgentLink::LocalForward), (true, AgentLink::ReverseForward)] {
        launch_through_drydock(reverse_forwards, link).await;
    }
}

async fn launch_through_drydock(reverse_forwards: bool, link: AgentLink) {
    let temp = tempfile::Builder::new().prefix("dl-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path();
    let convoy = format!("drydock-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);

    // The drydock stand-in and the provider instance flotilla composes for it.
    let runtime = ::drydock::runtime::Runtime::new(drydock_host::reference::registry());
    let reach = json!({"kind": "ssh", "endpoint": "gateway.test:22", "user": "minted", "host_key": "ssh-ed25519 AAAA",
        "reverse_forwards": reverse_forwards});
    let configuration = serde_json::from_value(json!({
        "modules": [{"name": "memory", "entrypoint": "reference"}],
        "providers": [{"name": "stand-in", "module": "memory",
            "settings": {"agent": true, "reach": reach, "workspace_root": root.join("workspace")},
            "credential": {"mode": "direct", "credential_reference": "test"}}]
    }))
    .expect("configuration");
    runtime.configure(&configuration).await.expect("configure drydock");
    runtime.credential("stand-in", drydock_protocol::Secret::new("stand-in-key")).await.expect("credential");
    fs::create_dir(root.join("vessel")).expect("vessel directory");
    let gateway = Gateway::start(&root.join("vessel"), reverse_forwards, BUILD).expect("gateway");
    let provider = DrydockEnvironmentProvider::new(Arc::new(runtime.clone()), "stand-in".into(), "reference".into(), root.join("keys"))
        .with_agent_reach(AgentReach::new(AgentLink::default()).with_dialer(gateway.clone()).with_timeout(Duration::from_secs(5)))
        .with_agent_build(BUILD.to_vec())
        .with_attach_root(root.join("attach"));
    let env_id = EnvironmentId::new("cloud-crew");
    let spec = EnvironmentSpec {
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
            probe_needs: Default::default(),
            image: format!("example.test/crew@sha256:{}", "a".repeat(64)),
            size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1024, disk_bytes: 1024 },
            lifetime_cap_seconds: None,
            env: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let prepared = provider.prepare(&spec, &Default::default()).await.expect("prepare");
    // Provisioning hands the vessel the daemon socket as a tool asset.
    let daemon_socket = tokio::net::UnixListener::bind(root.join("daemon.sock")).expect("daemon socket");
    let guest_socket = root.join("vessel/run/flotilla.sock");
    let tool = EnvironmentTool::new("flotilla", root.join("vessel/opt/flotilla")).with_asset(EnvironmentToolAsset::new(
        root.join("daemon.sock"),
        &guest_socket,
        EnvironmentToolAssetKind::UnixSocket,
        EnvironmentToolAssetAccess::SharedWritable,
        "the daemon socket",
    ));
    let handle =
        provider.provision(env_id.clone(), &prepared, ProvisionOpts { tools: vec![tool], ..Default::default() }).await.expect("provision");

    // The daemon, with the crew frozen aboard and its account bound.
    let config_base = root.join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"drydock-launch-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
    let backend = daemon.resource_backend();
    backend
        .clone()
        .definitions::<CredentialSpec>(NAMESPACE)
        .create(
            &empty_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "test@example.com".to_string() },
                source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
                authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
            },
        )
        .await
        .expect("credential declaration");
    let environments = backend.using::<Environment>(NAMESPACE);
    let environment = environments.create(&empty_meta(env_id.as_str()), &spec).await.expect("environment record");
    let status = flotilla_resources::EnvironmentStatus { provider_descriptor: handle.provider_descriptor(), ..Default::default() };
    environments.update_status(env_id.as_str(), &environment.metadata.resource_version, &status).await.expect("environment status");
    let source = LocalSkillSource::new(root, "research");
    freeze_crew(&backend, &convoy, "demo-work", "coder", "claude-code", vec![source.selected.clone()]).await;
    bind_account(&backend, &convoy, "coder", "claude-max", "test@example.com").await;

    // Adapters are discovered over a stand-in runner; every guest step goes
    // through the drydock handle's agent.
    let adapters: Arc<dyn CommandRunner> =
        Arc::new(CredentialInteriorRunner(DiscoveryMockRunner::builder().tool_exists("claude", true).build()));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", env_id.to_string()))
        .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
        .with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
    let pool = Arc::new(FakeTerminalPool::new());
    let mut registry = ProviderRegistry::new();
    registry.agent_adapters = AgentAdapterRegistry::discover(&bag, Arc::clone(&adapters));
    registry.terminal_pools.insert(
        "fake-terminals",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "fake-terminals"),
        pool.clone(),
    );
    daemon.register_provisioned_environment(env_id.clone(), handle.clone(), bag.clone(), Some(Arc::new(registry))).expect("register");
    let credential_store = Arc::new(CredentialStore::new_with_http(
        backend.clone(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        bag,
        adapters,
        Arc::new(ReplayHttpClient::new(Session::replaying_from_str("interactions: []\n", Masks::new()))),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([(
        FLOTILLA_SKILLS_DIR_ENV,
        source.supply.display().to_string(),
    )]))));
    let state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            Arc::new(ProviderRegistry::new()),
            None,
            "host-test".to_string(),
            "host-direct-host-test".to_string(),
        )
        .with_credential_store(credential_store)
        .with_agent_material(agent_material)
        .with_skill_trees(LocalSkillSource::trees(root)),
    );
    state
        .provisioned_environments
        .lock()
        .await
        .insert(handle.backing_id().to_string(), super::super::environments::ActiveProvisionedEnvironment { handle: handle.clone() });
    let spec = flotilla_resources::TerminalSessionSpec {
        env_ref: env_id.to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Implement the issue.".to_string(),
                copies: Vec::new(),
            },
            context: Box::new(flotilla_resources::TerminalCrewContext {
                namespace: NAMESPACE.to_string(),
                convoy: convoy.clone(),
                vessel_ref: "demo-work".to_string(),
            }),
            message: None,
        },
        cwd: root.display().to_string(),
        env: BTreeMap::new(),
        pool: "fake-terminals".to_string(),
    };

    TerminalControllerRuntime { state }.ensure_session("terminal-demo-work-coder", (&spec).into(), &[]).await.expect("launch");

    let home = Path::new(flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE)
        .join("agent-homes/requesters")
        .join(&convoy)
        .join("coder/claude");
    let launched = pool
        .ensured
        .lock()
        .await
        .iter()
        .find_map(|launch| launch.env_vars.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, home)| PathBuf::from(home)));
    assert_eq!(launched.as_ref(), Some(&home), "launch uses the home the access layer delivered");
    // #3663: the home's skills link resolves into the agent's content cache.
    assert!(fs::symlink_metadata(home.join("skills")).expect("skills link").file_type().is_symlink());
    assert!(fs::read_to_string(home.join("skills/research/SKILL.md")).expect("pushed skill").contains("name: research"));
    let mut guest = tokio::net::UnixStream::connect(&guest_socket).await.expect("guest daemon socket");
    guest.write_all(b"crew").await.expect("guest write");
    let (mut controller, _) = tokio::time::timeout(Duration::from_secs(5), daemon_socket.accept()).await.expect("accept").expect("stream");
    let mut received = [0; 4];
    controller.read_exact(&mut received).await.expect("controller read");
    assert_eq!(&received, b"crew", "the guest reaches the daemon socket through the agent forward");
    let deliveries =
        flotilla_projections::read_projections::access_delivery_evidence(&backend, NAMESPACE, &convoy).await.expect("deliveries");
    let [delivery] = deliveries.as_slice() else { panic!("one access delivered: {deliveries:?}") };
    assert_eq!(Path::new(&delivery.home), home);
    let mechanisms = delivery.needs.iter().map(|need| (need.need.as_str(), need.mechanism.as_str())).collect::<BTreeMap<_, _>>();
    assert_eq!(mechanisms.get("home"), Some(&"agent-push"));
    assert_eq!(mechanisms.get("skills"), Some(&"agent-push"));
    let connection = delivery.agent_connection.as_ref().expect("explain shows drydock's report");
    assert_eq!((connection.provider.as_str(), connection.mode.as_str()), ("stand-in", link.as_str()));
    assert_eq!(connection.vessel, handle.backing_id());
    assert!(gateway.links().iter().all(|dialled| *dialled == link), "never a link the gateway refuses: {:?}", gateway.links());
    handle.destroy().await.expect("destroy");
    // The stand-in vessel is this host: leave nothing at its contained base.
    let _ = fs::remove_dir_all(home.parent().and_then(Path::parent).expect("convoy homes"));
}
