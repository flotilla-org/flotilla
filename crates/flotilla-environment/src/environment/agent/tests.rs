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
        connect_current(|| self.open(), &self.current, Duration::from_secs(5)).await.map_err(|e| e.to_string())
    }
}
fn runner(agent: &Arc<MemoryAgent>, host: &Path) -> Arc<AgentRunner> {
    Arc::new(AgentRunner::new(agent.clone(), host.into()))
}

// Issue 3486: readiness requires the compatible hello, and a mismatched tools
// image upgrades by push/re-exec before any command uses the agent's API.
#[tokio::test]
async fn hello_upgrades_mismatched_agent_before_exec() {
    let agent = MemoryAgent::new(true);
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(&agent, directory.path());
    VesselAgent::ready(&runner).await.unwrap();
    assert_eq!(agent.upgrades.load(Ordering::SeqCst), 1);
    assert_eq!(agent.starts.load(Ordering::SeqCst), 2);
    let result =
        runner.run_output("sh", &["-c", "printf hello; printf problem >&2; exit 7"], Path::new("/"), &ChannelLabel::Default).await.unwrap();
    assert_eq!((result.stdout.as_str(), result.stderr.as_str(), result.exit_code), ("hello", "problem", Some(7)));
}

// Runner contract: arguments and working directories retain their values;
// stdout, stderr, nonzero exits, private writes, empty files, binary transfer,
// stdin and ensure-if-absent all cross the real agent rather than docker exec.
#[tokio::test]
async fn runner_and_material_contract() {
    use std::os::unix::fs::PermissionsExt;
    let agent = MemoryAgent::new(false);
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(&agent, directory.path());
    runner.configure_environment(BTreeMap::from([("FLOTILLA_AGENT_TEST".into(), "pushed environment".into())])).await.unwrap();
    let cwd = directory.path().join("work with spaces");
    std::fs::create_dir(&cwd).unwrap();
    let output = runner
        .run("sh", &["-c", "printf '%s|%s|%s' \"$PWD\" \"$1\" \"$FLOTILLA_AGENT_TEST\"", "test", "a' b\nc"], &cwd, &ChannelLabel::Default)
        .await
        .unwrap();
    assert_eq!(output, format!("{}|a' b\nc|pushed environment", cwd.display()));
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
    runner.write_file(&destination, "").await.unwrap();
    assert!(std::fs::read(&destination).unwrap().is_empty());
    assert!(runner.write_file_with_mode(&destination, "bad", 0o1000).await.is_err());
    assert!(runner.run("sh", &[], &cwd.join("missing"), &ChannelLabel::Default).await.is_err());
    assert!(runner.same_filesystem(&source, &destination).await.is_err());
}

// Issue 3486: flotilla's CLI cycle is separate from drydock's. Updating files
// changes what exec sees while the same agent incarnation continues running.
#[tokio::test]
async fn pushing_new_cli_layer_does_not_reexec_agent() {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    let agent = MemoryAgent::new(false);
    let host = tempfile::tempdir().unwrap();
    let guest = tempfile::tempdir().unwrap();
    let runner = runner(&agent, host.path());
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
        assert_eq!(agent.starts.load(Ordering::SeqCst), 1);
        assert_eq!(agent.upgrades.load(Ordering::SeqCst), 0);
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
#[tokio::test]
async fn attach_viewer_disconnect_releases_guest_seat() {
    let agent = MemoryAgent::new(false);
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(&agent, directory.path());
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
    assert_eq!(agent.starts.load(Ordering::SeqCst), 1);
}

// Empty/invalid current images and an unreachable initial transport must fail
// readiness, rather than reporting a container provisioned on Docker run alone.
#[tokio::test]
async fn missing_hello_is_bounded() {
    let error = connect_current(
        || async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "not started")) },
        b"current",
        Duration::from_millis(20),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
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
#[tokio::test]
async fn pushes_complete_generations_and_retains_old_cleat_files() {
    use super::super::{EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind};
    let memory = MemoryAgent::new(false);
    let host = tempfile::tempdir().unwrap();
    let guest = tempfile::tempdir().unwrap();
    let runner = runner(&memory, guest.path());
    let sources = host.path().join("source");
    std::fs::create_dir(&sources).unwrap();
    let cli = sources.join("flotilla");
    let cleat = sources.join("cleat");
    let library = sources.join("lib");
    let terminfo = sources.join("terminfo");
    std::fs::create_dir_all(terminfo.join("x")).unwrap();
    std::fs::write(terminfo.join("x/xterm-ghostty"), b"terminfo").unwrap();
    let fleet = host.path().join("fleet");
    let destination = guest.path().join("fleet");
    let mut tool = EnvironmentTool::new("fleet", destination.join("bin/flotilla"));
    for name in ["tools", "bin", "lib", "share/terminfo"] {
        tool = tool.with_asset(EnvironmentToolAsset::new(
            fleet.join(name),
            destination.join(name),
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
        let generation = crate::tools::publish(
            &fleet,
            &crate::tools::ToolsSource { flotilla: &cli, cleat: &cleat, library: &library, terminfo: &terminfo },
        )
        .unwrap();
        runner.push_tools(std::slice::from_ref(&tool)).await.unwrap();
        assert_eq!(std::fs::read(destination.join("bin/flotilla")).unwrap(), bytes);
        assert_eq!(std::fs::read(destination.join("bin/cleat")).unwrap(), bytes);
        assert_eq!(std::fs::read_link(destination.join("tools/current")).unwrap(), Path::new(generation.file_name().unwrap()));
        if let Some((old, bytes)) = previous {
            assert_eq!(std::fs::read(destination.join("tools").join(old).join("lib/libghostty-vt.so.0")).unwrap(), bytes);
        }
        previous = Some((generation.file_name().unwrap().to_os_string(), bytes.to_vec()));
    }
    assert_eq!(memory.starts.load(Ordering::SeqCst), 1);
    assert_eq!(memory.upgrades.load(Ordering::SeqCst), 0);
}

// Relative cwd never silently selects the agent's default directory, including
// the file-stream route. Default writes retain drydock's documented 0644 mode.
#[tokio::test]
async fn cwd_is_absolute_and_default_file_mode_is_explicit() {
    use std::os::unix::fs::PermissionsExt;
    let memory = MemoryAgent::new(false);
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(&memory, directory.path());
    for cwd in [".", "relative", "../parent", ""] {
        assert!(runner.run("pwd", &[], Path::new(cwd), &ChannelLabel::Default).await.unwrap_err().contains("must be absolute"));
        let output = directory.path().join("output");
        assert!(runner.run_to_file("pwd", &[], Path::new(cwd), &output).await.unwrap_err().contains("must be absolute"));
        assert!(!output.exists());
    }
    let file = directory.path().join("default-mode");
    runner.write_file(&file, "default").await.unwrap();
    assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o644);
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
    std::fs::write(&file, b"\x7fELFtest").unwrap();
    let connector = UnixAgentConnector::new(directory.path().join("absent.sock"), file.clone(), Duration::from_millis(20));
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
