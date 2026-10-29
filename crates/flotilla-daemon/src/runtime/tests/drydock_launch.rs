//! #3981: a cloud vessel is provisioned through the generic provider path and
//! reaches launch on its vessel and access layers alone. The drydock stand-in is
//! drydock's daemon, as flotillad embeds it, over a real host with the reference
//! module, whose vessels declare an agent reached over SSH, and drydock's
//! in-process gateway, which serves a real agent under that reach's forwarding
//! rules. The stand-in vessel is this host, so guest paths land here.
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use ::drydock::{
    agent::{stand_in::Gateway, AgentLink},
    daemon::{AgentBuilds, Daemon, HostOption, Seams},
};
use flotilla_discovery_api::HostDetector;
use flotilla_environment::environment::{
    drydock::DrydockEnvironmentProvider, EnvironmentProvisionError, EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess,
    EnvironmentToolAssetKind, DAEMON_SOCKET_ENV,
};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::environment_tools::{EnvironmentToolContext, EnvironmentToolFactory, EnvironmentToolProviderKind, DOCKER_PROVIDER_KIND};
use crate::server::environment_sockets::{directory_name, EnvironmentSocketRegistry, EnvironmentSocketService, SocketReach};

const BUILD: &[u8] = b"drydock launch scenario agent";

// Acceptance (#3981 contracts 1 and 2): a cloud-sandbox Environment, as the
// vessel reconciler writes it, is provisioned by its own provider through the
// generic environment runtime. Whether the provider's SSH server refuses reverse
// forwards (Daytona's gateway) or permits them, the vessel layer lands through
// the drydock agent: the generation's tools on the vessel's own PATH, the
// provider's environment kept, and the crew socket, which reaches this
// environment's own socket here. A crew aboard then launches through the real
// launch path with its home and skills from the access layer, and explain shows
// how the agent is reached: through drydock's exposure, over the link drydock chose.
#[tokio::test]
async fn cloud_vessel_reaches_launch_through_the_generic_provider_path() {
    // The provider declares the vessel's PATH, or the image keeps its own (here, this host's).
    for (reverse_forwards, link, declared_path) in
        [(false, AgentLink::LocalForward, Some("/opt/image/bin:/usr/bin:/bin")), (true, AgentLink::ReverseForward, None)]
    {
        launch_through_the_provider_path(reverse_forwards, link, declared_path).await;
    }
}

async fn launch_through_the_provider_path(reverse_forwards: bool, link: AgentLink, declared_path: Option<&str>) {
    let mut stand_in = stand_in(reverse_forwards, link, declared_path).await;
    let env_id = stand_in.env_id.clone();
    let root = stand_in.root().to_path_buf();
    let provisioned = DockerControllerRuntime { state: Arc::clone(&stand_in.state) }
        .provision_environment(env_id.as_str(), &stand_in.spec)
        .await
        .expect("provision");
    // Recorded as the Environment reconciler records it.
    let environments = stand_in.backend.using::<Environment>(NAMESPACE);
    let record = environments.get(env_id.as_str()).await.expect("environment record");
    let status = flotilla_resources::EnvironmentStatus {
        provider_descriptor: provisioned.provider_descriptor.map(|descriptor| *descriptor),
        ..Default::default()
    };
    environments.update_status(env_id.as_str(), &record.metadata.resource_version, &status).await.expect("environment status");
    let handle = stand_in.handle().await.expect("provisioned handle");

    // The vessel layer: the generation's CLI is on the vessel's own PATH beside
    // the provider's environment, and the socket the vessel's environment names
    // reaches this environment's socket.
    let commands = handle.agent().commands();
    let shell = |script: String| {
        let commands = Arc::clone(&commands);
        async move { commands.run("sh", &["-c", &script], Path::new("/"), &ChannelLabel::Default).await.expect("vessel command") }
    };
    let tools = root.join("vessel/opt/flotilla/bin");
    let own_path = declared_path.map(str::to_string).unwrap_or_else(|| std::env::var("PATH").expect("host PATH"));
    assert_eq!(shell("printf %s \"$PATH\"".into()).await, format!("{}:{own_path}", tools.display()));
    assert_eq!(shell("printf %s \"$LANG\"".into()).await, "C.UTF-8", "the provider's environment is kept");
    assert_eq!(shell("command -v flotilla".into()).await.trim(), tools.join("flotilla").display().to_string());
    let guest_socket = PathBuf::from(shell(format!("printf %s \"${DAEMON_SOCKET_ENV}\"")).await);
    let mut guest = tokio::net::UnixStream::connect(&guest_socket).await.expect("guest crew socket");
    guest.write_all(b"crew").await.expect("guest write");
    let (environment, reach, mut controller) =
        tokio::time::timeout(Duration::from_secs(5), stand_in.crew_connections.recv()).await.expect("accept").expect("crew connection");
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
                convoy: stand_in.convoy.clone(),
                vessel_ref: "demo-work".to_string(),
            }),
            message: None,
        },
        cwd: root.join("skill-source").display().to_string(),
        env: BTreeMap::new(),
        pool: "fake-terminals".to_string(),
    };
    TerminalControllerRuntime { state: Arc::clone(&stand_in.state) }
        .ensure_session("terminal-demo-work-coder", (&session).into(), &[])
        .await
        .expect("launch");

    let home = Path::new(flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE)
        .join("agent-homes/requesters")
        .join(&stand_in.convoy)
        .join("coder/claude");
    let launched = stand_in
        .pool
        .ensured
        .lock()
        .await
        .iter()
        .find_map(|launch| launch.env_vars.iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, home)| PathBuf::from(home)));
    assert_eq!(launched.as_ref(), Some(&home), "launch uses the home the access layer delivered");
    // #3663: the home's skills link resolves into the agent's content cache.
    assert!(fs::symlink_metadata(home.join("skills")).expect("skills link").file_type().is_symlink());
    assert!(fs::read_to_string(home.join("skills/research/SKILL.md")).expect("pushed skill").contains("name: research"));
    let deliveries = flotilla_projections::read_projections::access_delivery_evidence(&stand_in.backend, NAMESPACE, &stand_in.convoy)
        .await
        .expect("deliveries");
    let [delivery] = deliveries.as_slice() else { panic!("one access delivered: {deliveries:?}") };
    assert_eq!(Path::new(&delivery.home), home);
    let mechanisms = delivery.needs.iter().map(|need| (need.need.as_str(), need.mechanism.as_str())).collect::<BTreeMap<_, _>>();
    assert_eq!(mechanisms.get("home"), Some(&"agent-push"));
    assert_eq!(mechanisms.get("skills"), Some(&"agent-push"));
    let connection = delivery.agent_connection.as_ref().expect("explain shows drydock's report");
    assert_eq!((connection.provider.as_str(), connection.mode.as_str()), ("stand-in", "inward_exposure"));
    assert_eq!(connection.vessel, handle.backing_id());
    let dialled = stand_in.gateway.links();
    assert!(dialled.iter().all(|dialled| *dialled == link), "never a link the gateway refuses: {dialled:?}");
    handle.destroy().await.expect("destroy");
    let _ = fs::remove_dir_all(home.parent().and_then(Path::parent).expect("convoy homes"));
    stand_in.clean_up();
}

// #3981: a vessel layer that fails discards a vessel this provision created,
// with its crew socket, and retains a vessel it adopted, with its socket, for
// the retry.
#[tokio::test]
async fn a_failed_vessel_layer_discards_only_a_vessel_it_created() {
    let stand_in = stand_in(false, AgentLink::LocalForward, None).await;
    let runtime = DockerControllerRuntime { state: Arc::clone(&stand_in.state) };
    let name = stand_in.env_id.as_str();
    let provider = stand_in.state.provider_for_environment(name).await.expect("provider");
    let socket = stand_in.root().join("sockets").join(directory_name(&stand_in.env_id));

    stand_in.refuse_tools.store(true, Ordering::SeqCst);
    let created = runtime.provision_environment(name, &stand_in.spec).await.expect_err("tools refused");
    assert!(matches!(created, EnvironmentProvisionError::Failed(_)), "{created:?}");
    assert!(provider.list().await.expect("vessels").is_empty(), "the vessel it created is destroyed");
    assert!(!socket.exists(), "with its crew socket");

    stand_in.refuse_tools.store(false, Ordering::SeqCst);
    runtime.provision_environment(name, &stand_in.spec).await.expect("provision");
    stand_in.refuse_tools.store(true, Ordering::SeqCst);
    let adopted = runtime.provision_environment(name, &stand_in.spec).await.expect_err("tools refused");
    assert!(matches!(adopted, EnvironmentProvisionError::RetryAdoption(_)), "{adopted:?}");
    assert_eq!(provider.list().await.expect("vessels").len(), 1, "the vessel it adopted is retained");
    assert!(socket.exists(), "with its crew socket");

    stand_in.handle().await.expect("provisioned handle").destroy().await.expect("destroy");
    stand_in.clean_up();
}

/// The drydock stand-in, the provider instance flotilla composes for it, and
/// a daemon holding a cloud-sandbox Environment for it with one crew frozen.
struct StandIn {
    temp: tempfile::TempDir,
    convoy: String,
    env_id: EnvironmentId,
    spec: EnvironmentSpec,
    state: Arc<ControllerRuntimeState>,
    backend: ResourceBackend,
    pool: Arc<FakeTerminalPool>,
    gateway: Arc<Gateway>,
    host: tokio::task::JoinHandle<()>,
    crew_connections: tokio::sync::mpsc::UnboundedReceiver<(EnvironmentId, SocketReach, tokio::net::UnixStream)>,
    refuse_tools: Arc<AtomicBool>,
}

impl StandIn {
    fn root(&self) -> &Path {
        self.temp.path()
    }

    async fn handle(&self) -> Option<EnvironmentHandle> {
        let provisioned = self.state.provisioned_environments.lock().await;
        provisioned.values().find(|active| active.handle.id() == &self.env_id).map(|active| active.handle.clone())
    }

    /// The stand-in vessel is this host: remove what this environment left at
    /// its contained base, and nothing another test shares there.
    fn clean_up(&self) {
        self.host.abort();
        let base = Path::new(flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE);
        let _ = fs::remove_dir_all(base.join("agent-material/requesters").join(&self.convoy));
        let digest = format!("{:x}", sha2::Sha256::digest(self.env_id.as_str().as_bytes()));
        let _ = fs::remove_dir_all(base.join("crew-sockets").join(&digest[..16]));
    }
}

/// Drydock's agent build seam, given the provider's own build.
struct Build;
#[async_trait]
impl AgentBuilds for Build {
    async fn build(&self, _: &drydock_protocol::Platform) -> Result<Vec<u8>, String> {
        Ok(BUILD.to_vec())
    }
}

/// Prepare already names digests.
struct Digests;
#[async_trait]
impl ::drydock::launch::ImageResolver for Digests {
    async fn resolve(&self, image: &str) -> Result<String, ::drydock::client::Error> {
        Ok(image.into())
    }
}

async fn stand_in(reverse_forwards: bool, link: AgentLink, declared_path: Option<&str>) -> StandIn {
    let temp = tempfile::Builder::new().prefix("dl-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).expect("private root");
    }
    let convoy = format!("drydock-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let env_id = EnvironmentId::new(format!("env-{convoy}"));

    let socket = root.join("host.sock");
    let serving = socket.clone();
    let host = tokio::spawn(async move {
        drydock_host::Host::new(drydock_host::reference::registry()).serve_unix(serving, std::future::pending()).await.expect("host");
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while ::drydock::api::ProviderApi::hello(&::drydock::client::Client::unix(&socket).expect("client")).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reference host");
    fs::write(root.join("credential"), "stand-in-key").expect("credential");
    fs::write(root.join("hosts"), "127.0.0.1 localhost\n").expect("hosts file");
    let text = format!(
        "[providers.stand-in]\nmodule='reference'\nagent_link='{}'\ncredential={{file='{}'}}\n\
         settings={{agent=true, poll_interval_ms=10, workspace_root='{}', reach={{kind='ssh', endpoint='gateway.test:22', \
         user='minted', host_key='ssh-ed25519 AAAA', reverse_forwards={reverse_forwards}}}}}\n",
        link.as_str(),
        root.join("credential").display(),
        root.join("workspace").display()
    );
    fs::create_dir(root.join("vessel")).expect("vessel directory");
    // The provider prepares linux/amd64 guests, so the stand-in vessel reports
    // that platform on any test host.
    let gateway = Gateway::start_as(&root.join("vessel"), reverse_forwards, BUILD, flotilla_environment::guest_tools::linux_amd64())
        .expect("gateway");
    let seams = Seams {
        dialer: gateway.clone(),
        builds: Arc::new(Build),
        guest: ::drydock::exposures::GuestLayout { hosts: root.join("hosts"), sockets: root.join("guest") },
        ..Seams::new(Arc::new(Digests))
    };
    let option = HostOption { optional: false, name: "rust".into(), connection: ::drydock::HostConnection::ExternalUnix { path: socket } };
    let run = root.join("run");
    let drydock =
        Daemon::start_with_seams(::drydock::config::Config::parse(&text).expect("config"), &run, run.join("keys"), vec![option], seams)
            .await
            .expect("drydock");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            ::drydock::api::ProviderApi::provider(&*drydock, "stand-in").await.map(|p| p.readiness),
            Ok(drydock_protocol::Readiness::Ready)
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stand-in ready");
    let provider = DrydockEnvironmentProvider::new(drydock, "stand-in".into(), "reference".into())
        .with_agent_build(BUILD.to_vec())
        .with_guest_agent_socket(root.join("vessel/agent.sock"));

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
    let mut env = BTreeMap::from([("LANG".to_string(), "C.UTF-8".to_string())]);
    env.extend(declared_path.map(|path| ("PATH".to_string(), path.to_string())));
    let spec = EnvironmentSpec {
        provider_address: Some(address.clone()),
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
            probe_needs: Default::default(),
            image: format!("example.test/crew@sha256:{}", "a".repeat(64)),
            size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1024, disk_bytes: 1024 },
            lifetime_cap_seconds: None,
            env,
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
    let (accepted, crew_connections) = tokio::sync::mpsc::unbounded_channel();
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
    let refuse_tools = Arc::new(AtomicBool::new(false));
    let tools = GenerationCli { root: root.to_path_buf(), refused: Arc::clone(&refuse_tools) };
    state.environment_tools = EnvironmentToolProvisioner::new(vec![Arc::new(tools)]);
    let state = Arc::new(state);
    StandIn { temp, convoy, env_id, spec, state, backend, pool, gateway, host, crew_connections, refuse_tools }
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
/// with the daemon socket it reaches, as the CLI tool declares it; or refused.
struct GenerationCli {
    root: PathBuf,
    refused: Arc<AtomicBool>,
}

#[async_trait]
impl EnvironmentToolFactory for GenerationCli {
    fn provider_kinds(&self) -> &[EnvironmentToolProviderKind] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, _: &str, _: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        use std::os::unix::fs::PermissionsExt;
        if self.refused.load(Ordering::SeqCst) {
            return Err("the generation's tools are refused".into());
        }
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

// Review of #4119: drydock's sockets, the longest an exposure's local end
// (`exposures/<16>/<22-byte name>.sock`), fit macOS's 104-byte `sun_path`
// whatever the state directory, and each state directory gets its own.
#[hegel::test]
fn drydock_sockets_fit_sun_path_for_any_state_directory(tc: hegel::TestCase) {
    let depth = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(40));
    let state = PathBuf::from("/Users").join("a-long-home-directory-name".repeat(depth)).join(".local/state/flotilla");
    let run = super::super::drydock_run_directory(&state);
    let longest = run.join("exposures").join("0123456789abcdef").join(format!("cleat-{}.sock", "f".repeat(16)));
    assert!(longest.as_os_str().len() < 104, "{}", longest.display());
    assert_ne!(run, super::super::drydock_run_directory(&state.join("other")));
}
