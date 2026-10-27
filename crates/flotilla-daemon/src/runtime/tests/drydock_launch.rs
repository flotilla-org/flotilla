//! #3981: a cloud vessel is provisioned through the generic provider path and
//! reaches launch on its vessel and access layers alone. The drydock stand-in is
//! the real main-server runtime with the reference module, whose vessels declare
//! an agent reached over SSH, and drydock's in-process gateway, which serves a
//! real agent and refuses reverse forwards, as Daytona's does. The stand-in
//! vessel is this host, so guest paths land here.
use super::*;
use ::drydock::agent::{stand_in::Gateway, AgentLink, AgentReach};
use flotilla_discovery_api::HostDetector;
use flotilla_environment::environment::{
    drydock::DrydockEnvironmentProvider, EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind,
    DAEMON_SOCKET_ENV,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::environment_tools::{EnvironmentToolContext, EnvironmentToolFactory, EnvironmentToolProviderKind, DOCKER_PROVIDER_KIND};
use crate::server::environment_sockets::{EnvironmentSocketRegistry, EnvironmentSocketService, SocketReach};

const BUILD: &[u8] = b"drydock launch scenario agent";

// Acceptance (#3981 contracts 1 and 2): a cloud-sandbox Environment, as the
// vessel reconciler writes it, is provisioned by its own provider through the
// generic environment runtime. The vessel layer lands through the drydock agent:
// the generation's tools on the vessel's PATH and the crew socket, which reaches
// this environment's own socket here. A crew aboard then launches through the
// real launch path with its home and skills from the access layer.
#[tokio::test]
async fn cloud_vessel_reaches_launch_through_the_generic_provider_path() {
    let temp = tempfile::Builder::new().prefix("dl-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path();
    let convoy = format!("drydock-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let env_id = EnvironmentId::new(format!("env-{convoy}"));

    // The drydock stand-in and the provider instance flotilla composes for it.
    let runtime = ::drydock::runtime::Runtime::new(drydock_host::reference::registry());
    let reach = json!({"kind": "ssh", "endpoint": "gateway.test:22", "user": "minted", "host_key": "ssh-ed25519 AAAA",
        "reverse_forwards": false});
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
    // The provider prepares linux/amd64 guests, so the stand-in vessel reports
    // that platform on any test host.
    let gateway = Gateway::start_as(&root.join("vessel"), false, BUILD, flotilla_environment::guest_tools::linux_amd64()).expect("gateway");
    let provider = DrydockEnvironmentProvider::new(Arc::new(runtime.clone()), "stand-in".into(), "reference".into(), root.join("keys"))
        .with_agent_reach(AgentReach::new(AgentLink::default()).with_dialer(gateway.clone()).with_timeout(Duration::from_secs(5)))
        .with_agent_build(BUILD.to_vec())
        .with_attach_root(root.join("attach"));

    // The daemon: interior discovery finds the harness and the terminal pool.
    let config_base = root.join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"drydock-launch-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let pool = Arc::new(FakeTerminalPool::new());
    let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(pool.clone()));
    discovery.host_detectors = Arc::new(vec![Box::new(GuestHarness) as Box<dyn HostDetector>]);
    let git = git_process_discovery(false);
    discovery.factories.vcs = git.factories.vcs;
    discovery.repo_detectors = git.repo_detectors;
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        discovery,
        flotilla_protocol::HostName::new("controller"),
        Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
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
    let address: flotilla_address::Address = "account:stand-in/provider:stand-in".parse().expect("address");
    let spec = EnvironmentSpec {
        provider_address: Some(address.clone()),
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
            probe_needs: Default::default(),
            image: format!("example.test/crew@sha256:{}", "a".repeat(64)),
            size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1024, disk_bytes: 1024 },
            lifetime_cap_seconds: None,
            env: BTreeMap::from([("LANG".to_string(), "C.UTF-8".to_string())]),
        }),
        ..Default::default()
    };
    backend.using::<Environment>(NAMESPACE).create(&empty_meta(env_id.as_str()), &spec).await.expect("environment record");
    let source = LocalSkillSource::new(root, "research");
    freeze_crew(&backend, &convoy, "demo-work", "coder", "claude-code", vec![source.selected.clone()]).await;
    bind_account(&backend, &convoy, "coder", "claude-max", "test@example.com").await;

    let adapters: Arc<dyn CommandRunner> =
        Arc::new(CredentialInteriorRunner(DiscoveryMockRunner::builder().tool_exists("claude", true).build()));
    let credential_store = Arc::new(CredentialStore::new_with_http(
        backend.clone(),
        NAMESPACE,
        Arc::new(TestEnvVars::new([("TEST_CLAUDE_TOKEN", "oauth-secret-material")])),
        EnvironmentBag::new(),
        adapters,
        Arc::new(ReplayHttpClient::new(Session::replaying_from_str("interactions: []\n", Masks::new()))),
        config.state_dir().as_path().to_path_buf(),
    ));
    let agent_material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([(
        FLOTILLA_SKILLS_DIR_ENV,
        source.supply.display().to_string(),
    )]))));
    let (accepted, mut crew_connections) = tokio::sync::mpsc::unbounded_channel();
    let sockets = EnvironmentSocketService::new(
        Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        DaemonHostPath::new(root.join("sockets")),
        move |listener, environment, reach| {
            let accepted = accepted.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let _ = accepted.send((environment.clone(), reach, stream));
                }
            })
        },
    );
    let mut state = ControllerRuntimeState::new(
        Arc::clone(&daemon),
        Arc::clone(&config),
        Arc::new(ProviderRegistry::new()),
        None,
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    )
    .with_credential_store(credential_store)
    .with_agent_material(agent_material)
    .with_skill_trees(LocalSkillSource::trees(root))
    .with_environment_sockets(Some(sockets));
    state.environment_providers.insert(address, Arc::new(provider)).expect("provider address");
    state.environment_tools = EnvironmentToolProvisioner::new(vec![Arc::new(GenerationCli { root: root.to_path_buf() })]);
    let state = Arc::new(state);

    // Provisioned as the Environment reconciler provisions it.
    let provisioned =
        DockerControllerRuntime { state: Arc::clone(&state) }.provision_environment(env_id.as_str(), &spec).await.expect("provision");
    assert!(provisioned.provider_descriptor.is_some(), "the cloud provider describes itself");
    let handle = state
        .provisioned_environments
        .lock()
        .await
        .values()
        .find(|active| active.handle.id() == &env_id)
        .map(|active| active.handle.clone())
        .expect("provisioned handle");

    // The vessel layer: the generation's CLI is on the vessel's PATH, and the
    // socket the vessel's environment names reaches this environment's socket.
    let agent = handle.agent();
    let commands = agent.commands();
    let cli = commands.run("sh", &["-c", "command -v flotilla"], Path::new("/"), &ChannelLabel::Default).await.expect("CLI on PATH");
    assert_eq!(cli.trim(), root.join("vessel/opt/flotilla/bin/flotilla").display().to_string());
    let named = format!("printf %s \"${DAEMON_SOCKET_ENV}\"");
    let socket = commands.run("sh", &["-c", &named], Path::new("/"), &ChannelLabel::Default).await.expect("socket variable");
    let guest_socket = PathBuf::from(socket);
    let mut guest = tokio::net::UnixStream::connect(&guest_socket).await.expect("guest crew socket");
    guest.write_all(b"crew").await.expect("guest write");
    let (environment, reach, mut controller) =
        tokio::time::timeout(Duration::from_secs(5), crew_connections.recv()).await.expect("accept").expect("crew connection");
    assert_eq!((environment, reach), (env_id.clone(), SocketReach::Forwarded), "the vessel's own forwarded socket");
    let mut received = [0; 4];
    controller.read_exact(&mut received).await.expect("controller read");
    assert_eq!(&received, b"crew");

    let session = flotilla_resources::TerminalSessionSpec {
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
        cwd: root.join("skill-source").display().to_string(),
        env: BTreeMap::new(),
        pool: "fake-terminals".to_string(),
    };
    TerminalControllerRuntime { state }.ensure_session("terminal-demo-work-coder", (&session).into(), &[]).await.expect("launch");

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
    let deliveries =
        flotilla_projections::read_projections::access_delivery_evidence(&backend, NAMESPACE, &convoy).await.expect("deliveries");
    let [delivery] = deliveries.as_slice() else { panic!("one access delivered: {deliveries:?}") };
    let mechanisms = delivery.needs.iter().map(|need| (need.need.as_str(), need.mechanism.as_str())).collect::<BTreeMap<_, _>>();
    assert_eq!(mechanisms.get("home"), Some(&"agent-push"));
    assert_eq!(mechanisms.get("skills"), Some(&"agent-push"));
    assert!(gateway.links().iter().all(|dialled| *dialled == AgentLink::LocalForward), "never a link the gateway refuses");
    handle.destroy().await.expect("destroy");
    // The stand-in vessel is this host: leave nothing at its contained base.
    let base = Path::new(flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE);
    let _ = fs::remove_dir_all(home.parent().and_then(Path::parent).expect("convoy homes"));
    let _ = fs::remove_dir_all(base.join("agent-material/requesters").join(&convoy));
    let _ = fs::remove_dir_all(guest_socket.parent().expect("crew socket directory"));
    let _ = fs::remove_dir(base.join("crew-sockets"));
}

/// The vessel's harness, as interior discovery finds it.
struct GuestHarness;

#[async_trait]
impl HostDetector for GuestHarness {
    async fn detect(&self, _: &dyn CommandRunner, _: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        vec![EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"), EnvironmentAssertion::binary("git", "/usr/bin/git")]
    }
}

/// The generation's CLI in the contained layout, rooted in the stand-in vessel,
/// with the daemon socket it reaches, as the CLI tool declares it.
struct GenerationCli {
    root: PathBuf,
}

#[async_trait]
impl EnvironmentToolFactory for GenerationCli {
    fn provider_kinds(&self) -> &[EnvironmentToolProviderKind] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, _: &str, _: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        use std::os::unix::fs::PermissionsExt;
        let cli = self.root.join("generation/bin/flotilla");
        fs::create_dir_all(cli.parent().expect("CLI directory")).map_err(|error| error.to_string())?;
        fs::write(&cli, "#!/bin/sh\n").map_err(|error| error.to_string())?;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).map_err(|error| error.to_string())?;
        let guest = self.root.join("vessel/opt/flotilla/bin");
        Ok(EnvironmentTool::new("flotilla", guest.join("flotilla"))
            .with_environment(EnvironmentVariableUpdate::prepend_path("PATH", guest.to_string_lossy()))
            .with_asset(EnvironmentToolAsset::new(
                &cli,
                guest.join("flotilla"),
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the flotilla CLI",
            ))
            .with_asset(EnvironmentToolAsset::new(
                "/tmp/flotilla.sock",
                "/run/flotilla-daemon/flotilla.sock",
                EnvironmentToolAssetKind::UnixSocket,
                EnvironmentToolAssetAccess::SharedWritable,
                "the daemon socket",
            ))
            .with_environment(EnvironmentVariableUpdate::set(DAEMON_SOCKET_ENV, "/run/flotilla-daemon/flotilla.sock", "the daemon socket")))
    }
}
