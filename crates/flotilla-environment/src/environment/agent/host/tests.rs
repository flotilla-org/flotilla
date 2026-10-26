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

use super::super::{
    tests::{SocketAgent, Sshd},
    AgentRunner, VesselAgent,
};
use super::*;

/// A host whose commands run for real in local temporary directories. Its
/// supervisor is the one part played in process: the executable flotilla
/// copies there is a stub that records `--supervise`, and the host then serves
/// the vessel socket with drydock's real Agent, unless one already serves it,
/// as the supervisor's lock would decide.
struct FakeHost {
    local: ProcessCommandRunner,
    agents: StdMutex<BTreeMap<PathBuf, Arc<SocketAgent>>>,
    current: Vec<u8>,
    starts: AtomicUsize,
}

/// The stub copied as the agent executable: it records the request and the
/// environment it was started with.
const STUB_AGENT: &[u8] =
    b"#!/bin/sh\nenv > \"$(dirname \"$0\")/started-environment\"\n[ \"$1\" = --supervise ] && : > \"$(dirname \"$0\")/supervise-requested\"\n";

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
        if std::fs::remove_file(&request).is_err() {
            return;
        }
        let socket = directory.join(AGENT_SOCKET);
        let mut agents = self.agents.lock().unwrap();
        if agents.contains_key(&socket) {
            return;
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        agents.insert(socket.clone(), SocketAgent::start(socket, &self.current, false));
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
        self.local.run(cmd, args, cwd, label).await
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
    async fn open(&self, socket: &Path) -> io::Result<Transport> {
        let stream = UnixStream::connect(socket).await?;
        Transport::yamux(Box::new(stream), true, Limits::default()).await
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
    retire_host_agent(&ProcessCommandRunner, &directory, std::slice::from_ref(&state)).await.unwrap();
    for child in &mut ours {
        assert!(child.wait().is_ok_and(|status| !status.success()), "killed");
    }
    assert!(theirs.try_wait().unwrap().is_none(), "another vessel's agent keeps running");
    assert!(!directory.exists() && !state.exists());
    assert!(neighbour.exists());
    // Retiring again, with nothing left, succeeds.
    retire_host_agent(&ProcessCommandRunner, &directory, &[state]).await.unwrap();
    // SAFETY: the stand-in leads its own process group.
    unsafe {
        libc::kill(-(theirs.id() as i32), libc::SIGKILL);
    }
    let _ = theirs.wait();
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
            async fn connect(&self) -> Result<Client, String> {
                connect_current(|| self.0.open(&self.1), &self.2, b"current", Duration::from_secs(10)).await.map_err(|e| e.to_string())
            }
        }
        Arc::new(Direct(transport, socket, platform))
    };
    let runner = Arc::new(AgentRunner::new(connector, root.path().into()));
    let daemon = root.path().join("daemon.sock");
    let listener = UnixListener::bind(&daemon).unwrap();
    let guest = root.path().join("flotilla.sock");
    runner.add_guest_forward("flotilla-daemon", &guest, &daemon).await.unwrap();
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
        assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the guest socket carries nothing: {read:?}");
    }
    assert!(tokio::time::timeout(Duration::from_millis(500), listener.accept()).await.is_err(), "nothing reaches the daemon endpoint");
}

#[test]
fn uname_names_map_to_drydock_platforms() {
    assert_eq!(platform_from_uname("Linux\n", "x86_64\n"), Platform::new("linux", "amd64"));
    assert_eq!(platform_from_uname("Darwin", "arm64"), Platform::new("darwin", "arm64"));
    assert_eq!(platform_from_uname("Linux", "aarch64"), Platform::new("linux", "arm64"));
}
