//! Host-owned vessel control. Only this module knows drydock's client. A layer
//! update writes files and publishes links; it never activates an agent upgrade.
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use drydock_agent::{Client, Configuration, Endpoint, Exec, ExecEvent};
use drydock_transport::{Limits, Transport};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use sha2::{Digest, Sha256};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, OnceCell},
};

use super::{EnvironmentTool, VesselAgent};

/// The actual process/transport seam. Tests use drydock's real Agent over duplex
/// streams; Docker uses the private socket in its per-environment mounted dir.
#[async_trait]
pub trait AgentConnector: Send + Sync {
    async fn connect(&self) -> Result<Client, String>;
}

pub struct UnixAgentConnector {
    pub socket: PathBuf,
    pub executable: PathBuf,
    pub timeout: Duration,
    executable_bytes: Arc<OnceCell<Vec<u8>>>,
}
impl UnixAgentConnector {
    pub fn new(socket: PathBuf, executable: PathBuf, timeout: Duration) -> Self {
        Self { socket, executable, timeout, executable_bytes: Arc::new(OnceCell::new()) }
    }
    pub(crate) fn with_executable_cache(mut self, cache: Arc<OnceCell<Vec<u8>>>) -> Self {
        self.executable_bytes = cache;
        self
    }
}
#[async_trait]
impl AgentConnector for UnixAgentConnector {
    async fn connect(&self) -> Result<Client, String> {
        let executable = self
            .executable_bytes
            .get_or_try_init(|| async {
                let bytes = tokio::fs::read(&self.executable)
                    .await
                    .map_err(|e| format!("read current drydock agent {}: {e}", self.executable.display()))?;
                if !bytes.starts_with(b"\x7fELF") {
                    return Err("current drydock agent must be its Linux executable, not a host launcher script".into());
                }
                Ok::<_, String>(bytes)
            })
            .await?;
        connect_current(
            || async {
                let stream = UnixStream::connect(&self.socket).await?;
                Transport::yamux(Box::new(stream), true, Limits::default()).await
            },
            executable,
            self.timeout,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// Initial socket/hello failures are normal while Docker starts the detached
/// supervisor. Drydock itself handles mismatches and post-upgrade reconnects.
pub async fn connect_current<F, Fut>(mut open: F, executable: &[u8], timeout: Duration) -> io::Result<Client>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<Transport>>,
{
    if executable.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "current agent executable is empty"));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = "no hello".to_string();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("agent hello: {last}")));
        }
        match Client::connect_current(&mut open, executable, remaining).await {
            Ok(client) => return Ok(client),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(50).min(remaining)).await;
    }
}

struct Connection {
    client: Option<Arc<Client>>,
    configuration: Configuration,
    targets: BTreeMap<String, PathBuf>,
}

/// One handle, one vessel. There is no interface taking a host destination from
/// a guest. Host endpoints registered with drydock stay exclusively claimed.
pub struct AgentRunner {
    connector: Arc<dyn AgentConnector>,
    state: Mutex<Connection>,
    forwards: Mutex<BTreeMap<PathBuf, ForwardListener>>,
    host_directory: PathBuf,
    layer: Mutex<Option<String>>,
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
            state: Mutex::new(Connection { client: None, configuration: Configuration::default(), targets: BTreeMap::new() }),
            forwards: Mutex::default(),
            host_directory,
            layer: Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<Arc<Client>, String> {
        let mut state = self.state.lock().await;
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }
        let client = Arc::new(self.connector.connect().await?);
        client.configure(state.configuration.clone()).await.map_err(|e| e.to_string())?;
        for (id, path) in &state.targets {
            client.add_host_forward(id.clone(), Endpoint::Unix(path.clone())).await.map_err(|e| e.to_string())?;
        }
        state.client = Some(client.clone());
        Ok(client)
    }

    /// Explicit health boundary, used by readiness and the supervisor watcher.
    /// Command/file operations use the cached client without a liveness RPC.
    pub async fn healthy(&self) -> bool {
        let client = self.state.lock().await.client.clone();
        let Some(client) = client else { return false };
        if tokio::time::timeout(Duration::from_secs(2), client.configuration()).await.is_ok_and(|result| result.is_ok()) {
            return true;
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
        client.configure(config.clone()).await.map_err(|e| e.to_string())?;
        state.configuration = config;
        Ok(())
    }

    async fn bytes(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<(Vec<u8>, Vec<u8>, Option<i32>), String> {
        let mut stream = self
            .client()
            .await?
            .exec(Exec { program: cmd.into(), args: args.iter().map(|s| (*s).into()).collect(), cwd: Some(absolute_cwd(cwd)?) })
            .await
            .map_err(|e| e.to_string())?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        while let Some(event) = stream.next().await.map_err(|e| e.to_string())? {
            match event {
                ExecEvent::Stdout(bytes) => stdout.extend(bytes),
                ExecEvent::Stderr(bytes) => stderr.extend(bytes),
                ExecEvent::Exit { code } => return Ok((stdout, stderr, code)),
            }
        }
        Err("agent exec ended without an exit event".into())
    }

    async fn shell(&self, script: &str, args: &[&str]) -> Result<String, String> {
        let mut words = vec!["-c", script, "flotilla-agent"];
        words.extend_from_slice(args);
        self.run("sh", &words, Path::new("/"), &ChannelLabel::Default).await
    }

    // A unique private directory owns all intermediates, including a stdin
    // spool. Cancellation drops the guard and schedules cleanup without secrets
    // in argv. The final name is replaced only after its mode has been applied.
    async fn staging(&self, parent: &Path) -> Result<RemoteStaging, String> {
        let path = parent.join(format!(".flotilla-{}", uuid::Uuid::new_v4()));
        self.shell("mkdir -p -- \"$1\"; umask 077; mkdir -- \"$2\"", &[&parent.to_string_lossy(), &path.to_string_lossy()]).await?;
        Ok(RemoteStaging { client: self.client().await?, path })
    }

    async fn publish(&self, path: &Path, bytes: Vec<u8>, mode: Option<u32>) -> Result<(), String> {
        if !path.is_absolute() {
            return Err("agent file path must be absolute".into());
        }
        if mode.is_some_and(|m| m > 0o777) {
            return Err("file mode must contain only permission bits".into());
        }
        let staging = self.staging(path.parent().ok_or("file path requires a parent")?).await?;
        let temporary = staging.path.join("file");
        staging.client.write(temporary.clone(), bytes, false).await.map_err(|e| e.to_string())?;
        let mode = mode.map(|m| format!("{m:o}"));
        self.shell(
            "if [ -n \"$1\" ]; then chmod \"$1\" \"$2\" || exit; fi; mv -f -- \"$2\" \"$3\"",
            &[mode.as_deref().unwrap_or(""), &temporary.to_string_lossy(), &path.to_string_lossy()],
        )
        .await?;
        Ok(())
    }

    /// Push each regular asset through files/write, then replace generation
    /// symlinks. Never follow a link on the source host while traversing a tree.
    pub async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String> {
        // Snapshot once off the async executor: identity and publication use
        // exactly the same bytes even if a mutable source changes meanwhile.
        let tools = tools.to_vec();
        let (entries, mut links, mut identity) = tokio::task::spawn_blocking(move || {
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
            let mut identity = Sha256::new();
            let mut snapshot = Vec::new();
            for (source, destination, mode) in entries {
                let bytes = std::fs::read(source).map_err(|e| e.to_string())?;
                identity.update((destination.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
                identity.update(destination.as_os_str().as_encoded_bytes());
                identity.update(mode.to_le_bytes());
                identity.update((bytes.len() as u64).to_le_bytes());
                identity.update(&bytes);
                snapshot.push((destination, mode, bytes));
            }
            Ok::<_, String>((snapshot, links, identity))
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut layer = self.layer.lock().await;
        for (target, destination) in &links {
            identity.update((target.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(target.as_os_str().as_encoded_bytes());
            identity.update((destination.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            identity.update(destination.as_os_str().as_encoded_bytes());
        }
        let identity = format!("{:x}", identity.finalize());
        if layer.as_ref() == Some(&identity) {
            return Ok(());
        }
        for (destination, mode, bytes) in entries {
            self.publish(&destination, bytes, Some(mode)).await?;
        }
        // Flip current last, after every file and every stable entry point exists.
        links.sort_by_key(|(_, destination)| destination.file_name().is_some_and(|n| n == "current"));
        for (target, destination) in links {
            let staging = self.staging(destination.parent().ok_or("link requires parent")?).await?;
            let temporary = staging.path.join("link");
            self.shell(
                "ln -s -- \"$1\" \"$2\" && mv -Tf -- \"$2\" \"$3\"",
                &[&target.to_string_lossy(), &temporary.to_string_lossy(), &destination.to_string_lossy()],
            )
            .await?;
        }
        *layer = Some(identity);
        Ok(())
    }
}
struct RemoteStaging {
    client: Arc<Client>,
    path: PathBuf,
}
impl Drop for RemoteStaging {
    fn drop(&mut self) {
        let client = self.client.clone();
        let path = self.path.to_string_lossy().into_owned();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        runtime.spawn(async move {
            if let Ok(mut output) = client.exec(Exec { program: "rm".into(), args: vec!["-rf".into(), "--".into(), path], cwd: None }).await
            {
                while output.next().await.is_ok_and(|event| event.is_some()) {}
            }
        });
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

#[async_trait]
impl VesselAgent for Arc<AgentRunner> {
    async fn ready(&self) -> Result<(), String> {
        self.healthy().await;
        self.client().await.map(|_| ())
    }
    async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String> {
        AgentRunner::push_tools(self, tools).await
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
        drydock_agent::socket::protect(&path).map_err(|e| e.to_string())?;
        let client = self.client().await?;
        client.add_host_forward(id.clone(), Endpoint::Unix(guest.into())).await.map_err(|e| e.to_string())?;
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
                        if let Ok(mut stream) = client.forward(&id).await {
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
        let (stdout, stderr, exit_code) = self.bytes(cmd, args, cwd).await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code,
        })
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.run(cmd, args, Path::new("/"), &ChannelLabel::Default).await.is_ok()
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.publish(path, content.as_bytes().to_vec(), None).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.publish(path, content.as_bytes().to_vec(), Some(mode)).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let bytes = self.client().await?.read(source.into()).await.map_err(|e| e.to_string())?;
        tokio::fs::write(destination, bytes).await.map_err(|e| e.to_string())
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.publish(destination, tokio::fs::read(source).await.map_err(|e| e.to_string())?, None).await
    }
    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let client = self.client().await?;
        let mut stream = client
            .exec(Exec { program: cmd.into(), args: args.iter().map(|s| (*s).into()).collect(), cwd: Some(absolute_cwd(cwd)?) })
            .await
            .map_err(|e| e.to_string())?;
        let mut file = tokio::fs::File::create(destination).await.map_err(|e| e.to_string())?;
        let mut stderr = Vec::new();
        while let Some(event) = stream.next().await.map_err(|e| e.to_string())? {
            match event {
                ExecEvent::Stdout(bytes) => file.write_all(&bytes).await.map_err(|e| e.to_string())?,
                ExecEvent::Stderr(bytes) => stderr.extend(bytes.into_iter().take((64 * 1024_usize).saturating_sub(stderr.len()))),
                ExecEvent::Exit { code: Some(0) } => return file.flush().await.map_err(|e| e.to_string()),
                ExecEvent::Exit { code } => return Err(format!("{cmd} exited {code:?}: {}", String::from_utf8_lossy(&stderr))),
            }
        }
        Err("agent file stream ended without exit status".into())
    }
    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        self.run_with_input(cmd, args, cwd, &ChannelLabel::Default, &tokio::fs::read(source).await.map_err(|e| e.to_string())?)
            .await
            .map(|_| ())
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let staging = self.staging(Path::new("/tmp")).await?;
        let file = staging.path.join("stdin");
        staging.client.write(file.clone(), input.to_vec(), false).await.map_err(|e| e.to_string())?;
        let file = file.to_string_lossy();
        let mut words = vec!["-c", "input=$1; shift; exec \"$@\" < \"$input\"", "flotilla-stdin", &file, cmd];
        words.extend_from_slice(args);
        self.run("sh", &words, cwd, label).await
    }
    async fn writable_scratch_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        let base = self
            .shell("if [ -n \"${HOME:-}\" ] && [ -d \"$HOME\" ] && [ -w \"$HOME\" ]; then printf '%s' \"$HOME\"; else printf /tmp; fi", &[])
            .await?;
        Ok(Path::new(base.trim()).join("flotilla"))
    }
    async fn writable_config_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        Ok(super::runner::CONTAINED_WRITABLE_CONFIG_BASE.into())
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        let staging = self.staging(path.parent().ok_or("file path requires parent")?).await?;
        let file = staging.path.join("file");
        staging.client.write(file.clone(), content.as_bytes().to_vec(), false).await.map_err(|e| e.to_string())?;
        self.shell("ln -- \"$1\" \"$2\" 2>/dev/null || test -f \"$2\"", &[&file.to_string_lossy(), &path.to_string_lossy()]).await?;
        String::from_utf8(self.client().await?.read(path.into()).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub(crate) mod tests;
