use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex as StdMutex,
    },
};

use flotilla_provider_runtime::{CommandOutput, ProcessCommandRunner};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

use super::*;
use crate::agent::tests::{SocketAgent, Sshd};
use flotilla_environment::environment::{agent::AgentRunner, VesselAgent};

/// A host whose commands run for real in local temporary directories. Its
/// drydock agent is the one part played in process: the executable flotilla
/// copies there is a stub that records how `--supervise` was asked for, and
/// the host then serves the vessel socket with drydock's real Agent and the
/// store options it was given, unless one already serves it, as the
/// supervisor's lock would decide. Without a store the agent keeps drydock's
/// default beside the vessel's executable. A `--collect` of the copied
/// executable runs drydock's real collection.
struct FakeHost {
    local: ProcessCommandRunner,
    agents: StdMutex<BTreeMap<PathBuf, Arc<SocketAgent>>>,
    current: Vec<u8>,
    starts: AtomicUsize,
}

/// The stub copied as the agent executable: it records the request, one
/// argument per line, and the environment it was started with.
const STUB_AGENT: &[u8] = b"#!/bin/sh\nenv > \"$(dirname \"$0\")/started-environment\"\n[ \"$1\" = --supervise ] && printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/supervise-requested\"\n";

impl FakeHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            local: ProcessCommandRunner,
            agents: StdMutex::default(),
            current: STUB_AGENT.to_vec(),
            starts: AtomicUsize::new(0),
        })
    }

    fn supervise(&self, directory: &Path) {
        let request = directory.join("supervise-requested");
        let Ok(arguments) = std::fs::read_to_string(&request) else { return };
        std::fs::remove_file(&request).unwrap();
        let option = |name: &str| {
            let mut lines = arguments.lines();
            lines.find(|line| *line == name).and_then(|_| lines.next()).map(PathBuf::from)
        };
        let store = option("--store").unwrap_or_else(|| directory.join(format!("{AGENT_EXECUTABLE}.store")));
        let holder = option("--holder");
        let socket = directory.join(AGENT_SOCKET);
        let mut agents = self.agents.lock().unwrap();
        if agents.contains_key(&socket) {
            return;
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        let agent = SocketAgent::start_with(socket.clone(), &self.current, false, move |agent| {
            let agent = agent.with_store(store.clone());
            match &holder {
                Some(holder) => agent.with_holder(holder.clone()),
                None => agent,
            }
        });
        agents.insert(socket, agent);
    }

    /// The supervisor and its agent die, as on a host reboot.
    fn kill(&self, directory: &Path) {
        if let Some(agent) = self.agents.lock().unwrap().remove(&directory.join(AGENT_SOCKET)) {
            agent.kill();
        }
    }
}

#[async_trait]
impl CommandRunner for FakeHost {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if let ["--collect", "--store", store, "--grace", grace] = args {
            if std::fs::read(cmd).map_err(|error| format!("{cmd}: {error}"))? != STUB_AGENT {
                return Err(format!("{cmd}: not this generation's agent"));
            }
            let grace = Duration::from_secs(grace.parse().unwrap());
            let collected = drydock_agent::collect(PathBuf::from(store), grace).await.map_err(|error| error.to_string())?;
            return Ok(format!("removed {} entries and {} blobs from {store}\n", collected.entries, collected.blobs));
        }
        let output = self.local.run(cmd, args, cwd, label).await;
        // The stopped vessel's supervisor and agent die with it.
        if let ["-c", script, "flotilla-vessel-agent-retire", directory, ..] = args {
            if *script == STOP {
                self.kill(Path::new(directory));
            }
        }
        output
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let output = self.local.run_output(cmd, args, cwd, label).await;
        if let ["-c", script, "flotilla-vessel-agent", directory, ..] = args {
            if *script == BOOTSTRAP {
                self.supervise(Path::new(directory));
            }
        }
        output
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.local.exists(cmd, args).await
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        tokio::fs::copy(source, destination).await.map(|_| ()).map_err(|error| error.to_string())
    }
}

/// Dials the agent socket directly: the fake host is this machine.
struct LocalTransport;
#[async_trait]
impl AgentTransport for LocalTransport {
    async fn open(&self, socket: &Path) -> io::Result<ByteStream> {
        Ok(Box::new(UnixStream::connect(socket).await?))
    }
}

struct Fixture {
    host: Arc<FakeHost>,
    directory: PathBuf,
    runner: Arc<AgentRunner>,
    _root: tempfile::TempDir,
}

fn fixture(transport: Arc<dyn AgentTransport>) -> Fixture {
    let root = tempfile::Builder::new().prefix("fva-").tempdir_in("/tmp").unwrap();
    let directory = root.path().join("run/flotilla/v/vessel");
    let executable = root.path().join("drydock-agent-build");
    std::fs::write(&executable, STUB_AGENT).unwrap();
    let host = FakeHost::new();
    let base = BaseLayer::declared(
        [("PATH".to_string(), "/opt/supplied/bin:/usr/bin:/bin".to_string()), ("TOOL_HOME".to_string(), "/opt/supplied".to_string())],
        &Default::default(),
    )
    .unwrap();
    let connector = HostAgentConnector::new(
        host.clone(),
        transport,
        directory.clone(),
        Platform::current(),
        executable,
        Duration::from_secs(10),
        &base,
    );
    let runner = Arc::new(AgentRunner::new(Arc::new(connector), root.path().join("controller")));
    std::fs::create_dir(root.path().join("controller")).unwrap();
    Fixture { host, directory, runner, _root: root }
}

// #3792 contract 2: the first connect copies the agent build into a private
// vessel directory and starts it through the host runner. An agent killed
// between commands is bootstrapped again on the next connect, without a second
// copy, and the vessel's pushed environment is restored with it.
#[tokio::test]
async fn first_connect_installs_and_a_killed_agent_is_bootstrapped_again() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture(Arc::new(LocalTransport));
    let runner = &fixture.runner;
    runner.configure_environment(BTreeMap::from([("VESSEL".into(), "a".into())])).await.unwrap();
    let installed = fixture.directory.join(AGENT_EXECUTABLE);
    assert_eq!(std::fs::read(&installed).unwrap(), STUB_AGENT);
    assert_eq!(std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&fixture.directory).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fixture.host.starts.load(Ordering::SeqCst), 1);
    // The agent starts with the prepared base layer and the login identity,
    // nothing of the session that started it.
    let started = std::fs::read_to_string(fixture.directory.join("started-environment")).unwrap();
    let started = started.lines().filter_map(|line| line.split_once('=')).collect::<BTreeMap<_, _>>();
    assert_eq!(started.get("PATH"), Some(&"/opt/supplied/bin:/usr/bin:/bin"), "{started:?}");
    assert_eq!(started.get("TOOL_HOME"), Some(&"/opt/supplied"));
    assert!(!started.contains_key("CARGO_MANIFEST_DIR"), "{started:?}");
    let echo = || runner.run("sh", &["-c", "printf %s \"$VESSEL\""], Path::new("/"), &ChannelLabel::Default);
    assert_eq!(echo().await.unwrap(), "a");

    let copied = std::fs::metadata(&installed).unwrap().modified().unwrap();
    fixture.host.kill(&fixture.directory);
    VesselAgent::ready(runner).await.unwrap();
    assert_eq!(fixture.host.starts.load(Ordering::SeqCst), 2, "the next connect bootstraps a new agent");
    assert_eq!(std::fs::metadata(&installed).unwrap().modified().unwrap(), copied, "the executable is copied once");
    assert_eq!(echo().await.unwrap(), "a", "the vessel's environment is pushed again");

    // A supervised agent that answers is used as it is.
    VesselAgent::ready(runner).await.unwrap();
    assert_eq!(fixture.host.starts.load(Ordering::SeqCst), 2);
}

// After a reboot the runtime directory is gone with its executable: the
// next connect copies the build again.
#[tokio::test]
async fn a_reboot_removes_the_executable_and_the_next_connect_copies_it_again() {
    let fixture = fixture(Arc::new(LocalTransport));
    VesselAgent::ready(&fixture.runner).await.unwrap();
    fixture.host.kill(&fixture.directory);
    std::fs::remove_dir_all(&fixture.directory).unwrap();
    VesselAgent::ready(&fixture.runner).await.unwrap();
    assert_eq!(std::fs::read(fixture.directory.join(AGENT_EXECUTABLE)).unwrap(), STUB_AGENT);
    assert_eq!(fixture.host.starts.load(Ordering::SeqCst), 2);
}

/// Dials the agent socket directly, counting the bytes its connections write.
#[derive(Default)]
struct CountedTransport {
    written: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentTransport for CountedTransport {
    async fn open(&self, socket: &Path) -> io::Result<ByteStream> {
        Ok(Box::new(Counted { stream: UnixStream::connect(socket).await?, written: Arc::clone(&self.written) }))
    }
}
struct Counted {
    stream: UnixStream,
    written: Arc<AtomicUsize>,
}
impl tokio::io::AsyncRead for Counted {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for Counted {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<io::Result<usize>> {
        let written = std::pin::Pin::new(&mut self.stream).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(count)) = &written {
            self.written.fetch_add(*count, Ordering::SeqCst);
        }
        written
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// A base layer with the system tools the stub agent runs.
fn system_base() -> BaseLayer {
    BaseLayer::declared([("PATH".to_string(), "/usr/bin:/bin".to_string())], &Default::default()).unwrap()
}

/// Every regular file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).into_iter().flatten().flatten() {
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}

// #4087 acceptance: two vessels on one host, with the same harness pin, share
// the host user's durable store. The harness bytes cross once: the second
// vessel's push is answered from the store's index, and nothing of the
// artifact lands under the runtime directory. Retiring one vessel keeps the
// entry the other still links; retiring both lets the retirement's
// collection remove it, read-only as drydock keeps it, though the crews'
// homes, with their links, outlive their vessels. Mutation check: starting
// the agents without the shared `--store` sends the bytes twice and fails
// this scenario.
#[tokio::test]
async fn vessels_on_one_host_share_the_durable_store_and_retirement_collects_it() {
    let root = tempfile::Builder::new().prefix("fvs-").tempdir_in("/tmp").unwrap();
    let (runtime, state) = (root.path().join("run/flotilla/v"), root.path().join("state/flotilla"));
    let store = SharedStore { grace: Duration::ZERO, ..SharedStore::new(state.join("content-store")) };
    let executable = root.path().join("drydock-agent-build");
    std::fs::write(&executable, STUB_AGENT).unwrap();
    let harness = (0..1024 * 1024).map(|byte: u32| (byte % 251) as u8).collect::<Vec<_>>();
    let digest = format!("{:x}", Sha256::digest(&harness));
    let pinned = root.path().join("claude");
    std::fs::write(&pinned, &harness).unwrap();
    let host = FakeHost::new();
    let mut vessels = Vec::new();
    for name in ["a", "b"] {
        let (directory, holder) = (runtime.join(name), state.join("vessels").join(name));
        std::fs::create_dir_all(&holder).unwrap();
        let transport = Arc::new(CountedTransport::default());
        let written = Arc::clone(&transport.written);
        let connector = HostAgentConnector::new(
            host.clone(),
            transport,
            directory.clone(),
            Platform::current(),
            executable.clone(),
            Duration::from_secs(10),
            &system_base(),
        )
        .with_store(store.path.clone(), holder.clone());
        let controller = root.path().join(format!("controller-{name}"));
        std::fs::create_dir(&controller).unwrap();
        let runner = Arc::new(AgentRunner::new(Arc::new(connector), controller));
        VesselAgent::ready(&runner).await.unwrap();
        let before = written.load(Ordering::SeqCst);
        VesselAgent::push_artifact(&runner, &digest, &pinned, flotilla_resources::PinUnpack::File).await.unwrap();
        let link = state.join(format!("agent-homes/{name}/.flotilla/harness/bin/claude"));
        VesselAgent::link_artifact(&runner, &link, &digest, Path::new("")).await.unwrap();
        vessels.push((directory, holder, link, runner, written.load(Ordering::SeqCst) - before));
    }
    assert!(vessels[0].4 >= harness.len(), "the first vessel pushes the harness: {} bytes", vessels[0].4);
    assert!(vessels[1].4 < 64 * 1024, "the second vessel sends no harness bytes: {} bytes", vessels[1].4);
    assert_eq!(std::fs::read(&vessels[1].2).unwrap(), harness);
    let large =
        files_under(&runtime).into_iter().filter(|file| std::fs::metadata(file).unwrap().len() >= harness.len() as u64).collect::<Vec<_>>();
    assert!(large.is_empty(), "the runtime directory holds artifact files: {large:?}");

    let (directory, holder, ..) = &vessels[0];
    retire_host_agent(host.as_ref(), directory, std::slice::from_ref(holder), Some(&store)).await.unwrap();
    assert!(!directory.exists() && !holder.exists());
    assert_eq!(std::fs::read(&vessels[1].2).unwrap(), harness, "the entry another live vessel links is kept");

    let (directory, holder, link, ..) = &vessels[1];
    retire_host_agent(host.as_ref(), directory, std::slice::from_ref(holder), Some(&store)).await.unwrap();
    assert!(std::fs::symlink_metadata(link).is_ok(), "the crew's home outlives its vessel");
    assert_eq!(std::fs::read(link).map_err(|error| error.kind()), Err(io::ErrorKind::NotFound), "no live vessel uses the entry");
    assert_eq!(std::fs::read_dir(store.path.join("artifacts")).unwrap().count(), 0);
}

// An executable an earlier generation installed refuses this generation's
// store options; the bootstrap replaces it with this generation's build and
// starts that.
#[tokio::test]
async fn an_executable_refusing_the_store_options_is_replaced_and_started() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new().prefix("fvo-").tempdir_in("/tmp").unwrap();
    let directory = root.path().join("run/flotilla/v/vessel");
    std::fs::create_dir_all(&directory).unwrap();
    let earlier = b"#!/bin/sh\nfor argument; do [ \"$argument\" = --store ] && { echo 'usage: drydock-agent --supervise --socket PATH' >&2; exit 1; }; done\n";
    std::fs::write(directory.join(AGENT_EXECUTABLE), earlier).unwrap();
    std::fs::set_permissions(directory.join(AGENT_EXECUTABLE), std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = root.path().join("drydock-agent-build");
    std::fs::write(&executable, STUB_AGENT).unwrap();
    let holder = root.path().join("state/vessel");
    std::fs::create_dir_all(&holder).unwrap();
    let host = FakeHost::new();
    let connector = HostAgentConnector::new(
        host.clone(),
        Arc::new(LocalTransport),
        directory.clone(),
        Platform::current(),
        executable,
        Duration::from_secs(10),
        &system_base(),
    )
    .with_store(root.path().join("store"), holder);
    connector.connect().await.map(drop).unwrap();
    assert_eq!(std::fs::read(directory.join(AGENT_EXECUTABLE)).unwrap(), STUB_AGENT);
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
}

// Teardown stops exactly the processes serving this vessel's socket, the
// supervisor with them, and removes its directories. Another vessel's agent,
// whose socket path only shares a prefix, keeps running.
#[tokio::test]
async fn retire_kills_this_vessels_supervisor_and_agent_and_removes_its_directories() {
    use std::os::unix::process::CommandExt;
    let root = tempfile::Builder::new().prefix("fvr-").tempdir_in("/tmp").unwrap();
    let directory = root.path().join("v/vessel");
    let neighbour = root.path().join("v/vessel-2");
    let state = root.path().join("state/vessel");
    for path in [&directory, &neighbour, &state] {
        std::fs::create_dir_all(path).unwrap();
    }
    // Stand-ins that end their argv as the supervisor and agent do. `; :`
    // keeps the shell from exec-ing `sleep`, which would replace that argv.
    let spawn = |socket: &Path, supervisor: bool| {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 60; :", "drydock-agent"]);
        if supervisor {
            command.arg("--supervisor");
        }
        command.arg("--socket").arg(socket).process_group(0).spawn().unwrap()
    };
    let mut ours = [spawn(&directory.join(AGENT_SOCKET), true), spawn(&directory.join(AGENT_SOCKET), false)];
    let mut theirs = spawn(&neighbour.join(AGENT_SOCKET), false);
    retire_host_agent(&ProcessCommandRunner, &directory, std::slice::from_ref(&state), None).await.unwrap();
    for child in &mut ours {
        assert!(child.wait().is_ok_and(|status| !status.success()), "killed");
    }
    assert!(theirs.try_wait().unwrap().is_none(), "another vessel's agent keeps running");
    assert!(!directory.exists() && !state.exists());
    assert!(neighbour.exists());
    // Retiring again, with nothing left, succeeds.
    retire_host_agent(&ProcessCommandRunner, &directory, &[state], None).await.unwrap();
    // SAFETY: the stand-in leads its own process group.
    unsafe {
        libc::kill(-(theirs.id() as i32), libc::SIGKILL);
    }
    let _ = theirs.wait();
}

// Drydock stores content-cache entries read-only; retirement still removes them.
#[tokio::test]
async fn retire_removes_read_only_content_cache_entries() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("v/vessel");
    let entry = directory.join("drydock-agent.store/artifacts/abc/testing");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("hegel.md"), "skill").unwrap();
    std::fs::set_permissions(entry.join("hegel.md"), std::fs::Permissions::from_mode(0o400)).unwrap();
    for dir in entry.ancestors().take_while(|dir| dir.starts_with(&directory)) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    retire_host_agent(&ProcessCommandRunner, &directory, &[], None).await.unwrap();
    assert!(!directory.exists());
}

// #3653 under the agent: once the controller's handle is gone (the daemon
// stopped), nothing it started keeps forwarding. The guest socket reaches no
// daemon endpoint and no SSH client of the handle remains.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_controller_leaves_nothing_forwarding() {
    let Some(sshd) = Sshd::start().await else { return };
    let root = tempfile::Builder::new().prefix("fvs-").tempdir_in("/tmp").unwrap();
    let agent = SocketAgent::start(root.path().join(AGENT_SOCKET), b"current", false);
    let platform = Platform::current();
    let transport = sshd.transport();
    let socket = agent.socket.clone();
    let connector = {
        struct Direct(Arc<dyn AgentTransport>, PathBuf, Platform);
        #[async_trait]
        impl AgentConnector for Direct {
            async fn connect(&self) -> Result<Arc<dyn AgentClient>, String> {
                let connected = connect_current(|| self.0.open(&self.1), &platform_of(&self.2), b"current", Duration::from_secs(10)).await;
                connected.map(|client| Arc::new(DrydockClient(client)) as Arc<dyn AgentClient>).map_err(|e| e.to_string())
            }
        }
        Arc::new(Direct(transport, socket, platform))
    };
    let runner = Arc::new(AgentRunner::new(connector, root.path().into()));
    let daemon = root.path().join("daemon.sock");
    let listener = UnixListener::bind(&daemon).unwrap();
    let guest = root.path().join("flotilla.sock");
    runner.add_outward_forward("flotilla-daemon", &guest, &daemon).await.unwrap();
    let mut crew = UnixStream::connect(&guest).await.unwrap();
    crew.write_all(b"ping").await.unwrap();
    let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
    let mut ping = [0; 4];
    accepted.read_exact(&mut ping).await.unwrap();
    drop((crew, accepted));

    let port = format!("{}", sshd.reach.port);
    let clients = || {
        let output = std::process::Command::new("ps").args(["-axww", "-o", "command="]).output().unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("ssh ") && line.contains(&format!(" -p {port} ")))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert!(!clients().is_empty(), "the handle owns an SSH client while it runs");
    drop(runner);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !clients().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no SSH client of the stopped controller remains");
    if let Ok(mut crew) = UnixStream::connect(&guest).await {
        let _ = crew.write_all(b"ping").await;
        let mut buffer = [0; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), crew.read(&mut buffer)).await;
        // The agent outlives the controller and holds the unclaimed connection
        // until its claim deadline, so silence is as good as a close.
        assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_)) | Err(_)), "the guest socket carries nothing: {read:?}");
    }
    assert!(tokio::time::timeout(Duration::from_millis(500), listener.accept()).await.is_err(), "nothing reaches the daemon endpoint");
}

/// Dials the agent socket directly, holding the first connection's bytes
/// once `limit` have passed, as a slow link holds an upgrade's upload.
struct HeldTransport {
    limit: usize,
    held: Arc<tokio::sync::Notify>,
    opened: AtomicUsize,
}
#[async_trait]
impl AgentTransport for HeldTransport {
    async fn open(&self, socket: &Path) -> io::Result<ByteStream> {
        let stream = UnixStream::connect(socket).await?;
        if self.opened.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(Box::new(stream));
        }
        let held = Held { stream, room: self.limit, held: Arc::clone(&self.held) };
        Ok(Box::new(held))
    }
}
struct Held {
    stream: UnixStream,
    room: usize,
    held: Arc<tokio::sync::Notify>,
}
impl tokio::io::AsyncRead for Held {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for Held {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<io::Result<usize>> {
        if self.room == 0 {
            self.held.notify_one();
            return std::task::Poll::Pending;
        }
        let room = self.room.min(buf.len());
        let written = std::pin::Pin::new(&mut self.stream).poll_write(cx, &buf[..room]);
        if let std::task::Poll::Ready(Ok(written)) = &written {
            self.room -= written;
        }
        written
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Log lines written while a test runs.
#[derive(Clone, Default)]
struct Logged(Arc<StdMutex<Vec<u8>>>);
impl io::Write for Logged {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Logged {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

// #4054: a connect dropped partway through its agent upgrade's upload logs
// the abandoned upgrade: the vessel, the build's digest, how many bytes the
// agent holds and why. Mutation check: without the connect's upgrade watch
// the drop leaves no log line and this scenario fails.
#[tokio::test]
async fn a_connect_dropped_partway_through_an_upgrade_logs_it_abandoned() {
    let logged = Logged::default();
    let writer = logged.clone();
    let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
    let _logging = tracing::subscriber::set_default(subscriber);

    let root = tempfile::Builder::new().prefix("fvu-").tempdir_in("/tmp").unwrap();
    let directory = root.path().join("vessel");
    std::fs::create_dir(&directory).unwrap();
    let build = (0..2 * 1024 * 1024).map(|byte: u32| byte as u8).collect::<Vec<_>>();
    let executable = root.path().join("drydock-agent-build");
    std::fs::write(&executable, &build).unwrap();
    let _agent = SocketAgent::start(directory.join(AGENT_SOCKET), &build, true);
    let held = Arc::new(tokio::sync::Notify::new());
    let transport = Arc::new(HeldTransport { limit: 512 * 1024, held: Arc::clone(&held), opened: AtomicUsize::new(0) });
    let connector = Arc::new(
        HostAgentConnector::new(
            FakeHost::new(),
            transport,
            directory.clone(),
            Platform::current(),
            executable,
            Duration::from_secs(30),
            &BaseLayer::default(),
        )
        .with_vessel("host-direct-b1/convoy-1-work"),
    );

    let connecting = tokio::spawn({
        let connector = Arc::clone(&connector);
        async move { connector.connect().await.map(drop) }
    });
    tokio::time::timeout(Duration::from_secs(10), held.notified()).await.expect("the upload reaches the hold");
    connecting.abort();
    assert!(connecting.await.is_err_and(|error| error.is_cancelled()));

    let digest = format!("{:x}", Sha256::digest(&build));
    let line = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(line) = logged.text().lines().find(|line| line.contains("vessel agent upgrade abandoned partway")) {
                return line.to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the abandoned upgrade is logged");
    assert!(line.contains("vessel=host-direct-b1/convoy-1-work"), "{line}");
    assert!(line.contains(&format!("digest={digest}")), "{line}");
    assert!(line.contains(&format!("bytes_total={}", build.len())), "{line}");
    assert!(line.contains("reason=the connect was dropped before it finished"), "{line}");
    let sent = line.split("bytes_sent=").nth(1).and_then(|rest| rest.split_whitespace().next()).and_then(|sent| sent.parse::<usize>().ok());
    assert!(sent.is_some_and(|sent| sent > 0 && sent < build.len()), "{line}");
}

/// A host whose agent link never opens, as when SSH is unreachable.
#[derive(Default)]
struct Unreachable {
    opened: AtomicUsize,
}
#[async_trait]
impl AgentTransport for Unreachable {
    async fn open(&self, _socket: &Path) -> io::Result<ByteStream> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "unreachable"))
    }
}

// #4054 review: a connect that never reached the agent uploaded nothing, so
// its failure opens no second link to ask how far an upgrade got.
#[tokio::test]
async fn a_connect_that_never_reached_the_agent_asks_nothing_after_failing() {
    let root = tempfile::Builder::new().prefix("fvn-").tempdir_in("/tmp").unwrap();
    let executable = root.path().join("drydock-agent-build");
    std::fs::write(&executable, STUB_AGENT).unwrap();
    let transport = Arc::new(Unreachable::default());
    let connector = HostAgentConnector::new(
        FakeHost::new(),
        transport.clone(),
        root.path().join("vessel"),
        Platform::current(),
        executable,
        Duration::from_millis(200),
        &BaseLayer::default(),
    );
    connector.connect().await.map(drop).expect_err("the host is unreachable");
    let attempts = transport.opened.load(Ordering::SeqCst);
    // Every task the connect spawned has finished, so a probe it started has
    // already opened its link.
    let runtime = tokio::runtime::Handle::current();
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.metrics().num_alive_tasks() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the connect's tasks finish");
    assert_eq!(transport.opened.load(Ordering::SeqCst), attempts, "no probe follows a connect that never reached the agent");
}

#[test]
fn uname_names_map_to_drydock_platforms() {
    assert_eq!(platform_from_uname("Linux\n", "x86_64\n"), Platform::new("linux", "amd64"));
    assert_eq!(platform_from_uname("Darwin", "arm64"), Platform::new("darwin", "arm64"));
    assert_eq!(platform_from_uname("Linux", "aarch64"), Platform::new("linux", "arm64"));
}
