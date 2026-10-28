use super::*;
use drydock_agent::Agent;
use drydock_transport::{Opener, Purpose};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

/// Real drydock agent and client, with only their byte transport in memory.
/// The composition-root fake reloads Agent after run returns an upgrade path;
/// drydock's actual files/upgrade/hello handlers decide that path and its bytes.
pub(crate) struct MemoryAgent {
    transports: mpsc::Sender<drydock_transport::ByteStream>,
    current: Vec<u8>,
    pub(crate) starts: Arc<AtomicUsize>,
    pub(crate) upgrades: Arc<AtomicUsize>,
    guest: Arc<Mutex<Option<Opener>>>,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl MemoryAgent {
    pub(crate) fn new(mismatched: bool) -> Arc<Self> {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("agent");
        let current = b"current drydock executable".to_vec();
        std::fs::write(&installed, b"previous drydock executable").unwrap();
        let (transports, receiver) = mpsc::channel::<drydock_transport::ByteStream>(8);
        let connections = Arc::new(MemoryConnections { incoming: Mutex::new(receiver) });
        let starts = Arc::new(AtomicUsize::new(0));
        let upgrades = Arc::new(AtomicUsize::new(0));
        let (started, upgraded) = (starts.clone(), upgrades.clone());
        let guest = Arc::new(Mutex::new(None));
        let opener = guest.clone();
        let mut fingerprint = if mismatched { "previous-build".to_string() } else { format!("{:x}", Sha256::digest(&current)) };
        let task = tokio::spawn(async move {
            loop {
                let transport = Transport::native(connections.clone(), Limits::default()).unwrap();
                *opener.lock().await = Some(transport.opener.clone());
                started.fetch_add(1, Ordering::SeqCst);
                if let Ok(path) = Agent::new(transport, fingerprint.clone(), installed.clone()).run().await {
                    let bytes = tokio::fs::read(&path).await.unwrap();
                    fingerprint = format!("{:x}", Sha256::digest(&bytes));
                    upgraded.fetch_add(1, Ordering::SeqCst);
                    tokio::fs::rename(path, &installed).await.unwrap();
                }
            }
        });
        Arc::new(Self { transports, current, starts, upgrades, guest, task, _directory: directory })
    }
    pub(crate) fn stop(&self) {
        self.task.abort();
    }
    async fn open(&self) -> io::Result<Transport> {
        let (host, guest) = tokio::io::duplex(8192);
        self.transports.send(Box::new(guest)).await.map_err(|_| io::Error::other("test agent stopped"))?;
        Transport::yamux(Box::new(host), true, Limits::default()).await
    }

    async fn guest(&self) -> Opener {
        self.guest.lock().await.as_ref().unwrap().clone()
    }
}
struct MemoryConnections {
    incoming: Mutex<mpsc::Receiver<drydock_transport::ByteStream>>,
}
#[async_trait]
impl drydock_transport::Connections for MemoryConnections {
    async fn connect(&self) -> io::Result<drydock_transport::ByteStream> {
        Err(io::Error::new(io::ErrorKind::NotConnected, "use accepted multiplexed link"))
    }
    async fn accept(&self) -> io::Result<drydock_transport::ByteStream> {
        self.incoming.lock().await.recv().await.ok_or_else(|| io::Error::other("test transport closed"))
    }
}
impl Drop for MemoryAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[async_trait]
impl AgentConnector for MemoryAgent {
    async fn connect(&self) -> Result<Client, String> {
        connect_current(|| self.open(), &Platform::current(), &self.current, Duration::from_secs(5)).await.map_err(|e| e.to_string())
    }
}
impl TestAgent for MemoryAgent {
    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
    fn upgrades(&self) -> usize {
        self.upgrades.load(Ordering::SeqCst)
    }
}

/// A vessel agent the runner scenarios drive, whatever carries its streams.
pub(crate) trait TestAgent: AgentConnector {
    /// Agent incarnations so far: an upgrade re-execs into a new one.
    fn starts(&self) -> usize;
    fn upgrades(&self) -> usize;
}

/// Drydock's real Agent serving a real Unix socket, as the agent binary does,
/// in this process. An upgrade re-binds the socket under the new fingerprint.
pub(crate) struct SocketAgent {
    pub(crate) socket: PathBuf,
    starts: Arc<AtomicUsize>,
    upgrades: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl SocketAgent {
    /// Serve `socket`, reporting the digest of `current` unless `mismatched`.
    pub(crate) fn start(socket: PathBuf, current: &[u8], mismatched: bool) -> Arc<Self> {
        let directory = tempfile::Builder::new().prefix("fa-").tempdir_in("/tmp").unwrap();
        let installed = directory.path().join("agent");
        std::fs::write(&installed, b"previous drydock executable").unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let upgrades = Arc::new(AtomicUsize::new(0));
        let (started, upgraded, path) = (starts.clone(), upgrades.clone(), socket.clone());
        let mut fingerprint = if mismatched { "previous-build".to_string() } else { format!("{:x}", Sha256::digest(current)) };
        let peer = directory.path().join("host.sock");
        let task = tokio::spawn(async move {
            loop {
                let _ = std::fs::remove_file(&path);
                let listener = UnixListener::bind(&path).unwrap();
                let connections = drydock_transport::native::UnixConnections::new(listener, peer.clone());
                let transport = Transport::native(Arc::new(connections), Limits::default()).unwrap();
                started.fetch_add(1, Ordering::SeqCst);
                match Agent::new(transport, fingerprint.clone(), installed.clone()).run().await {
                    Ok(upgrade) => {
                        let bytes = tokio::fs::read(&upgrade).await.unwrap();
                        fingerprint = format!("{:x}", Sha256::digest(&bytes));
                        upgraded.fetch_add(1, Ordering::SeqCst);
                        tokio::fs::rename(upgrade, &installed).await.unwrap();
                    }
                    Err(_) => return,
                }
            }
        });
        Arc::new(Self { socket, starts, upgrades, task, _directory: directory })
    }
    /// The agent dies: its socket stays behind, refusing connections.
    pub(crate) fn kill(&self) {
        self.task.abort();
    }
    pub(crate) fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}
impl Drop for SocketAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A real OpenSSH server on loopback with disposable keys, running as the
/// current user and reading no ambient configuration. `FLOTILLA_TEST_SSHD`
/// names the binary when it is not `/usr/sbin/sshd`. Without one the SSH
/// variants skip, unless `FLOTILLA_TEST_SSHD_REQUIRED` is set, as CI's test
/// job sets it.
pub(crate) struct Sshd {
    pub(crate) reach: host::SshAgentReach,
    child: tokio::process::Child,
    pub(crate) directory: tempfile::TempDir,
}
impl Sshd {
    pub(crate) async fn start() -> Option<Self> {
        let binary = std::env::var_os("FLOTILLA_TEST_SSHD").map(PathBuf::from).unwrap_or_else(|| "/usr/sbin/sshd".into());
        if !binary.exists() {
            assert!(
                std::env::var_os("FLOTILLA_TEST_SSHD_REQUIRED").is_none(),
                "SSH contract tests require sshd at {} (or set FLOTILLA_TEST_SSHD)",
                binary.display()
            );
            eprintln!("skipping SSH variant: no sshd at {} (set FLOTILLA_TEST_SSHD)", binary.display());
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory =
            tempfile::Builder::new().prefix("fssh-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in("/tmp").unwrap();
        let root = directory.path();
        for key in ["host_key", "user_key"] {
            let status = tokio::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(key))
                .status()
                .await
                .expect("ssh-keygen");
            assert!(status.success());
        }
        let user = String::from_utf8(std::process::Command::new("id").arg("-un").output().unwrap().stdout).unwrap().trim().to_string();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host_key\nAuthorizedKeysFile {root}/user_key.pub\nPidFile {root}/sshd.pid\n\
             StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAllowUsers {user}\n\
             AllowStreamLocalForwarding local\nStreamLocalBindUnlink yes\n",
            root = root.display()
        );
        std::fs::write(root.join("sshd_config"), config).unwrap();
        let log = std::fs::File::create(root.join("sshd.log")).unwrap();
        let mut child = tokio::process::Command::new(binary)
            .args(["-D", "-e", "-f"])
            .arg(root.join("sshd_config"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .expect("start sshd");
        for _ in 0..250 {
            assert!(child.try_wait().unwrap().is_none(), "sshd exited: {}", std::fs::read_to_string(root.join("sshd.log")).unwrap());
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                let reach = host::SshAgentReach {
                    address: "127.0.0.1".into(),
                    port,
                    user,
                    host_key: std::fs::read_to_string(root.join("host_key.pub")).unwrap().trim().into(),
                    identity_file: root.join("user_key"),
                };
                return Some(Self { reach, child, directory });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sshd did not listen: {}", std::fs::read_to_string(root.join("sshd.log")).unwrap())
    }
    pub(crate) fn transport(&self) -> Arc<dyn host::AgentTransport> {
        Arc::new(host::SshAgentTransport::new(self.reach.clone(), Duration::from_secs(10)))
    }
}
impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// The socket agent reached through drydock's SSH transport and the loopback
/// sshd, as an SSH host's vessel agent is.
pub(crate) struct SshAgent {
    agent: Arc<SocketAgent>,
    transport: Arc<dyn host::AgentTransport>,
    current: Vec<u8>,
    _sshd: Sshd,
}
impl SshAgent {
    pub(crate) async fn new(mismatched: bool) -> Option<Arc<Self>> {
        let sshd = Sshd::start().await?;
        let current = b"current drydock executable".to_vec();
        let agent = SocketAgent::start(sshd.directory.path().join("agent.sock"), &current, mismatched);
        Some(Arc::new(Self { transport: sshd.transport(), agent, current, _sshd: sshd }))
    }
}
#[async_trait]
impl AgentConnector for SshAgent {
    async fn connect(&self) -> Result<Client, String> {
        let socket = self.agent.socket.clone();
        connect_current(|| self.transport.open(&socket), &Platform::current(), &self.current, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())
    }
}
impl TestAgent for SshAgent {
    fn starts(&self) -> usize {
        self.agent.starts()
    }
    fn upgrades(&self) -> usize {
        self.agent.upgrades.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy)]
enum Over {
    Memory,
    Ssh,
}
async fn agent_over(over: Over, mismatched: bool) -> Option<Arc<dyn TestAgent>> {
    match over {
        Over::Memory => Some(MemoryAgent::new(mismatched)),
        Over::Ssh => SshAgent::new(mismatched).await.map(|agent| agent as Arc<dyn TestAgent>),
    }
}

/// Run one runner scenario against each agent transport: in memory, and over
/// SSH to an agent on a real socket (#3792). `mismatched` starts the agent
/// from an older build.
macro_rules! over_each_transport {
    ($scenario:ident) => {
        over_each_transport!($scenario, false);
    };
    ($scenario:ident, $mismatched:expr) => {
        mod $scenario {
            #[tokio::test]
            async fn memory() {
                super::$scenario(super::agent_over(super::Over::Memory, $mismatched).await.unwrap()).await;
            }
            #[tokio::test(flavor = "multi_thread")]
            async fn ssh() {
                if let Some(agent) = super::agent_over(super::Over::Ssh, $mismatched).await {
                    super::$scenario(agent).await;
                }
            }
        }
    };
}

fn runner(agent: &Arc<MemoryAgent>, host: &Path) -> Arc<AgentRunner> {
    Arc::new(AgentRunner::new(agent.clone(), host.into()))
}
fn runner_for(agent: &Arc<dyn TestAgent>, host: &Path) -> Arc<AgentRunner> {
    Arc::new(AgentRunner::new(agent.clone(), host.into()))
}

// Issue 3486: readiness requires the compatible hello, and a mismatched tools
// image upgrades by push/re-exec before any command uses the agent's API.
over_each_transport!(hello_upgrades_mismatched_agent_before_exec, true);
async fn hello_upgrades_mismatched_agent_before_exec(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner_for(&agent, directory.path());
    VesselAgent::ready(&runner).await.unwrap();
    assert_eq!(agent.upgrades(), 1);
    assert_eq!(agent.starts(), 2);
    let result =
        runner.run_output("sh", &["-c", "printf hello; printf problem >&2; exit 7"], Path::new("/"), &ChannelLabel::Default).await.unwrap();
    assert_eq!((result.stdout.as_str(), result.stderr.as_str(), result.exit_code), ("hello", "problem", Some(7)));
}

// Runner contract: arguments and working directories retain their values;
// stdout, stderr, nonzero exits, private writes, empty files, binary transfer,
// stdin and ensure-if-absent all cross the real agent rather than docker exec.
over_each_transport!(runner_and_material_contract);
async fn runner_and_material_contract(agent: Arc<dyn TestAgent>) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let runner = runner_for(&agent, directory.path());
    runner.configure_environment(BTreeMap::from([("FLOTILLA_AGENT_TEST".into(), "pushed environment".into())])).await.unwrap();
    let cwd = directory.path().join("work with spaces");
    std::fs::create_dir(&cwd).unwrap();
    let output = runner
        .run("sh", &["-c", "printf '%s|%s|%s' \"$PWD\" \"$1\" \"$FLOTILLA_AGENT_TEST\"", "test", "a' b\nc"], &cwd, &ChannelLabel::Default)
        .await
        .unwrap();
    // The shell's `$PWD` is the resolved directory (`/private/var` on macOS).
    assert_eq!(output, format!("{}|a' b\nc|pushed environment", std::fs::canonicalize(&cwd).unwrap().display()));
    let path = cwd.join("credentials").join("auth.json");
    runner.write_file_with_mode(&path, "secret", 0o400).await.unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o400);
    assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    runner.write_file_with_mode(&path, "rotated", 0o400).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"rotated");
    let ensure = cwd.join("first");
    assert_eq!(runner.ensure_file(&ensure, "original").await.unwrap(), "original");
    assert_eq!(runner.ensure_file(&ensure, "replacement").await.unwrap(), "original");
    let source = cwd.join("source");
    let destination = cwd.join("destination");
    let copied = cwd.join("copied");
    let bytes: Vec<u8> = (0..=255).collect();
    std::fs::write(&source, &bytes).unwrap();
    runner.write_file_from(&source, &destination).await.unwrap();
    runner.read_file_to(&destination, &copied).await.unwrap();
    assert_eq!(std::fs::read(copied).unwrap(), bytes);
    assert_eq!(runner.run_with_input("cat", &[], &cwd, &ChannelLabel::Default, b"stdin secret").await.unwrap(), "stdin secret");
    // Input and output larger than any pipe buffer stream concurrently, and a
    // program that never reads its input still succeeds, as with a redirect.
    let large = "0123456789abcdef".repeat(256 * 1024);
    assert_eq!(runner.run_with_input("cat", &[], &cwd, &ChannelLabel::Default, large.as_bytes()).await.unwrap(), large);
    assert_eq!(runner.run_with_input("true", &[], &cwd, &ChannelLabel::Default, large.as_bytes()).await.unwrap(), "");
    assert!(runner.run_with_input("false", &[], &cwd, &ChannelLabel::Default, b"").await.is_err());
    runner.write_file(&destination, "").await.unwrap();
    assert!(std::fs::read(&destination).unwrap().is_empty());
    assert!(runner.write_file_with_mode(&destination, "bad", 0o1000).await.is_err());
    assert!(runner.run("sh", &[], &cwd.join("missing"), &ChannelLabel::Default).await.is_err());
    assert!(runner.same_filesystem(&source, &destination).await.is_err());
}

// Vessel delivery (#3964 contract 3) runs every step through the agent's own file
// facilities. The real agent creates a home with exactly its mode, keeping what is
// already there, and refuses a symbolic link in its place. It reads a missing
// record as absent and a written one back within its limit, stats without
// following links, replaces a link in place, and resolves the contained base.
over_each_transport!(delivery_operations_cross_the_agent);
async fn delivery_operations_cross_the_agent(agent: Arc<dyn TestAgent>) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let vessel: Arc<dyn VesselAgent> = Arc::new(runner_for(&agent, directory.path()));
    let home = directory.path().join("agent-homes/requesters/convoy/coder/claude");
    vessel.create_dir(&home, 0o700).await.unwrap();
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
    let record = home.join(".record");
    assert_eq!(vessel.read(&record, 1024).await.unwrap(), None);
    vessel.write(&record, b"skills\tdigest\tagent-push\n".to_vec(), 0o600).await.unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    vessel.create_dir(&home, 0o700).await.unwrap();
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700, "an existing home is narrowed again");
    assert_eq!(
        vessel.read(&record, 1024).await.unwrap().as_deref(),
        Some(b"skills\tdigest\tagent-push\n".as_slice()),
        "its contents are kept"
    );
    assert!(vessel.read(&record, 4).await.is_err(), "a read beyond its limit is refused");
    assert_eq!(vessel.stat(&record).await.unwrap(), Some(EntryKind::File));
    assert_eq!(vessel.stat(&home.join("absent")).await.unwrap(), None);
    let login = home.join("auth.json");
    vessel.symlink(&login, &record).await.unwrap();
    vessel.symlink(&login, &home.join("rotated")).await.unwrap();
    assert_eq!(std::fs::read_link(&login).unwrap(), home.join("rotated"), "a link is replaced in place");
    assert_eq!(vessel.stat(&login).await.unwrap(), Some(EntryKind::Symlink), "stat never follows a link");
    assert_eq!(std::fs::metadata(&record).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(vessel.create_dir(Path::new("relative"), 0o700).await.is_err());
    let link = directory.path().join("linked-home");
    std::os::unix::fs::symlink(&home, &link).unwrap();
    assert!(vessel.create_dir(&link, 0o755).await.is_err(), "a linked home is refused, never followed");
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(vessel.writable_base().await.unwrap(), Path::new(super::super::runner::CONTAINED_WRITABLE_CONFIG_BASE));
}

// #3909: a pushed artifact becomes a verified store entry, and a link into it reads as
// the pushed bytes; bytes that do not hash to the digest, or a kind the store cannot
// unpack, leave nothing behind.
over_each_transport!(artifacts_cross_the_agent_by_digest);
async fn artifacts_cross_the_agent_by_digest(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::tempdir().unwrap();
    let vessel: Arc<dyn VesselAgent> = Arc::new(runner_for(&agent, directory.path()));
    let local = directory.path().join("harness");
    std::fs::write(&local, b"pinned harness").unwrap();
    let sha256 = format!("{:x}", Sha256::digest(b"pinned harness"));
    let link = directory.path().join("home/.flotilla/harness/bin/claude");
    let other = format!("{:x}", Sha256::digest(b"other bytes"));
    let refused = vessel.push_artifact(&other, &local, PinUnpack::File).await.expect_err("a mismatched digest");
    assert!(vessel.link_artifact(&link, &other, Path::new("")).await.is_err(), "nothing was stored: {refused}");
    vessel.push_artifact(&sha256, &local, PinUnpack::File).await.unwrap();
    vessel.link_artifact(&link, &sha256, Path::new("")).await.unwrap();
    assert_eq!(std::fs::read(&link).unwrap(), b"pinned harness");
    assert!(vessel.push_artifact(&sha256, &local, PinUnpack::TarXz).await.unwrap_err().contains("tar.xz"));
}

// Issue 3486: flotilla's CLI cycle is separate from drydock's. Updating files
// changes what exec sees while the same agent incarnation continues running.
over_each_transport!(pushing_new_cli_layer_does_not_reexec_agent);
async fn pushing_new_cli_layer_does_not_reexec_agent(agent: Arc<dyn TestAgent>) {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    let host = tempfile::tempdir().unwrap();
    let guest = tempfile::tempdir().unwrap();
    let runner = runner_for(&agent, host.path());
    let source = host.path().join("flotilla-vessel");
    let target = guest.path().join("bin/flotilla");
    let layer = vec![EnvironmentTool::new("flotilla", &target).with_asset(EnvironmentToolAsset::new(
        &source,
        &target,
        EnvironmentToolAssetKind::File,
        EnvironmentToolAssetAccess::ReadOnly,
        "vessel CLI",
    ))];
    for contents in [b"first CLI".as_slice(), b"new CLI", b"new CLI"] {
        std::fs::write(&source, contents).unwrap();
        runner.push_tools(&layer).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), contents);
        assert_eq!(agent.starts(), 1);
        assert_eq!(agent.upgrades(), 0);
    }
}

// The guest sees only its own forward IDs. A pathname or another vessel's ID
// is never treated as a host destination; one host endpoint cannot be registered
// by two vessel clients, including through a symlink alias.
#[tokio::test]
async fn vessels_cannot_cross_forward_registries_or_open_host_control() {
    let a = MemoryAgent::new(false);
    let b = MemoryAgent::new(false);
    let dir = tempfile::tempdir().unwrap();
    let endpoint = dir.path().join("host-endpoint");
    let _listener = UnixListener::bind(&endpoint).unwrap();
    let ca = a.connect().await.unwrap();
    let cb = b.connect().await.unwrap();
    ca.register_host_endpoint("vessel-a".into(), Endpoint::Unix(endpoint.clone())).unwrap();
    assert!(cb.register_host_endpoint("vessel-b".into(), Endpoint::Unix(endpoint.clone())).is_err());
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&endpoint, &alias).unwrap();
    assert!(cb.register_host_endpoint("alias".into(), Endpoint::Unix(alias)).is_err());
    for id in ["vessel-a".to_string(), endpoint.to_string_lossy().into_owned(), "/run/flotilla/control.sock".into()] {
        let mut stream = b.guest().await.open(Purpose::Forward { id }).await.unwrap();
        let length = stream.read_u32().await.unwrap();
        assert!(length < 64 * 1024);
        let mut result = vec![0; length as usize];
        stream.read_exact(&mut result).await.unwrap();
        let result: Result<(), drydock_agent::AgentError> = serde_json::from_slice(&result).unwrap();
        assert!(result.is_err());
    }
    let mut control = b.guest().await.open(Purpose::Control).await.unwrap();
    let read = tokio::time::timeout(Duration::from_secs(1), control.read_u8()).await.unwrap();
    assert!(read.is_err(), "the host client must not serve a guest control API");
}

// Attach is an opaque host-to-guest forward. The service fake stands in for
// cleat's seat socket: EOF must arrive when a viewer disconnects, so the seat
// can be acquired by the next viewer without killing the session/server.
over_each_transport!(attach_viewer_disconnect_releases_guest_seat);
async fn attach_viewer_disconnect_releases_guest_seat(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::Builder::new().prefix("fat-").tempdir_in("/tmp").unwrap();
    let runner = runner_for(&agent, directory.path());
    let guest = directory.path().join("cleat.sock");
    let listener = UnixListener::bind(&guest).unwrap();
    let (released, mut releases) = mpsc::channel(2);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut seat, _) = listener.accept().await.unwrap();
            seat.write_all(b"seat granted").await.unwrap();
            let mut contents = Vec::new();
            seat.read_to_end(&mut contents).await.unwrap();
            released.send(contents).await.unwrap();
        }
    });
    let socket = VesselAgent::forward_socket(&runner, &guest).await.unwrap();
    assert_eq!(socket, VesselAgent::forward_socket(&runner, &guest).await.unwrap());
    for contents in [b"viewer one".as_slice(), b"viewer two"] {
        let mut viewer = UnixStream::connect(&socket).await.unwrap();
        let mut response = [0; 12];
        viewer.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"seat granted");
        viewer.write_all(contents).await.unwrap();
        drop(viewer);
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), releases.recv()).await.unwrap().unwrap(), contents);
    }
    server.await.unwrap();
    assert_eq!(agent.starts(), 1);
}

// Empty/invalid current images and an unreachable initial transport must fail
// readiness, rather than reporting a container provisioned on Docker run alone.
#[tokio::test]
async fn missing_hello_is_bounded() {
    let error = connect_current(
        || async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "not started")) },
        &Platform::current(),
        b"current",
        Duration::from_millis(20),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

// An agent that refuses the upgrade upload (an upload of this digest is already
// in progress) refuses it on every attempt. Connect fails at once with that
// refusal, having opened one transport for the hello and none to retry.
#[tokio::test]
async fn conflicting_upgrade_upload_fails_promptly_with_the_refusal() {
    let agent = MemoryAgent::new(true);
    let executable = agent.current.clone();
    let digest = format!("{:x}", Sha256::digest(&executable));
    let first = Client::connect(agent.open().await.unwrap(), String::new()).await.unwrap();
    first.create_upload(&digest, Some(executable.len() as u64), tokio::io::empty(), false).await.unwrap();
    drop(first);

    let opens = AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let error = connect_current(
        || {
            opens.fetch_add(1, Ordering::SeqCst);
            agent.open()
        },
        &Platform::current(),
        &executable,
        Duration::from_secs(30),
    )
    .await
    .err()
    .expect("conflict");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(drydock_agent::AgentError::of(&error).map(|e| e.kind), Some(drydock_agent::ErrorKind::Conflict), "{error}");
    assert!(error.to_string().contains("resume it"), "{error}");
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

// Transient failures are retried with growing pauses, and the deadline reports
// the last real failure rather than the timeout that cut the last attempt.
#[tokio::test(start_paused = true)]
async fn transient_failures_back_off_and_report_the_last_error() {
    let times = Mutex::new(Vec::new());
    let started = tokio::time::Instant::now();
    let error = connect_current(
        || {
            let mut times = times.try_lock().unwrap();
            times.push(started.elapsed());
            async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "not listening")) }
        },
        &Platform::current(),
        b"current",
        Duration::from_millis(700),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("not listening"), "{error}");
    let times = times.into_inner();
    let ms = |n| Duration::from_millis(n);
    assert_eq!(times, [ms(0), ms(50), ms(150), ms(350)]);
}

// Generated lifecycle sequences interleave empty/binary material writes, reads,
// duplicate CLI pushes and invalid modes. After every operation, visible files
// match the model and the compatible agent remains the same incarnation.
#[hegel::test]
fn generated_material_and_layer_cycles_keep_agent_alive(tc: hegel::TestCase) {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
    let operations: Vec<_> = (0..count)
        .map(|_| {
            let op = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(32));
            let byte = tc.draw(gs::integers::<u8>().min_value(0).max_value(255));
            (op, vec![byte; length])
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let memory = MemoryAgent::new(false);
        let directory = tempfile::tempdir().unwrap();
        let runner = runner(&memory, directory.path());
        VesselAgent::ready(&runner).await.unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("guest/cli");
        let output = directory.path().join("readback");
        let tools = vec![EnvironmentTool::new("cli", &target).with_asset(EnvironmentToolAsset::new(
            &source,
            &target,
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "test layer",
        ))];
        let mut model: Option<Vec<u8>> = None;
        let material = directory.path().join("guest/material");
        let mut material_model: Option<Vec<u8>> = None;
        for (operation, bytes) in operations {
            match operation {
                0 => {
                    std::fs::write(&source, &bytes).unwrap();
                    runner.write_file_from(&source, &material).await.unwrap();
                    material_model = Some(bytes);
                }
                1 => {
                    std::fs::write(&source, &bytes).unwrap();
                    runner.push_tools(&tools).await.unwrap();
                    model = Some(bytes);
                }
                2 => {
                    assert!(runner.write_file_with_mode(&target, "invalid", 0o1000).await.is_err());
                }
                _ => {
                    assert_eq!(
                        runner.run("printf", &["still alive"], Path::new("/"), &ChannelLabel::Default).await.unwrap(),
                        "still alive"
                    );
                }
            }
            if let Some(model) = &model {
                runner.read_file_to(&target, &output).await.unwrap();
                assert_eq!(&std::fs::read(&output).unwrap(), model);
            }
            if let Some(expected) = &material_model {
                runner.read_file_to(&material, &output).await.unwrap();
                assert_eq!(&std::fs::read(&output).unwrap(), expected);
            }
            assert_eq!(memory.starts.load(Ordering::SeqCst), 1);
            assert_eq!(memory.upgrades.load(Ordering::SeqCst), 0);
        }
    });
}

// A running generation stays usable after current moves: files are installed
// under their content identity before the one published link changes. Only the
// selected source generation is needed; old guest generations remain intact.
over_each_transport!(pushes_complete_generations_and_retains_old_cleat_files);
async fn pushes_complete_generations_and_retains_old_cleat_files(memory: Arc<dyn TestAgent>) {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    let host = tempfile::tempdir().unwrap();
    let guest = tempfile::tempdir().unwrap();
    let runner = runner_for(&memory, guest.path());
    let sources = host.path().join("source");
    std::fs::create_dir(&sources).unwrap();
    let composition = crate::composition::flotilla_tools();
    let cli = sources.join("bin/flotilla-vessel");
    let cleat = sources.join("bin/cleat");
    let library = sources.join("lib/libghostty-vt.so.0");
    let terminfo = sources.join("share/terminfo");
    for path in [&cli, &cleat, &library] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    std::fs::create_dir_all(terminfo.join("x")).unwrap();
    std::fs::write(terminfo.join("x/xterm-ghostty"), b"terminfo").unwrap();
    std::fs::write(terminfo.join("LICENSE"), b"license").unwrap();
    let fleet = host.path().join("fleet");
    let destination = guest.path().join("fleet");
    let mut tool = EnvironmentTool::new("fleet", destination.join("bin/flotilla"));
    for name in std::iter::once(PathBuf::from("tools")).chain(composition.exposed_directories()) {
        tool = tool.with_asset(EnvironmentToolAsset::new(
            fleet.join(&name),
            destination.join(&name),
            EnvironmentToolAssetKind::Directory,
            EnvironmentToolAssetAccess::ReadOnly,
            "generation tools",
        ));
    }
    let mut previous = None;
    for bytes in [b"generation one".as_slice(), b"generation two"] {
        for path in [&cli, &cleat, &library] {
            std::fs::write(path, bytes).unwrap();
        }
        let generation = crate::tools::publish(&fleet, crate::tools::Payload { composition, root: &sources }).unwrap();
        runner.push_tools(std::slice::from_ref(&tool)).await.unwrap();
        assert_eq!(std::fs::read(destination.join("bin/flotilla")).unwrap(), bytes);
        assert_eq!(std::fs::read(destination.join("bin/cleat")).unwrap(), bytes);
        assert_eq!(std::fs::read_link(destination.join("tools/current")).unwrap(), Path::new(generation.file_name().unwrap()));
        if let Some((old, bytes)) = previous {
            assert_eq!(std::fs::read(destination.join("tools").join(old).join("lib/libghostty-vt.so.0")).unwrap(), bytes);
        }
        previous = Some((generation.file_name().unwrap().to_os_string(), bytes.to_vec()));
    }
    assert_eq!(memory.starts(), 1);
    assert_eq!(memory.upgrades(), 0);
}

// Relative cwd never silently selects the agent's default directory, including
// the file-stream route. A write without a mode takes the agent's private
// default, so a credential written through the runner is never world-readable.
over_each_transport!(cwd_is_absolute_and_default_file_mode_is_private);
async fn cwd_is_absolute_and_default_file_mode_is_private(memory: Arc<dyn TestAgent>) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let runner = runner_for(&memory, directory.path());
    for cwd in [".", "relative", "../parent", ""] {
        assert!(runner.run("pwd", &[], Path::new(cwd), &ChannelLabel::Default).await.unwrap_err().contains("must be absolute"));
        let output = directory.path().join("output");
        assert!(runner.run_to_file("pwd", &[], Path::new(cwd), &output).await.unwrap_err().contains("must be absolute"));
        assert!(!output.exists());
    }
    let file = directory.path().join("default-mode");
    runner.write_file(&file, "default").await.unwrap();
    assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
}

struct ReplaceableConnector(Mutex<Arc<MemoryAgent>>);
#[async_trait]
impl AgentConnector for ReplaceableConnector {
    async fn connect(&self) -> Result<Client, String> {
        self.0.lock().await.clone().connect().await
    }
}
// A transport failure during add_host_forward must remove the bound listener;
// after explicit reconnection the same handle can retry successfully.
#[tokio::test]
async fn failed_forward_registration_cleans_path_and_can_retry() {
    let memory = MemoryAgent::new(false);
    let directory = tempfile::tempdir().unwrap();
    let guest = directory.path().join("guest.sock");
    let listener = UnixListener::bind(&guest).unwrap();
    let host = directory.path().join("host");
    std::fs::create_dir(&host).unwrap();
    let connector = Arc::new(ReplaceableConnector(Mutex::new(memory.clone())));
    let runner = Arc::new(AgentRunner::new(connector.clone(), host.clone()));
    runner.ready().await.unwrap();
    memory.stop();
    assert!(tokio::time::timeout(Duration::from_secs(2), runner.forward_socket(&guest)).await.unwrap().is_err());
    assert_eq!(std::fs::read_dir(&host).unwrap().count(), 0);
    assert!(!runner.healthy().await);
    *connector.0.lock().await = MemoryAgent::new(false);
    let forwarded = runner.forward_socket(&guest).await.unwrap();
    let viewer = UnixStream::connect(forwarded).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await.unwrap().unwrap();
    drop(viewer);
}

// Reconnects within one daemon generation use the same pinned executable bytes,
// even if the source file is later removed (e.g. fleet launcher cleanup).
#[tokio::test]
async fn connector_caches_generation_bytes_after_first_read() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("agent");
    let mut executable = vec![0; 64];
    executable[..6].copy_from_slice(b"\x7fELF\x02\x01");
    executable[18] = 62;
    std::fs::write(&file, executable).unwrap();
    let connector = UnixAgentConnector::new(
        directory.path().join("absent.sock"),
        file.clone(),
        crate::guest_tools::linux_amd64(),
        Duration::from_millis(20),
    );
    assert!(connector.connect().await.is_err());
    std::fs::remove_file(file).unwrap();
    assert!(!connector.connect().await.err().unwrap().contains("read current drydock agent"));
}

struct ChangingSourceConnector {
    memory: Arc<MemoryAgent>,
    source: PathBuf,
}
#[async_trait]
impl AgentConnector for ChangingSourceConnector {
    async fn connect(&self) -> Result<Client, String> {
        std::fs::write(&self.source, b"changed after snapshot").unwrap();
        self.memory.connect().await
    }
}
// Publication and the identity cache share a single immutable byte snapshot;
// changing a source during connection cannot poison the duplicate-layer cache.
#[tokio::test]
async fn layer_hash_and_published_bytes_use_the_same_snapshot() {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let target = directory.path().join("guest/cli");
    std::fs::write(&source, b"original snapshot").unwrap();
    let runner = Arc::new(AgentRunner::new(
        Arc::new(ChangingSourceConnector { memory: MemoryAgent::new(false), source: source.clone() }),
        directory.path().into(),
    ));
    let first_source = directory.path().join("first-source");
    std::fs::write(&first_source, b"first file opens the connection").unwrap();
    let tools = [EnvironmentTool::new("cli", &target)
        .with_asset(EnvironmentToolAsset::new(
            &source,
            &target,
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "cli",
        ))
        .with_asset(EnvironmentToolAsset::new(
            &first_source,
            directory.path().join("guest/0-first"),
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "first",
        ))];
    runner.push_tools(&tools).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"original snapshot");
    runner.push_tools(&tools).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"changed after snapshot");
}

// Scenario: a reconcile reuses the digest without reading unchanged binaries;
// replacing a file with equal-length content invalidates that cache.
#[test]
fn layer_identity_cache_skips_reads_and_detects_atomic_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("cli");
    std::fs::write(&source, b"old").unwrap();
    let entries = vec![(source.clone(), PathBuf::from("/opt/flotilla/cli"), 0o755)];
    let reads = AtomicUsize::new(0);
    let read = |path: &Path| {
        reads.fetch_add(1, Ordering::SeqCst);
        std::fs::read(path)
    };
    let first = snapshot_layer(entries.clone(), vec![], None, read).unwrap().unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(snapshot_layer(entries.clone(), vec![], Some(&first.sources), read).unwrap().is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 1, "unchanged layers never read binary bytes");
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, b"new").unwrap();
    std::fs::rename(replacement, &source).unwrap();
    let changed = snapshot_layer(entries, vec![], Some(&first.sources), read).unwrap().unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(changed.files[0].2, b"new");
    assert_ne!(changed.sources, first.sources);
}

// #3792: a guest forward is how a crew reaches its daemon. Guest connections
// reach the host endpoint over this handle's own link; adding it again is a
// no-op, a reconnect claims the endpoint again and keeps the listener, and
// removing it closes the guest socket.
over_each_transport!(guest_forward_reaches_host_endpoint_across_reconnects);
async fn guest_forward_reaches_host_endpoint_across_reconnects(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::Builder::new().prefix("fgf-").tempdir_in("/tmp").unwrap();
    let runner = runner_for(&agent, directory.path());
    let endpoint = directory.path().join("daemon.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    let guest = directory.path().join("flotilla.sock");
    runner.add_guest_forward("flotilla-daemon", &guest, &endpoint).await.unwrap();
    runner.add_guest_forward("flotilla-daemon", &guest, &endpoint).await.unwrap();
    let other = directory.path().join("other.sock");
    let _other = UnixListener::bind(&other).unwrap();
    assert!(runner.add_guest_forward("flotilla-daemon", &guest, &other).await.is_err());
    for round in 0..2u8 {
        if round == 1 {
            // A dropped connection: the next use reconnects.
            assert!(runner.healthy().await);
            runner.state.lock().await.client = None;
            VesselAgent::ready(&runner).await.unwrap();
        }
        let mut crew = UnixStream::connect(&guest).await.unwrap();
        crew.write_all(&[round; 3]).await.unwrap();
        let (mut daemon, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
        let mut request = [0; 3];
        daemon.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [round; 3]);
        daemon.write_all(b"reply").await.unwrap();
        let mut reply = [0; 5];
        crew.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply");
    }
    runner.remove_guest_forward("flotilla-daemon").await.unwrap();
    assert!(UnixStream::connect(&guest).await.is_err(), "a removed forward leaves no guest listener");
    assert_eq!(agent.starts(), 1);
    // A closed handle never connects again (teardown, #3792).
    runner.close().await;
    assert!(runner.is_closed());
    assert!(VesselAgent::ready(&runner).await.unwrap_err().contains("closed"));
    assert!(runner.run("true", &[], Path::new("/"), &ChannelLabel::Default).await.is_err());
}

// A hello only verifies the prepared platform. A mismatch must stop before the
// upgrade route is used, even when the fingerprint also needs an update.
#[tokio::test]
async fn recorded_platform_mismatch_refuses_before_upload() {
    let current = Platform::current();
    let recorded = Platform::new("linux", if current == crate::guest_tools::linux_amd64() { "arm64" } else { "amd64" });
    let mut opens = 0;
    let error = connect_current(
        || {
            opens += 1;
            async {
                let (host, guest) = tokio::io::duplex(4096);
                let (host, guest) = tokio::join!(
                    Transport::yamux(Box::new(host), true, Default::default()),
                    Transport::yamux(Box::new(guest), false, Default::default())
                );
                tokio::spawn(drydock_agent::Agent::new(guest?, "older".into(), "/unused".into()).run());
                host
            }
        },
        &recorded,
        b"matching recorded build",
        Duration::from_millis(300),
    )
    .await
    .err()
    .expect("platform mismatch");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains(&format!("prepared for {recorded} but its agent reports {current}")));
    assert_eq!(opens, 1);
}

// Files, links and directories go through the agent's file operations, never
// a guest process. A replacement applies its new mode, readers see only whole
// files, and no staging entry is left beside the destination.
over_each_transport!(file_operations_publish_atomically_without_guest_processes);
async fn file_operations_publish_atomically_without_guest_processes(agent: Arc<dyn TestAgent>) {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    use std::os::unix::fs::PermissionsExt;
    let host = tempfile::tempdir().unwrap();
    let guest = tempfile::tempdir().unwrap();
    let runner = runner_for(&agent, host.path());
    let target = guest.path().join("material/nested/auth.json");
    runner.write_file_with_mode(&target, "first", 0o600).await.unwrap();
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
    runner.write_file_with_mode(&target, "second", 0o640).await.unwrap();
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read(&target).unwrap(), b"second");
    let source = host.path().join("source");
    std::fs::write(&source, vec![7; 300 * 1024]).unwrap();
    let copy = guest.path().join("material/copy");
    runner.write_file_from(&source, &copy).await.unwrap();
    let readback = host.path().join("readback");
    runner.read_file_to(&copy, &readback).await.unwrap();
    assert_eq!(std::fs::read(&readback).unwrap(), vec![7; 300 * 1024]);
    let config = guest.path().join("config/settings");
    assert_eq!(runner.ensure_file(&config, "kept").await.unwrap(), "kept");
    assert_eq!(runner.ensure_file(&config, "ignored").await.unwrap(), "kept");
    let tools = host.path().join("kit");
    std::fs::create_dir_all(tools.join("v1")).unwrap();
    std::fs::write(tools.join("v1/tool"), b"tool").unwrap();
    std::fs::set_permissions(tools.join("v1/tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("v1/tool", tools.join("entry")).unwrap();
    let installed = guest.path().join("opt");
    let layer = [EnvironmentTool::new("tool", installed.join("entry")).with_asset(EnvironmentToolAsset::new(
        &tools,
        &installed,
        EnvironmentToolAssetKind::Directory,
        EnvironmentToolAssetAccess::ReadOnly,
        "tool",
    ))];
    runner.push_tools(&layer).await.unwrap();
    assert_eq!(std::fs::read(installed.join("entry")).unwrap(), b"tool");
    assert_eq!(std::fs::metadata(installed.join("v1/tool")).unwrap().permissions().mode() & 0o777, 0o755);
    for directory in [guest.path().join("material"), guest.path().join("material/nested"), guest.path().join("config"), installed] {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(!name.to_string_lossy().starts_with('.'), "staging entry {name:?} left in {}", directory.display());
        }
    }
    let observer = agent.connect().await.unwrap();
    assert!(observer.processes().await.unwrap().is_empty(), "file operations must not run guest processes");
}

// An agent that restarts behind a surviving link comes back with no
// configuration and no forwards. Its configuration tag changes, so the next
// health check pushes the environment and every forward again.
over_each_transport!(health_check_restores_state_an_agent_lost);
async fn health_check_restores_state_an_agent_lost(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::Builder::new().prefix("frs-").tempdir_in("/tmp").unwrap();
    let runner = runner_for(&agent, directory.path());
    runner.configure_environment(BTreeMap::from([("VESSEL".into(), "restored".into())])).await.unwrap();
    let endpoint = directory.path().join("daemon.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    let guest = directory.path().join("flotilla.sock");
    runner.add_guest_forward("flotilla-daemon", &guest, &endpoint).await.unwrap();
    // Forwards are resources of their own: adding one leaves the configuration
    // tag alone, so a health check after it sees no restart.
    let pushed = runner.state.lock().await.etag.clone().unwrap();
    let live = runner.state.lock().await.client.clone().unwrap();
    assert_eq!(live.configuration().await.unwrap().etag, pushed);
    assert!(runner.healthy().await);
    assert_eq!(runner.state.lock().await.etag.clone().unwrap(), pushed, "an unchanged tag pushes nothing");
    // What a restart leaves behind the same link: empty configuration and no forwards.
    let client = runner.state.lock().await.client.clone().unwrap();
    client.remove_guest_forward("flotilla-daemon").await.unwrap();
    client.configure(Configuration::default()).await.unwrap();
    assert!(UnixStream::connect(&guest).await.is_err());
    assert!(runner.healthy().await);
    let echo = runner.run("sh", &["-c", "printf %s \"$VESSEL\""], Path::new("/"), &ChannelLabel::Default).await.unwrap();
    assert_eq!(echo, "restored");
    let mut crew = UnixStream::connect(&guest).await.unwrap();
    crew.write_all(b"ping").await.unwrap();
    let (mut daemon, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
    let mut request = [0; 4];
    daemon.read_exact(&mut request).await.unwrap();
    assert_eq!(&request, b"ping");
    assert_eq!(agent.starts(), 1);
}

// A refused guest forward releases the host endpoint it claimed, on the same
// client: correcting the request and retrying succeeds, and nothing is cached.
over_each_transport!(refused_guest_forward_releases_its_endpoint);
async fn refused_guest_forward_releases_its_endpoint(agent: Arc<dyn TestAgent>) {
    let directory = tempfile::Builder::new().prefix("frg-").tempdir_in("/tmp").unwrap();
    let runner = runner_for(&agent, directory.path());
    let endpoint = directory.path().join("daemon.sock");
    let _listener = UnixListener::bind(&endpoint).unwrap();
    let occupied = directory.path().join("occupied");
    std::fs::write(&occupied, b"not a socket").unwrap();
    assert!(runner.add_guest_forward("flotilla-daemon", &occupied, &endpoint).await.is_err());
    assert!(runner.state.lock().await.guest_forwards.is_empty());
    let guest = directory.path().join("flotilla.sock");
    runner.add_guest_forward("flotilla-daemon", &guest, &endpoint).await.unwrap();
    UnixStream::connect(&guest).await.unwrap();
    assert_eq!(agent.starts(), 1, "the refusal did not need a new client");
}
