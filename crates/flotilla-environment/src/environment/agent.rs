//! Host-owned vessel control. Only this module drives the vessel agent's
//! client. A layer update writes files and publishes links; it never activates
//! an agent upgrade.
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use client::{
    AgentClient, AgentLink, ArtifactSource, ByteStream, Configuration, Endpoint, Exec, ExecEvent, ExitStatus, Fetch, FileOptions,
    InwardForward, OutwardForward, Platform, Rename, Unpack,
};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::PinUnpack;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, OnceCell},
};

use super::{EntryKind, EnvironmentTool, VesselAgent};
use crate::composition::role;

pub mod client;

/// The actual process/transport seam. Tests use an in-memory agent; Docker
/// uses the private socket in its per-environment mounted dir.
#[async_trait]
pub trait AgentConnector: Send + Sync {
    async fn connect(&self) -> Result<Arc<dyn AgentClient>, String>;
}

pub struct UnixAgentConnector {
    pub socket: PathBuf,
    pub executable: PathBuf,
    pub timeout: Duration,
    link: Arc<dyn AgentLink>,
    platform: Platform,
    executable_bytes: Arc<OnceCell<Vec<u8>>>,
}
impl UnixAgentConnector {
    pub fn new(link: Arc<dyn AgentLink>, socket: PathBuf, executable: PathBuf, platform: Platform, timeout: Duration) -> Self {
        Self { socket, executable, timeout, link, platform, executable_bytes: Arc::new(OnceCell::new()) }
    }
    pub(crate) fn with_executable_cache(mut self, cache: Arc<OnceCell<Vec<u8>>>) -> Self {
        self.executable_bytes = cache;
        self
    }
}
#[async_trait]
impl AgentConnector for UnixAgentConnector {
    async fn connect(&self) -> Result<Arc<dyn AgentClient>, String> {
        let executable = self
            .executable_bytes
            .get_or_try_init(|| async {
                let bytes = tokio::fs::read(&self.executable)
                    .await
                    .map_err(|e| format!("read current drydock agent {}: {e}", self.executable.display()))?;
                crate::guest_tools::validate_native(&bytes, role::AGENT, &self.platform)?;
                Ok::<_, String>(bytes)
            })
            .await?;
        let mut open = || Box::pin(async { Ok(Box::new(UnixStream::connect(&self.socket).await?) as ByteStream) }) as _;
        self.link.connect(&mut open, &self.platform, executable, self.timeout).await.map_err(|e| e.to_string())
    }
}

struct Connection {
    client: Option<Arc<dyn AgentClient>>,
    configuration: Configuration,
    /// The tag the agent gave `configuration`. It changes when the agent
    /// restarts, even behind a link that survived the restart.
    etag: Option<String>,
    targets: BTreeMap<String, PathBuf>,
    /// Each outward forward with the host endpoint it reaches, pushed again
    /// with the configuration to every new client or restarted agent.
    outward_forwards: BTreeMap<String, (OutwardForward, Endpoint)>,
}
impl Connection {
    async fn push(&mut self, client: &dyn AgentClient) -> Result<(), String> {
        self.etag = Some(client.configure(self.configuration.clone()).await.map_err(|e| e.to_string())?);
        for (forward, endpoint) in self.outward_forwards.values() {
            client
                .put_outward_forward(forward.clone(), endpoint.clone())
                .await
                .map_err(|e| format!("outward forward {}: {e}", forward.id))?;
        }
        for (id, path) in &self.targets {
            client
                .put_inward_forward(InwardForward { id: id.clone(), endpoint: Endpoint::Unix(path.clone()) })
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// An outward forward the vessel agent did not establish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardFailure {
    pub id: String,
    pub guest: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for ForwardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outward forward {} at {}: {}", self.id, self.guest.display(), self.reason)
    }
}

impl std::error::Error for ForwardFailure {}

/// Files flotilla ensures are small configuration.
pub(crate) const ENSURED_FILE_LIMIT: u64 = 16 * 1024 * 1024;
/// Command output collected in memory, per stream. Generous, but finite.
pub(crate) const OUTPUT_LIMIT: usize = 256 * 1024 * 1024;
/// How long `healthy` waits to restore a restarted agent's configuration
/// and forwards before dropping the client for a fresh connection.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a vessel's writable state lives. A contained vessel owns its whole
/// filesystem; an agent on a shared host resolves the same base the host's
/// SSH runner does, so state written through either path stays in one place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WritableBases {
    #[default]
    Contained,
    Host,
}

/// One handle, one vessel. There is no interface taking a host destination from
/// a guest. Host endpoints registered with drydock stay exclusively claimed.
pub struct AgentRunner {
    connector: Arc<dyn AgentConnector>,
    state: Mutex<Connection>,
    forwards: Mutex<BTreeMap<PathBuf, ForwardListener>>,
    host_directory: PathBuf,
    layer: Mutex<Option<PublishedLayer>>,
    bases: WritableBases,
    closed: std::sync::atomic::AtomicBool,
}
struct PublishedLayer {
    sources: String,
    content: String,
}

// The content digest is cached by file identity, including replacement, length,
// mode and nanosecond change/modify times. Unchanged reconciles stat assets but
// never read or hash their bytes. Cache no binary payloads per vessel.
fn source_identity(entries: &[FileEntry], links: &[(PathBuf, PathBuf)]) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut identity = Sha256::new();
    for (source, destination, mode) in entries {
        let metadata = std::fs::symlink_metadata(source).map_err(|e| e.to_string())?;
        for path in [source, destination] {
            identity.update((path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(path.as_os_str().as_encoded_bytes());
        }
        identity.update(mode.to_le_bytes());
        for value in [
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ] {
            identity.update(value.to_le_bytes());
        }
    }
    for (target, destination) in links {
        for path in [target, destination] {
            identity.update((path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(path.as_os_str().as_encoded_bytes());
        }
    }
    Ok(format!("{:x}", identity.finalize()))
}

struct LayerSnapshot {
    files: Vec<(PathBuf, u32, Vec<u8>)>,
    links: Vec<(PathBuf, PathBuf)>,
    identity: Sha256,
    sources: String,
}
fn snapshot_layer(
    entries: Vec<FileEntry>,
    links: Vec<(PathBuf, PathBuf)>,
    previous_sources: Option<&String>,
    read: impl Fn(&Path) -> io::Result<Vec<u8>>,
) -> Result<Option<LayerSnapshot>, String> {
    let sources = source_identity(&entries, &links)?;
    if previous_sources == Some(&sources) {
        return Ok(None);
    }
    let mut identity = Sha256::new();
    let mut snapshot = Vec::new();
    for (source, destination, mode) in entries {
        let bytes = read(&source).map_err(|e| e.to_string())?;
        identity.update((destination.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
        identity.update(destination.as_os_str().as_encoded_bytes());
        identity.update(mode.to_le_bytes());
        identity.update((bytes.len() as u64).to_le_bytes());
        identity.update(&bytes);
        snapshot.push((destination, mode, bytes));
    }
    Ok(Some(LayerSnapshot { files: snapshot, links, identity, sources }))
}

struct ForwardListener {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
struct SocketCleanup(Option<PathBuf>);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
impl Drop for ForwardListener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
impl AgentRunner {
    pub fn new(connector: Arc<dyn AgentConnector>, host_directory: PathBuf) -> Self {
        Self {
            connector,
            state: Mutex::new(Connection {
                client: None,
                configuration: Configuration::default(),
                etag: None,
                targets: BTreeMap::new(),
                outward_forwards: BTreeMap::new(),
            }),
            forwards: Mutex::default(),
            host_directory,
            layer: Mutex::new(None),
            bases: WritableBases::Contained,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A runner whose vessel's sockets are carried by something else, as a
    /// drydock vessel's exposures are: it makes no attach forwards of its own.
    pub fn without_attach_forwards(connector: Arc<dyn AgentConnector>) -> Self {
        Self::new(connector, PathBuf::new())
    }

    pub fn with_writable_bases(mut self, bases: WritableBases) -> Self {
        self.bases = bases;
        self
    }

    async fn client(&self) -> Result<Arc<dyn AgentClient>, String> {
        let mut state = self.state.lock().await;
        if self.is_closed() {
            return Err("vessel agent handle is closed".into());
        }
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }
        let client = self.connector.connect().await?;
        state.push(client.as_ref()).await?;
        state.client = Some(client.clone());
        Ok(client)
    }

    /// Explicit health boundary, used by readiness and the supervisor watcher.
    /// Command/file operations use the cached client without a liveness RPC.
    /// A configuration tag this handle did not set means the agent restarted
    /// (or was reconfigured) behind a live link: it gets its state again.
    pub async fn healthy(&self) -> bool {
        let client = self.state.lock().await.client.clone();
        let Some(client) = client else { return false };
        if let Ok(Ok(current)) = tokio::time::timeout(Duration::from_secs(2), client.configuration_tag()).await {
            let mut state = self.state.lock().await;
            if !state.client.as_ref().is_some_and(|cached| Arc::ptr_eq(cached, &client)) || state.etag.as_ref() == Some(&current) {
                return true;
            }
            tracing::info!("vessel agent lost its configuration; pushing it again");
            // The state lock is held across the push, so a hung link must
            // not hold every other caller with it.
            if matches!(tokio::time::timeout(PUSH_TIMEOUT, state.push(client.as_ref())).await, Ok(Ok(()))) {
                return true;
            }
        }
        let mut state = self.state.lock().await;
        if state.client.as_ref().is_some_and(|cached| Arc::ptr_eq(cached, &client)) {
            state.client = None;
        }
        false
    }

    pub async fn configure_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        let client = self.client().await?;
        let mut state = self.state.lock().await;
        let mut config = state.configuration.clone();
        config.environment = environment;
        state.etag = Some(client.configure(config.clone()).await.map_err(|e| e.to_string())?);
        state.configuration = config;
        Ok(())
    }

    /// Add `environment` to what the vessel's commands run with, over what is
    /// already configured there.
    pub async fn extend_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        let client = self.client().await?;
        let mut state = self.state.lock().await;
        let mut config = state.configuration.clone();
        config.environment.extend(environment);
        state.etag = Some(client.configure(config.clone()).await.map_err(|e| e.to_string())?);
        state.configuration = config;
        Ok(())
    }

    /// A guest-side socket at `guest` whose connections reach the Unix socket
    /// `host` on the controller. The agent listens and announces each
    /// connection on its event stream; this handle's client claims it, so the
    /// forward serves nothing while no controller is connected. It survives
    /// reconnects, and its endpoint stays claimed by this vessel alone.
    pub async fn add_outward_forward(&self, id: &str, guest: &Path, host: &Path) -> Result<(), String> {
        let forward = OutwardForward { id: id.into(), unix: guest.into(), tcp: None };
        self.put_outward_forward(forward, Endpoint::Unix(host.into())).await.map_err(|failure| failure.to_string())
    }

    /// Register `forward` in the guest, reaching `host` on the controller, as
    /// [`Self::add_outward_forward`] does for a Unix socket. The guest may also
    /// listen on a loopback TCP address. A refusal, or no answer within the
    /// push timeout, is a failure naming the forward.
    pub async fn put_outward_forward(&self, forward: OutwardForward, host: Endpoint) -> Result<(), ForwardFailure> {
        let failed = |reason: String| ForwardFailure { id: forward.id.clone(), guest: forward.unix.clone(), reason };
        if !forward.unix.is_absolute() {
            return Err(failed("outward forward path must be absolute".into()));
        }
        let client = self.client().await.map_err(failed)?;
        let mut state = self.state.lock().await;
        match state.outward_forwards.get(&forward.id) {
            Some(existing) if *existing == (forward.clone(), host.clone()) => return Ok(()),
            Some(_) => return Err(failed("already serves another endpoint".into())),
            None => {}
        }
        // The cached client may have been replaced while this call waited.
        let current = state.client.clone().unwrap_or(client);
        // A refused upsert releases its endpoint claim.
        match tokio::time::timeout(PUSH_TIMEOUT, current.put_outward_forward(forward.clone(), host.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(failed(error.to_string())),
            Err(_) => return Err(failed(format!("no answer within {PUSH_TIMEOUT:?}"))),
        }
        state.outward_forwards.insert(forward.id.clone(), (forward, host));
        Ok(())
    }

    pub async fn remove_outward_forward(&self, id: &str) -> Result<(), String> {
        let client = self.client().await?;
        let mut state = self.state.lock().await;
        if !state.outward_forwards.contains_key(id) {
            return Ok(());
        }
        let current = state.client.clone().unwrap_or(client);
        match current.remove_outward_forward(id).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.to_string()),
            _ => {}
        }
        state.outward_forwards.remove(id);
        Ok(())
    }

    /// Drop this handle's connection. The next use connects again and pushes
    /// the configuration and forwards to the new client.
    pub async fn disconnect(&self) {
        self.state.lock().await.client = None;
    }

    /// Drop this handle's connection and its attach listeners for good.
    /// Teardown calls this before stopping the agent: a closed handle never
    /// connects again, so a caller still holding it cannot restart a retired
    /// vessel's agent.
    pub async fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.forwards.lock().await.clear();
        self.state.lock().await.client = None;
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn bytes(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<(Vec<u8>, Vec<u8>, ExitStatus), String> {
        let exec = Exec { cwd: Some(absolute_cwd(cwd)?), ..Exec::new(cmd, args.iter().copied()) };
        self.client().await?.run(exec, OUTPUT_LIMIT).await.map_err(|e| e.to_string())
    }

    async fn shell(&self, script: &str, args: &[&str]) -> Result<String, String> {
        let mut words = vec!["-c", script, "flotilla-agent"];
        words.extend_from_slice(args);
        self.run("sh", &words, Path::new("/"), &ChannelLabel::Default).await
    }

    fn file_options(path: &Path, mode: Option<u32>) -> Result<FileOptions, String> {
        if !path.is_absolute() {
            return Err("agent file path must be absolute".into());
        }
        if mode.is_some_and(|m| m > 0o777) {
            return Err("file mode must contain only permission bits".into());
        }
        // Without a mode, a new file takes the agent's private default (0600), so
        // a credential written through a runner is never world-readable.
        Ok(FileOptions { mode, parents: true, ..FileOptions::default() })
    }

    async fn publish(&self, path: &Path, bytes: Vec<u8>, mode: Option<u32>) -> Result<(), String> {
        let options = Self::file_options(path, mode)?;
        self.client().await?.write_bytes(path, bytes, &options).await.map_err(|e| e.to_string())
    }

    /// Push each regular asset through files/write, then replace generation
    /// symlinks. Never follow a link on the source host while traversing a tree.
    pub async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String> {
        // Snapshot once off the async executor: identity and publication use
        // exactly the same bytes even if a mutable source changes meanwhile.
        let mut layer = self.layer.lock().await;
        let previous_sources = layer.as_ref().map(|layer| layer.sources.clone());
        let tools = tools.to_vec();
        let snapshot = tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            let mut links = Vec::new();
            for tool in tools {
                for asset in tool.assets {
                    if !asset.requires_mount() {
                        collect(asset.host_path.as_path(), asset.environment_path.as_path(), &mut entries, &mut links)?;
                    }
                }
            }
            entries.sort();
            links.sort();
            snapshot_layer(entries, links, previous_sources.as_ref(), |path| std::fs::read(path))
        })
        .await
        .map_err(|e| e.to_string())??;
        let Some(LayerSnapshot { files: entries, mut links, mut identity, sources }) = snapshot else { return Ok(()) };
        for (target, destination) in &links {
            identity.update((target.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(target.as_os_str().as_encoded_bytes());
            identity.update((destination.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(destination.as_os_str().as_encoded_bytes());
        }
        let identity = format!("{:x}", identity.finalize());
        if layer.as_ref().is_some_and(|layer| layer.content == identity) {
            *layer = Some(PublishedLayer { sources, content: identity });
            return Ok(());
        }
        for (destination, mode, bytes) in entries {
            self.publish(&destination, bytes, Some(mode)).await?;
        }
        // Flip current last, after every file and every stable entry point exists.
        links.sort_by_key(|(_, destination)| destination.file_name().is_some_and(|n| n == "current"));
        let client = self.client().await?;
        let options = FileOptions { parents: true, ..FileOptions::default() };
        for (target, destination) in links {
            client.symlink(&destination, &target, &options).await.map_err(|e| e.to_string())?;
        }
        *layer = Some(PublishedLayer { sources, content: identity });
        Ok(())
    }
}
impl AgentRunner {
    async fn host_base(&self) -> Result<PathBuf, String> {
        let base = self
            .shell(flotilla_provider_runtime::ssh_runner::REMOTE_WRITABLE_BASE_SCRIPT, &[])
            .await
            .map_err(|error| format!("find writable directory on host: {error}"))?;
        let base = base.trim();
        if base.is_empty() {
            return Err("find writable directory on host: probe returned an empty path".into());
        }
        Ok(base.into())
    }
}

fn absolute_cwd(cwd: &Path) -> Result<PathBuf, String> {
    if !cwd.is_absolute() {
        return Err("agent working directory must be absolute".into());
    }
    Ok(cwd.into())
}

type FileEntry = (PathBuf, PathBuf, u32);
fn collect(source: &Path, destination: &Path, files: &mut Vec<FileEntry>, links: &mut Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(source).map_err(|e| format!("tool asset {}: {e}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(source).map_err(|e| e.to_string())?;
        // Fleet bin links intentionally use ../tools/current. These are trusted
        // host assets, copied as links without ever following them on the host.
        if target.is_absolute() {
            return Err("tool links must be relative".into());
        }
        links.push((target, destination.into()));
    } else if metadata.is_dir() {
        // Snapshot the selected immutable fleet generation once. Enumerating
        // every historical generation while current changes can miss the new
        // directory and then publish a dangling current link in the guest.
        if source.file_name().is_some_and(|name| name == "tools") {
            if let Ok(generation) = std::fs::read_link(source.join("current")) {
                let name = generation.to_str().ok_or("tool generation must be UTF-8")?;
                if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("tool generation must be a single content digest".into());
                }
                collect(&source.join(&generation), &destination.join(&generation), files, links)?;
                links.push((generation, destination.join("current")));
                return Ok(());
            }
        }
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            collect(&entry.path(), &destination.join(entry.file_name()), files, links)?;
        }
    } else if metadata.is_file() {
        files.push((source.into(), destination.into(), metadata.permissions().mode() & 0o777));
    } else {
        return Err("tool assets must be regular files, directories or relative links".into());
    }
    Ok(())
}

/// The store's unpack kind for a pin's. The store unpacks `tar` and `tar.gz` archives.
fn store_unpack(unpack: PinUnpack) -> Result<Unpack, String> {
    match unpack {
        PinUnpack::File => Ok(Unpack::File { mode: 0o555 }),
        PinUnpack::TarGz => Ok(Unpack::TarGz),
        PinUnpack::TarXz => Err("the vessel agent's store cannot unpack a tar.xz artifact".into()),
    }
}

#[async_trait]
impl VesselAgent for Arc<AgentRunner> {
    fn commands(&self) -> Arc<dyn CommandRunner> {
        self.clone()
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        CommandRunner::writable_config_base(self.as_ref(), None, Path::new(super::runner::CONTAINED_WRITABLE_CONFIG_BASE)).await
    }
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
        match self.client().await?.stat(path).await {
            Ok(kind) => Ok(Some(kind)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("stat {}: {error}", path.display())),
        }
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        // The agent applies exactly `mode`, keeps an existing directory's contents
        // and refuses anything else, a symbolic link included, at `path`.
        let options = FileOptions { mode: Some(mode), parents: true, ..FileOptions::default() };
        self.client().await?.create_dir(path, &options).await.map_err(|error| format!("create {}: {error}", path.display()))
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        match self.client().await?.read_bytes(path, limit).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        self.publish(path, bytes, Some(mode)).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        let options = FileOptions { parents: true, ..FileOptions::default() };
        self.client()
            .await?
            .symlink(path, target, &options)
            .await
            .map_err(|error| format!("link {} to {}: {error}", path.display(), target.display()))
    }
    async fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        let rename = Rename { from: from.to_path_buf(), to: to.to_path_buf(), exclusive: true, parents: false };
        self.client().await?.rename(&rename).await.map_err(|error| format!("rename {} to {}: {error}", from.display(), to.display()))
    }
    async fn architecture(&self) -> Result<String, String> {
        let platform = self.client().await?.platform();
        platform.map(|platform| platform.architecture).ok_or_else(|| "the vessel agent predates platform reporting".to_string())
    }
    async fn fetch_artifact(&self, sha256: &str, urls: &[String], unpack: PinUnpack) -> Result<(), String> {
        let sources = urls.iter().map(|url| ArtifactSource::Upstream { url: url.clone() }).collect();
        let fetch = Fetch { sources, unpack: store_unpack(unpack)? };
        self.client().await?.fetch_artifact(sha256, &fetch).await.map_err(|error| format!("fetch artifact {sha256}: {error}"))
    }
    async fn push_artifact(&self, sha256: &str, file: &Path, unpack: PinUnpack) -> Result<(), String> {
        let fetch = Fetch { sources: vec![ArtifactSource::Push], unpack: store_unpack(unpack)? };
        let content = tokio::fs::File::open(file).await.map_err(|error| format!("open {}: {error}", file.display()))?;
        let length = content.metadata().await.map_err(|error| format!("stat {}: {error}", file.display()))?.len();
        self.client()
            .await?
            .fetch_artifact_or_push(sha256, &fetch, length, content)
            .await
            .map_err(|error| format!("push artifact {sha256}: {error}"))
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        let options = FileOptions { parents: true, ..FileOptions::default() };
        self.client()
            .await?
            .link_artifact(path, sha256, member, &options)
            .await
            .map_err(|error| format!("link {} to artifact {sha256}: {error}", path.display()))
    }
    async fn ready(&self) -> Result<(), String> {
        self.healthy().await;
        self.client().await.map(|_| ())
    }
    async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String> {
        AgentRunner::push_tools(self, tools).await
    }
    async fn forward_controller_socket(&self, guest: &Path, tcp: Option<SocketAddr>, controller: &Path) -> Result<(), String> {
        let id = format!("socket-{}", &format!("{:x}", Sha256::digest(guest.as_os_str().as_encoded_bytes()))[..16]);
        let forward = OutwardForward { id, unix: guest.into(), tcp };
        self.put_outward_forward(forward, Endpoint::Unix(controller.into())).await.map_err(|failure| failure.to_string())
    }
    async fn extend_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        AgentRunner::extend_environment(self, environment).await
    }
    async fn forward_socket(&self, guest: &Path) -> Result<PathBuf, String> {
        if !guest.is_absolute() {
            return Err("guest socket path must be absolute".into());
        }
        let mut listeners = self.forwards.lock().await;
        if let Some(listener) = listeners.get(guest) {
            return Ok(listener.path.clone());
        }
        let id = format!("cleat-{:x}", Sha256::digest(guest.as_os_str().as_encoded_bytes()));
        let path = self.host_directory.join(format!("{}.sock", &id[..22]));
        // Never replace an unknown/live socket. The owning handle removes it.
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                return Err("attach forward path is occupied by a non-socket".into());
            }
            match UnixStream::connect(&path).await {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
                _ => return Err("attach forward path is already live or inaccessible".into()),
            }
        }
        let listener = UnixListener::bind(&path).map_err(|e| format!("bind attach forward {}: {e}", path.display()))?;
        let mut cleanup = SocketCleanup(Some(path.clone()));
        client::protect_socket(&path).map_err(|e| e.to_string())?;
        let client = self.client().await?;
        client
            .put_inward_forward(InwardForward { id: id.clone(), endpoint: Endpoint::Unix(guest.into()) })
            .await
            .map_err(|e| e.to_string())?;
        self.state.lock().await.targets.insert(id.clone(), guest.into());
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            while let Ok((mut viewer, _)) = listener.accept().await {
                let Some(agent) = weak.upgrade() else {
                    break;
                };
                let id = id.clone();
                tokio::spawn(async move {
                    if let Ok(client) = agent.client().await {
                        if let Ok(mut stream) = client.open_inward(&id).await {
                            // EOF in either direction drops both streams, so cleat
                            // observes viewer loss and releases its controller seat.
                            let (mut vr, mut vw) = tokio::io::split(&mut viewer);
                            let (mut gr, mut gw) = tokio::io::split(&mut stream);
                            tokio::select! { _ = tokio::io::copy(&mut vr, &mut gw) => {}, _ = tokio::io::copy(&mut gr, &mut vw) => {} }
                        }
                    }
                });
            }
        });
        cleanup.0 = None;
        listeners.insert(guest.into(), ForwardListener { path: path.clone(), task });
        Ok(path)
    }
}

#[async_trait]
impl CommandRunner for AgentRunner {
    async fn same_filesystem(&self, _: &Path, _: &Path) -> Result<bool, String> {
        Err("host clone filesystem comparison must run before container provisioning".into())
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        let (stdout, stderr, status) = self.bytes(cmd, args, cwd).await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: status.code,
        })
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.run(cmd, args, Path::new("/"), &ChannelLabel::Default).await.is_ok()
    }
    // A write without a mode takes the agent's private 0600 default. Readers
    // must run as the agent's uid (true of every vessel layout today: Docker
    // runs the container as one user, and crews start through the agent);
    // a reader under another uid needs `write_file_with_mode`.
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.publish(path, content.as_bytes().to_vec(), None).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.publish(path, content.as_bytes().to_vec(), Some(mode)).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let client = self.client().await?;
        let mut file = tokio::fs::File::create(destination).await.map_err(|e| e.to_string())?;
        client.read_to(source, &mut file).await.map(drop).map_err(|e| e.to_string())
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let options = Self::file_options(destination, None)?;
        let file = tokio::fs::File::open(source).await.map_err(|e| e.to_string())?;
        self.client().await?.write(destination, Box::new(file), &options).await.map_err(|e| e.to_string())
    }
    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let exec = Exec { cwd: Some(absolute_cwd(cwd)?), ..Exec::new(cmd, args.iter().copied()) };
        let mut stream = self.client().await?.exec(exec).await.map_err(|e| e.to_string())?;
        let mut file = tokio::fs::File::create(destination).await.map_err(|e| e.to_string())?;
        let mut stderr = Vec::new();
        while let Some(event) = stream.next().await.transpose().map_err(|e| e.to_string())? {
            match event {
                ExecEvent::Stdout(bytes) => file.write_all(&bytes).await.map_err(|e| e.to_string())?,
                ExecEvent::Stderr(bytes) => stderr.extend(bytes.into_iter().take((64 * 1024_usize).saturating_sub(stderr.len()))),
                ExecEvent::Exit(status) if status.success() => return file.flush().await.map_err(|e| e.to_string()),
                ExecEvent::Exit(status) => return Err(format!("{cmd} exited {:?}: {}", status.code, String::from_utf8_lossy(&stderr))),
            }
        }
        Err("agent file stream ended without exit status".into())
    }
    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        self.run_with_input(cmd, args, cwd, &ChannelLabel::Default, &tokio::fs::read(source).await.map_err(|e| e.to_string())?)
            .await
            .map(|_| ())
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let exec = Exec { cwd: Some(absolute_cwd(cwd)?), ..Exec::new(cmd, args.iter().copied()) };
        let (stdout, stderr, status) =
            self.client().await?.run_with_input(exec, input.to_vec(), OUTPUT_LIMIT).await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(String::from_utf8_lossy(&stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&stderr).into_owned())
        }
    }
    async fn writable_scratch_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        if self.bases == WritableBases::Host {
            return self.host_base().await;
        }
        let base = self
            .shell("if [ -n \"${HOME:-}\" ] && [ -d \"$HOME\" ] && [ -w \"$HOME\" ]; then printf '%s' \"$HOME\"; else printf /tmp; fi", &[])
            .await?;
        Ok(Path::new(base.trim()).join("flotilla"))
    }
    async fn writable_config_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        if self.bases == WritableBases::Host {
            return self.host_base().await;
        }
        Ok(super::runner::CONTAINED_WRITABLE_CONFIG_BASE.into())
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        let options = FileOptions { exclusive: true, ..Self::file_options(path, None)? };
        let client = self.client().await?;
        match client.write_bytes(path, content.as_bytes().to_vec(), &options).await {
            Ok(()) => Ok(content.into()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let bytes = client.read_bytes(path, ENSURED_FILE_LIMIT).await.map_err(|e| e.to_string())?;
                String::from_utf8(bytes).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
pub(crate) mod memory;
#[cfg(test)]
mod tests;
