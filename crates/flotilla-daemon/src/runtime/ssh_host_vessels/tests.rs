//! The vessel manager against real drydock agents. The "host" is this
//! machine: each vessel's agent is drydock's Agent serving the vessel's real
//! socket in this process, reached directly rather than over SSH (the SSH
//! transport has its own contract tests in flotilla-environment), and host
//! commands run through the local process runner.

use std::sync::atomic::{AtomicUsize, Ordering};

use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_environment::environment::agent::{connect_current, AgentConnector};
use flotilla_provider_runtime::{
    host_environment::{HostEnvironment, HostSupply},
    ProcessCommandRunner,
};
use flotilla_resources::{FulfilmentFacts, Selector, TerminalBrief, TerminalCrewContext, TerminalSessionSource, TerminalSessionSpec};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, RwLock},
};

use super::*;
use crate::runtime::discovery::LocalProvisioningProfile;
use crate::server::environment_sockets::EnvironmentSocketRegistry;

const ENVIRONMENT: &str = "host-direct-b1";

/// Drydock's Agent on a vessel's socket, one per agent directory, as the
/// supervisor keeps one per vessel on the host.
#[derive(Default)]
struct LocalAgents {
    agents: StdMutex<HashMap<PathBuf, tokio::task::JoinHandle<()>>>,
    opened: AtomicUsize,
    /// Every handle opened, to check what teardown and failures close.
    handles: StdMutex<Vec<Arc<AgentRunner>>>,
    /// Agents start from an older build, which the controller's upgrades.
    upgrade: Option<Upgrade>,
    upgrades: Arc<AtomicUsize>,
}

/// The controller's own agent build, newer than the one its agents start
/// from, and the gate its bytes to the agents pass through.
struct Upgrade {
    build: Vec<u8>,
    gate: Arc<Gate>,
}

impl Drop for LocalAgents {
    fn drop(&mut self) {
        if let Ok(agents) = self.agents.get_mut() {
            for agent in agents.values() {
                agent.abort();
            }
        }
    }
}

const BUILD: &[u8] = b"local drydock agent";

impl LocalAgents {
    /// Agents that the controller upgrades to `build` on first connect.
    fn upgrading(build: Vec<u8>, gate: Arc<Gate>) -> Self {
        let mut agents = Self::default();
        agents.upgrade = Some(Upgrade { build, gate });
        agents
    }

    fn serve(&self, directory: &Path) {
        let mut agents = self.agents.lock().expect("agents");
        let socket = directory.join("agent.sock");
        // A removed directory took the agent with it: start it again, as the
        // next connect bootstraps one on the host.
        if socket.exists() && agents.contains_key(directory) {
            return;
        }
        if let Some(stopped) = agents.remove(directory) {
            stopped.abort();
        }
        std::fs::create_dir_all(directory).expect("agent directory");
        let mut listener = Some(UnixListener::bind(&socket).expect("agent socket"));
        let installed = directory.join("drydock-agent");
        std::fs::write(&installed, BUILD).expect("agent executable");
        let mut fingerprint = format!("{:x}", Sha256::digest(BUILD));
        if self.upgrade.is_some() {
            fingerprint = "older-build".into();
        }
        let upgrades = Arc::clone(&self.upgrades);
        agents.insert(
            directory.into(),
            tokio::spawn(async move {
                // An activated upgrade re-executes the agent on the same
                // socket, as its supervisor restarts it.
                loop {
                    let listener = match listener.take() {
                        Some(listener) => listener,
                        None => {
                            let _ = std::fs::remove_file(&socket);
                            UnixListener::bind(&socket).expect("agent socket")
                        }
                    };
                    let Ok(upgrade) = drydock_agent::Agent::new(listener, fingerprint.clone(), installed.clone()).run().await else {
                        return;
                    };
                    fingerprint = format!("{:x}", Sha256::digest(std::fs::read(&upgrade).expect("upgrade image")));
                    std::fs::rename(upgrade, &installed).expect("install upgrade");
                    upgrades.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
    }
}

/// Holds the controller's bytes to its agents once `limit` have passed, as a
/// slow link to a hosted box holds an upload partway.
struct Gate {
    limit: std::sync::atomic::AtomicU64,
    written: std::sync::atomic::AtomicU64,
    waiting: StdMutex<Vec<std::task::Waker>>,
    held: tokio::sync::Notify,
}

impl Gate {
    fn closing_after(limit: u64) -> Arc<Self> {
        Arc::new(Self { limit: limit.into(), written: 0.into(), waiting: StdMutex::default(), held: tokio::sync::Notify::new() })
    }

    fn open(&self) {
        self.limit.store(u64::MAX, Ordering::SeqCst);
        for waker in self.waiting.lock().expect("waiting").drain(..) {
            waker.wake();
        }
    }

    fn room(&self) -> u64 {
        self.limit.load(Ordering::SeqCst).saturating_sub(self.written.load(Ordering::SeqCst))
    }
}

struct Gated {
    stream: UnixStream,
    gate: Arc<Gate>,
}

impl tokio::io::AsyncRead for Gated {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Gated {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.gate.room() == 0 {
            self.gate.waiting.lock().expect("waiting").push(cx.waker().clone());
            self.gate.held.notify_one();
            // Opened while the waker was registered: write now.
            if self.gate.room() == 0 {
                return std::task::Poll::Pending;
            }
        }
        let room = usize::try_from(self.gate.room()).unwrap_or(usize::MAX).min(buf.len());
        let written = std::pin::Pin::new(&mut self.stream).poll_write(cx, &buf[..room]);
        if let std::task::Poll::Ready(Ok(written)) = &written {
            self.gate.written.fetch_add(*written as u64, Ordering::SeqCst);
        }
        written
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct LocalConnector {
    socket: PathBuf,
    build: Vec<u8>,
    gate: Option<Arc<Gate>>,
}

#[async_trait]
impl AgentConnector for LocalConnector {
    async fn connect(&self) -> Result<drydock_agent::Client, String> {
        connect_current(
            || async {
                let stream = UnixStream::connect(&self.socket).await?;
                let stream: drydock_transport::ByteStream = match &self.gate {
                    Some(gate) => Box::new(Gated { stream, gate: Arc::clone(gate) }),
                    None => Box::new(stream),
                };
                Ok(stream)
            },
            &drydock_protocol::Platform::current(),
            &self.build,
            Duration::from_secs(5),
        )
        .await
        .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl VesselAgents for LocalAgents {
    async fn host_refusals(&self, _profile: &SshHostProfile) -> Vec<HostRefusal> {
        Vec::new()
    }
    async fn open(
        &self,
        profile: &SshHostProfile,
        _: &VesselKey,
        _: &HostPlatform,
        paths: &VesselPaths,
        local: &Path,
    ) -> Result<Arc<dyn VesselHandle>, String> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.serve(&paths.agent);
        let connector = LocalConnector {
            socket: paths.agent.join("agent.sock"),
            build: self.upgrade.as_ref().map_or_else(|| BUILD.to_vec(), |upgrade| upgrade.build.clone()),
            gate: self.upgrade.as_ref().map(|upgrade| Arc::clone(&upgrade.gate)),
        };
        let agent = Arc::new(AgentRunner::new(Arc::new(connector), local.into()).with_writable_bases(WritableBases::Host));
        self.handles.lock().expect("handles").push(Arc::clone(&agent));
        Ok(Arc::new(AgentVessel { agent, host: Arc::clone(&profile.runner) }))
    }
    async fn probe(
        &self,
        _discovery: &DiscoveryRuntime,
        _config: &ConfigStore,
        _profile: &SshHostProfile,
        _handle: &dyn VesselHandle,
        _environment: &BTreeMap<String, String>,
    ) -> Result<Arc<ProviderRegistry>, String> {
        Ok(Arc::new(ProviderRegistry::new()))
    }
}

/// Bytes each vessel socket received, keyed by the socket that accepted them.
type Received = mpsc::UnboundedReceiver<(PathBuf, EnvironmentId, Vec<u8>)>;

/// The daemon side: every vessel socket records what its crews send.
fn recording_sockets(state: &Path) -> (EnvironmentSocketService, Received) {
    let (sender, received) = mpsc::unbounded_channel();
    let service = EnvironmentSocketService::new(
        Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        flotilla_paths::path_context::DaemonHostPath::new(state.join("env")),
        move |listener, environment, _| {
            let sender = sender.clone();
            tokio::spawn(async move {
                let path = listener.local_addr().ok().and_then(|address| address.as_pathname().map(Path::to_path_buf)).unwrap_or_default();
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut bytes = [0; 1];
                    if stream.read_exact(&mut bytes).await.is_ok() {
                        let _ = sender.send((path.clone(), environment.clone(), bytes.to_vec()));
                        let _ = stream.write_all(b"ok").await;
                    }
                }
            })
        },
    );
    (service, received)
}

struct Host {
    _runtime: flotilla_test_support::TestSocketDir,
    home: flotilla_test_support::TestSocketDir,
    state: flotilla_test_support::TestSocketDir,
    profile: SshHostProfile,
}

fn host(runner: Arc<dyn CommandRunner>) -> Host {
    let runtime = flotilla_test_support::TestSocketDir::new();
    // The home holds each vessel's cleat runtime and the controller state its
    // daemon sockets: both are socket directories (SUN_LEN).
    let home = flotilla_test_support::TestSocketDir::new();
    let state = flotilla_test_support::TestSocketDir::new();
    let profile = SshHostProfile {
        provisioning: LocalProvisioningProfile {
            environment_providers: Vec::new(),
            host_id: "b1".into(),
            display_name: "b1".into(),
            repo_default_dir: home.path().join("repos").display().to_string(),
            host_direct_pool: "cleat".into(),
            host_direct_provider: "host-direct".into(),
            docker_pool: "cleat".into(),
            available_pools: vec!["cleat".into()],
            available_agent_adapters: Default::default(),
            docker_available: false,
            max_vessels: None,
        },
        environment_id: EnvironmentId::new(ENVIRONMENT),
        destination: "crew@b1".into(),
        env_bag: EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("HOME", home.path().display().to_string()))
            .with(EnvironmentAssertion::env_var("USER", "crew"))
            .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", runtime.path().display().to_string())),
        runner,
        host_environment: Default::default(),
        accounts: flotilla_discovery::FactoryRegistry::default_all().account_variables(),
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::<String, FulfilmentFacts>::new())),
        vessel_agent: Err("vessel agents are reached directly in this test".into()),
        single_tenant: false,
    };
    Host { _runtime: runtime, home, state, profile }
}

/// What the controller lends a preparation, owned here.
struct Preparer {
    tools: EnvironmentToolProvisioner,
    discovery: DiscoveryRuntime,
    config: ConfigStore,
}

impl VesselPreparer for Preparer {
    fn preparation(&self) -> VesselPreparation<'_> {
        VesselPreparation { tools: &self.tools, discovery: &self.discovery, config: &self.config }
    }
}

/// An attach request preparing `vessel` in its own task, which the test can
/// abort as a request past its deadline is.
fn attaching(vessels: &Arc<SshHostVessels>, host: &Host, vessel: &str) -> tokio::task::JoinHandle<Result<Arc<VesselAccess>, String>> {
    let (vessels, profile, vessel) = (Arc::clone(vessels), host.profile.clone(), vessel.to_string());
    let preparer = Preparer {
        tools: EnvironmentToolProvisioner::new(Vec::new()),
        discovery: flotilla_discovery_testkit::fake_discovery(false),
        config: ConfigStore::with_base(host.state.path().join("config")),
    };
    tokio::spawn(async move { vessels.access(Arc::new(preparer), &profile, &vessel).await })
}

async fn access(vessels: &Arc<SshHostVessels>, host: &Host, vessel: &str) -> Result<Arc<VesselAccess>, String> {
    access_with(vessels, host, vessel, EnvironmentToolProvisioner::new(Vec::new())).await
}

async fn access_with(
    vessels: &Arc<SshHostVessels>,
    host: &Host,
    vessel: &str,
    tools: EnvironmentToolProvisioner,
) -> Result<Arc<VesselAccess>, String> {
    let preparer = Preparer {
        tools,
        discovery: flotilla_discovery_testkit::fake_discovery(false),
        config: ConfigStore::with_base(host.state.path().join("config")),
    };
    vessels.access(Arc::new(preparer), &host.profile, vessel).await
}

async fn shell(access: &VesselAccess, script: &str) -> String {
    access.runner().run("sh", &["-c", script], Path::new("/"), &ChannelLabel::Default).await.expect("vessel command")
}

/// A crew's request through its vessel's daemon forward, and the reply. A
/// forward that nothing serves refuses, breaks or closes: no reply.
async fn crew_request(access: &VesselAccess, byte: u8) -> Vec<u8> {
    crew_request_within(access, byte, Duration::from_secs(5)).await.expect("reply")
}

/// As [`crew_request`], but a connection held open past the deadline is an
/// `Err`: a vessel agent that outlives its controller keeps an unclaimed
/// connection until its claim deadline.
async fn crew_request_within(access: &VesselAccess, byte: u8, wait: Duration) -> Result<Vec<u8>, tokio::time::error::Elapsed> {
    let Ok(mut crew) = UnixStream::connect(access.paths.daemon_socket()).await else { return Ok(Vec::new()) };
    if crew.write_all(&[byte]).await.is_err() {
        return Ok(Vec::new());
    }
    let mut reply = Vec::new();
    tokio::time::timeout(wait, crew.read_to_end(&mut reply)).await.map(|_| reply)
}

// #3792 acceptance: two vessels on one host each get their own agent and
// socket. One vessel's environment change and its daemon forward are
// invisible to the other, and each forward reaches its own vessel socket
// serving the host's Environment. Mutation check: deriving the vessel
// directory from the environment alone puts both vessels in one agent and
// fails this scenario.
#[tokio::test(flavor = "multi_thread")]
async fn two_vessels_on_one_host_each_get_their_own_agent_and_socket() {
    let host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let (sockets, mut received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));
    let a = access(&vessels, &host, "convoy-1-work").await.expect("vessel a");
    let b = access(&vessels, &host, "convoy-2-work").await.expect("vessel b");
    assert_eq!(agents.agents.lock().unwrap().len(), 2, "one agent per vessel");
    assert_ne!(a.paths.agent, b.paths.agent);
    assert_ne!(a.paths.daemon_socket(), b.paths.daemon_socket());
    for vessel in [&a, &b] {
        use std::os::unix::fs::PermissionsExt;
        assert!(vessel.paths.agent.starts_with(host._runtime.path().join("flotilla/v")));
        assert_eq!(std::fs::metadata(&vessel.paths.agent).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(shell(vessel, "printf %s \"$FLOTILLA_DAEMON_SOCKET\"").await, vessel.paths.daemon_socket().display().to_string());
        assert_eq!(shell(vessel, "printf %s \"$FLOTILLA_ENVIRONMENT_ID\"").await, ENVIRONMENT);
        assert!(shell(vessel, "printf %s \"$PATH\"").await.starts_with(&vessel.paths.generation_path().join(":")));
        assert!(vessel.paths.cleat_runtime().is_dir());
    }

    // One vessel's environment change is invisible to the other.
    let mut changed = vessel_environment(&host.profile, &a.paths).expect("environment");
    changed.insert("ONLY_IN_A".into(), "a".into());
    a.handle.configure_environment(changed).await.expect("reconfigure a");
    assert_eq!(shell(&a, "printf %s \"${ONLY_IN_A-unset}\"").await, "a");
    assert_eq!(shell(&b, "printf %s \"${ONLY_IN_A-unset}\"").await, "unset");

    // Each crew's request reaches its own vessel's socket, which serves the
    // host's Environment.
    assert_eq!(crew_request(&a, b'a').await, b"ok");
    assert_eq!(crew_request(&b, b'b').await, b"ok");
    let mut by_socket = BTreeMap::new();
    for _ in 0..2 {
        let (socket, environment, bytes) = tokio::time::timeout(Duration::from_secs(5), received.recv()).await.unwrap().unwrap();
        assert_eq!(environment.as_str(), ENVIRONMENT);
        by_socket.entry(socket).or_insert_with(Vec::new).extend(bytes);
    }
    assert_eq!(by_socket.len(), 2, "two vessel sockets: {by_socket:?}");
    assert!(by_socket.values().any(|bytes| bytes == b"a") && by_socket.values().any(|bytes| bytes == b"b"));

    // Access again uses the prepared vessel: no second agent is opened.
    access(&vessels, &host, "convoy-1-work").await.expect("vessel a again");
    assert_eq!(agents.opened.load(Ordering::SeqCst), 2);
}

// #4054: an attach request past its deadline stops waiting while the vessel's
// agent is partway through its upgrade upload. The preparation is not the
// caller's to cut off: the upgrade finishes, and the next caller finds the
// vessel prepared, with its agent on the new build. Mutation check: awaiting
// the preparation in the caller's own future drops the upload partway, and
// the next preparation is refused by the agent's partial upload.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_stops_waiting_does_not_cut_off_its_vessels_agent_upgrade() {
    let host = host(Arc::new(ProcessCommandRunner));
    let build = (0..4 * 1024 * 1024).map(|byte: u32| byte as u8).collect::<Vec<_>>();
    let gate = Gate::closing_after(1024 * 1024);
    let agents = Arc::new(LocalAgents::upgrading(build.clone(), Arc::clone(&gate)));
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));

    let attach = attaching(&vessels, &host, "convoy-1-work");
    tokio::time::timeout(Duration::from_secs(10), gate.held.notified()).await.expect("the upload reaches the gate");
    attach.abort();
    assert!(attach.await.is_err_and(|error| error.is_cancelled()));
    assert_eq!(agents.upgrades.load(Ordering::SeqCst), 0, "the upload is partway");

    gate.open();
    let vessel = tokio::time::timeout(Duration::from_secs(10), access(&vessels, &host, "convoy-1-work"))
        .await
        .expect("preparation finishes")
        .expect("the vessel is prepared");
    assert_eq!(agents.upgrades.load(Ordering::SeqCst), 1, "the upgrade ran to completion");
    assert_eq!(agents.opened.load(Ordering::SeqCst), 1, "the next caller found the first preparation");
    assert_eq!(std::fs::read(vessel.paths.agent.join("drydock-agent")).unwrap(), build);
    assert_eq!(shell(&vessel, "printf ready").await, "ready");
}

// #4054 review: retiring a vessel while a preparation no caller waits for is
// partway through its upgrade waits for the preparation, then closes what it
// prepared. Nothing outlives the retire. Mutation check: a retire that doesn't
// wait for the slot lock tears the vessel down under its unfinished upgrade.
#[tokio::test(flavor = "multi_thread")]
async fn retiring_a_vessel_mid_preparation_closes_what_the_preparation_leaves() {
    let host = host(Arc::new(ProcessCommandRunner));
    let build = (0..4 * 1024 * 1024).map(|byte: u32| byte as u8).collect::<Vec<_>>();
    let gate = Gate::closing_after(1024 * 1024);
    let agents = Arc::new(LocalAgents::upgrading(build, Arc::clone(&gate)));
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));

    let attach = attaching(&vessels, &host, "convoy-1-work");
    tokio::time::timeout(Duration::from_secs(10), gate.held.notified()).await.expect("the upload reaches the gate");
    attach.abort();
    assert!(attach.await.is_err_and(|error| error.is_cancelled()));

    let key = VesselKey::new(&host.profile.environment_id, "convoy-1-work");
    let retire = tokio::spawn({
        let (vessels, profile) = (Arc::clone(&vessels), host.profile.clone());
        async move { vessels.retire(&profile, "convoy-1-work").await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !vessels.retired.lock().await.contains(&key) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retire takes the vessel's slot");
    assert!(!retire.is_finished(), "the retire waits for the preparation");

    gate.open();
    tokio::time::timeout(Duration::from_secs(10), retire).await.expect("the retire finishes").expect("retire");
    assert_eq!(agents.upgrades.load(Ordering::SeqCst), 1, "the preparation finished before the retire");
    let handles = agents.handles.lock().unwrap().clone();
    assert_eq!(handles.len(), 1);
    assert!(handles[0].is_closed(), "the retire closed what the preparation stored");
    let error = access(&vessels, &host, "convoy-1-work").await.err().expect("not resurrected");
    assert!(error.contains("torn down"), "{error}");
}

// #3792 contract 6: removing a vessel removes its forward, stops its agent
// and removes its directories, and a late observation cannot resurrect it.
// The other vessel keeps working. A vessel live in the store again is
// admitted.
#[tokio::test(flavor = "multi_thread")]
async fn retiring_a_vessel_removes_it_without_touching_its_neighbour() {
    let host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));
    let gone = access(&vessels, &host, "convoy-1-work").await.expect("vessel");
    let kept = access(&vessels, &host, "convoy-2-work").await.expect("neighbour");

    vessels.retire_absent(&host.profile, &BTreeSet::from(["convoy-2-work".to_string()])).await.expect("retire absent");
    assert!(!gone.paths.agent.exists() && !gone.paths.state.exists(), "the vessel's directories are removed");
    assert!(crew_request(&gone, b'x').await.is_empty(), "nothing serves the removed vessel's forward");
    let error = access(&vessels, &host, "convoy-1-work").await.err().expect("not resurrected");
    assert!(error.contains("torn down"), "{error}");
    // A caller that kept the vessel's runner (a token delivery refreshing
    // through it) reaches the host instead, and never restarts the agent.
    let kept_runner = gone.runner();
    let marker = host.home.path().join("refreshed");
    kept_runner.write_file(&marker, "token").await.expect("write through the host runner");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "token");
    assert!(!gone.paths.agent.exists(), "the retired vessel's agent is not restarted");
    assert_eq!(agents.opened.load(Ordering::SeqCst), 2);
    assert_eq!(crew_request(&kept, b'k').await, b"ok");
    assert_eq!(shell(&kept, "printf kept").await, "kept");

    vessels.retire_absent(&host.profile, &BTreeSet::from(["convoy-1-work".to_string(), "convoy-2-work".to_string()])).await.expect("admit");
    access(&vessels, &host, "convoy-1-work").await.expect("a vessel live again is admitted");
}

// A controller that restarts never prepared the vessels removed while it was
// down. Their markers on the host name them, so the first pass retires them.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_controller_retires_vessels_removed_while_it_was_down() {
    let host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let paths = {
        let (sockets, _received) = recording_sockets(host.state.path());
        let before = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));
        access(&before, &host, "convoy-1-work").await.expect("vessel").paths.clone()
    };
    assert!(paths.agent.join("vessel.json").is_file() && paths.state.join("vessel.json").is_file());
    let (sockets, _received) = recording_sockets(host.state.path());
    let restarted = Arc::new(SshHostVessels::new(agents, sockets, host.state.path().join("vessels")));
    restarted.retire_absent(&host.profile, &BTreeSet::new()).await.expect("retire absent");
    assert!(!paths.agent.exists() && !paths.state.exists());
}

/// A host whose runner always fails, as an unreachable host's SSH does.
struct Unreachable;

#[async_trait]
impl CommandRunner for Unreachable {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        Err("ssh: connect to host b1 port 22: Connection refused".into())
    }
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|_| unreachable!())
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

// #3792 contract 6: a stopped or unreachable agent is logged and does not
// block teardown. The controller's side is released all the same.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_host_does_not_block_teardown() {
    let mut host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents, sockets, host.state.path().join("vessels")));
    let vessel = access(&vessels, &host, "convoy-1-work").await.expect("vessel");
    host.profile.runner = Arc::new(Unreachable);
    vessels.retire(&host.profile, "convoy-1-work").await;
    // The host stayed unreachable, so its agent lives on holding the connection unclaimed.
    // A served forward answers within milliseconds; one second of silence shows it is gone.
    assert!(
        crew_request_within(&vessel, b'x', Duration::from_secs(1)).await.map_or(true, |reply| reply.is_empty()),
        "the controller no longer serves the vessel's forward"
    );
    assert!(access(&vessels, &host, "convoy-1-work").await.is_err());
}

#[test]
fn vessel_paths_are_stable_private_per_vessel_and_bounded() {
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", "/home/crew"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", "/run/user/1000"));
    let environment = EnvironmentId::new(ENVIRONMENT);
    let paths = |environment: &EnvironmentId, vessel: &str| VesselPaths::new(&bag, &VesselKey::new(environment, vessel)).expect("paths");
    let a = paths(&environment, "convoy-1-work");
    assert_eq!(a, paths(&environment, "convoy-1-work"), "sessions find their vessel after a controller restart");
    assert!(a.agent.starts_with("/run/user/1000/flotilla/v"));
    assert!(a.state.starts_with("/home/crew/.local/state/flotilla/vessels"));
    assert_eq!(a.daemon_socket(), a.agent.join("flotilla.sock"));
    assert_ne!(a, paths(&environment, "convoy-2-work"));
    assert_ne!(a, paths(&EnvironmentId::new("host-direct-b2"), "convoy-1-work"));
    // Length framing: moving a boundary between the names moves the vessel.
    assert_ne!(paths(&EnvironmentId::new("ab"), "c"), paths(&EnvironmentId::new("a"), "bc"));

    let state = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", "/home/crew"))
        .with(EnvironmentAssertion::env_var("XDG_STATE_HOME", "/state"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", "/run/user/1000"));
    assert!(VesselPaths::new(&state, &VesselKey::new(&environment, "v")).unwrap().state.starts_with("/state/flotilla/vessels"));
    let no_runtime = EnvironmentBag::new().with(EnvironmentAssertion::env_var("HOME", "/home/crew"));
    assert!(VesselPaths::new(&no_runtime, &VesselKey::new(&environment, "v")).unwrap_err().contains("XDG_RUNTIME_DIR"));
    let long = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", "/home/crew"))
        .with(EnvironmentAssertion::env_var("XDG_RUNTIME_DIR", format!("/run/{}", "x".repeat(80))));
    assert!(VesselPaths::new(&long, &VesselKey::new(&environment, "v")).unwrap_err().contains("SUN_LEN"));
}

// #3722 under the agent: the vessel's pushed environment is its layer, the
// declared supplies and the system tail, with the supplies' variables, the
// login basics and the daemon forward. A supply cannot set what flotilla sets.
#[test]
fn vessel_environment_is_composed_from_the_layer_supplies_and_forward() {
    let mut host = host(Arc::new(ProcessCommandRunner));
    host.profile.host_environment = HostEnvironment::new(vec![HostSupply {
        name: "rust".into(),
        path: vec!["~/.cargo/bin".into()],
        env: vec![("CARGO_HOME".into(), "~/.cargo".into())],
    }])
    .expect("supplies");
    let paths = VesselPaths::new(&host.profile.env_bag, &VesselKey::new(&host.profile.environment_id, "v")).expect("paths");
    let environment = vessel_environment(&host.profile, &paths).expect("environment");
    let home = host.home.path().display().to_string();
    assert_eq!(environment["PATH"], format!("{}:{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin", paths.generation_path().join(":")));
    assert_eq!(environment["CARGO_HOME"], format!("{home}/.cargo"));
    assert_eq!(environment["HOME"], home);
    assert_eq!(environment["USER"], "crew");
    assert_eq!(environment[DAEMON_SOCKET_ENV], paths.daemon_socket().display().to_string());
    assert_eq!(environment[CONTAINED_DAEMON_REQUIRED_ENV], "1");
    assert_eq!(environment["FLOTILLA_ENVIRONMENT_ID"], ENVIRONMENT);
    assert_eq!(environment["CLEAT_RUNTIME_DIR"], paths.cleat_runtime().display().to_string());

    host.profile.host_environment = HostEnvironment::new(vec![HostSupply {
        name: "cleat".into(),
        path: Vec::new(),
        env: vec![("CLEAT_RUNTIME_DIR".into(), "/elsewhere".into())],
    }])
    .expect("supplies");
    assert_eq!(
        vessel_environment(&host.profile, &paths).expect_err("collision"),
        "host configuration error for host-direct-b1: supply `cleat` sets CLEAT_RUNTIME_DIR, which flotilla sets"
    );
}

fn crew_spec(vessel: &str) -> TerminalSessionSpec {
    TerminalSessionSpec::builder()
        .env_ref(ENVIRONMENT.to_string())
        .role("coder".to_string())
        .source(TerminalSessionSource::Agent {
            selector: Selector { capability: "code".into(), adapter: None, model: None },
            brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: String::new(), copies: Vec::new() },
            context: Box::new(TerminalCrewContext { namespace: "flotilla".into(), convoy: "convoy-9".into(), vessel_ref: vessel.into() }),
            message: None,
        })
        .cwd("/work".to_string())
        .pool("cleat".to_string())
        .build()
}

// A convoy homed on another daemon may place its vessel on this daemon's
// SSH host: its replicated session keeps the vessel's agent alive.
// Mutation check: listing local records only retires that agent.
#[tokio::test]
async fn a_replicated_tool_session_keeps_its_vessel_live() {
    use flotilla_protocol::NodeId;
    let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    let actuator = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("actuator"));
    let sessions = home.clone().using::<TerminalSession>("flotilla");
    sessions
        .create(
            &flotilla_resources::InputMeta::builder()
                .name("terminal-convoy-9-work-coder".to_string())
                .labels(BTreeMap::from([(flotilla_resources::VESSEL_REF_LABEL.to_string(), "convoy-9-work".to_string())]))
                .build(),
            &TerminalSessionSpec { source: TerminalSessionSource::Tool { command: "sh".into() }, ..crew_spec("convoy-9-work") },
        )
        .await
        .expect("home session");
    actuator
        .replica_writer::<TerminalSession>(NodeId::new("home"), "flotilla")
        .replace(&sessions.list().await.expect("home sessions"), chrono::Utc::now())
        .await
        .expect("replicate");
    let live = live_vessels(&actuator, "flotilla", ENVIRONMENT).await.expect("live vessels");
    assert_eq!(live, BTreeSet::from(["convoy-9-work".to_string()]));
    assert!(live_vessels(&actuator, "flotilla", "host-direct-b2").await.expect("live vessels").is_empty());
}

/// A generation layer whose sources are missing.
struct MissingLayer;

#[async_trait]
impl crate::environment_tools::EnvironmentToolFactory for MissingLayer {
    fn provider_kinds(&self) -> &[crate::environment_tools::EnvironmentToolProviderKind] {
        &[SSH_HOST_PROVIDER_KIND]
    }
    async fn prepare(
        &self,
        _environment_name: &str,
        _context: &dyn EnvironmentToolContext,
    ) -> Result<flotilla_environment::environment::EnvironmentTool, String> {
        Err("publish fleet tools: cleat release lacks xterm-ghostty terminfo".into())
    }
}

// Missing tools refuse before opening an agent or uploading any guest bytes;
// a later complete preparation can retry successfully.
#[tokio::test(flavor = "multi_thread")]
async fn missing_tools_refuse_before_opening_an_agent() {
    let host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents.clone(), sockets, host.state.path().join("vessels")));
    let error = access_with(&vessels, &host, "convoy-1-work", EnvironmentToolProvisioner::new(vec![Arc::new(MissingLayer)]))
        .await
        .err()
        .expect("missing layer");
    assert!(error.contains("xterm-ghostty"), "{error}");
    assert!(agents.handles.lock().unwrap().is_empty(), "tool preflight must precede agent upload");
    access(&vessels, &host, "convoy-1-work").await.expect("the retry prepares the vessel");
    let handles = agents.handles.lock().unwrap().clone();
    assert_eq!(handles.len(), 1);
    assert!(!handles[0].is_closed());
}

/// The host, except that its vessel markers cannot be listed.
struct MarkersUnlisted;

#[async_trait]
impl CommandRunner for MarkersUnlisted {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if args.contains(&"flotilla-vessel-markers") {
            return Err("ssh: connection reset".into());
        }
        ProcessCommandRunner.run(cmd, args, cwd, label).await
    }
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        ProcessCommandRunner.run_output(cmd, args, cwd, label).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
}

// Review #3808: a failed marker listing still retires the vessels this
// controller prepared; only the marker-discovered ones wait for the next pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_marker_listing_still_retires_known_vessels() {
    let mut host = host(Arc::new(ProcessCommandRunner));
    let agents = Arc::new(LocalAgents::default());
    let (sockets, _received) = recording_sockets(host.state.path());
    let vessels = Arc::new(SshHostVessels::new(agents, sockets, host.state.path().join("vessels")));
    let gone = access(&vessels, &host, "convoy-1-work").await.expect("vessel");
    host.profile.runner = Arc::new(MarkersUnlisted);
    let error = vessels.retire_absent(&host.profile, &BTreeSet::new()).await.expect_err("markers unlisted");
    assert!(error.contains("connection reset"), "{error}");
    assert!(!gone.paths.agent.exists() && !gone.paths.state.exists(), "the known vessel is retired");
    assert!(access(&vessels, &host, "convoy-1-work").await.is_err());
}
