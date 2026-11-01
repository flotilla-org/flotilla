//! An in-memory vessel agent: flotilla's agent client served by this process,
//! running commands and touching files on this machine as a vessel's agent
//! would in its vessel. Drydock's own agent is tested where its client is.
use super::client::*;
use super::{AgentConnector, EntryKind};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The digest-pinned tools image a [`MemoryLink`] names.
pub(crate) const TOOLS_IMAGE: &str = "registry.test/drydock-tools@sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Default)]
pub(crate) struct MemoryAgent(Arc<Agent>);

#[derive(Default)]
pub(crate) struct Agent {
    configuration: Mutex<(Configuration, usize)>,
    forwards: Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>,
    inwards: Mutex<BTreeMap<String, Endpoint>>,
    stopped: AtomicBool,
}

impl MemoryAgent {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }
    /// The agent stops answering, as a dead one does.
    pub(crate) fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
    }
    /// The agent restarts: it answers again, without its configuration.
    pub(crate) fn restart(&self) {
        let mut configuration = self.0.configuration.lock().expect("configuration");
        *configuration = (Configuration::default(), configuration.1 + 1);
        self.0.stopped.store(false, Ordering::SeqCst);
    }
    /// A connection to the agent, whether or not it answers.
    pub(crate) fn client(&self) -> Arc<dyn AgentClient> {
        self.0.clone()
    }
}

impl Agent {
    fn live(&self) -> io::Result<()> {
        match self.stopped.load(Ordering::SeqCst) {
            true => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "memory agent stopped")),
            false => Ok(()),
        }
    }
    fn command(&self, exec: &Exec) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&exec.program);
        command.args(&exec.args).current_dir(exec.cwd.as_deref().unwrap_or(Path::new("/")));
        command.envs(&self.configuration.lock().expect("configuration").0.environment);
        // Drydock's agent sets these pipeline defaults over its configuration.
        command.envs([
            ("CI", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("NO_COLOR", "1"),
            ("TERM", "dumb"),
            ("PAGER", "cat"),
            ("GIT_PAGER", "cat"),
            ("DEBIAN_FRONTEND", "noninteractive"),
        ]);
        command
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        for (_, task) in std::mem::take(&mut *self.forwards.lock().expect("forwards")) {
            task.abort();
        }
    }
}

#[async_trait]
impl AgentConnector for MemoryAgent {
    async fn connect(&self) -> Result<Arc<dyn AgentClient>, String> {
        self.0.live().map_err(|error| error.to_string())?;
        Ok(self.0.clone())
    }
}

/// Reaches its one agent whatever transport it is given.
pub(crate) struct MemoryLink(pub Arc<MemoryAgent>);

#[async_trait]
impl AgentLink for MemoryLink {
    async fn connect<'a>(&self, _: &mut Open<'a>, _: &Platform, _: &[u8], timeout: Duration) -> io::Result<Arc<dyn AgentClient>> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Ok(client) = AgentConnector::connect(self.0.as_ref()).await {
                    return client;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "agent hello: memory agent stopped"))
    }
    fn tools_image(&self) -> Result<String, String> {
        Ok(TOOLS_IMAGE.into())
    }
}

fn limited(mut bytes: Vec<u8>, limit: usize) -> Vec<u8> {
    bytes.truncate(limit);
    bytes
}

async fn parents(path: &Path, options: &FileOptions) -> io::Result<()> {
    match path.parent() {
        Some(parent) if options.parents => tokio::fs::create_dir_all(parent).await,
        _ => Ok(()),
    }
}

#[async_trait]
impl AgentClient for Agent {
    fn platform(&self) -> Option<Platform> {
        Some(Platform::current())
    }
    async fn configure(&self, configuration: Configuration) -> io::Result<String> {
        self.live()?;
        let mut current = self.configuration.lock().expect("configuration");
        *current = (configuration, current.1 + 1);
        Ok(current.1.to_string())
    }
    async fn configuration_tag(&self) -> io::Result<String> {
        self.live()?;
        Ok(self.configuration.lock().expect("configuration").1.to_string())
    }
    async fn run(&self, exec: Exec, limit: usize) -> io::Result<Output> {
        self.run_with_input(exec, Vec::new(), limit).await
    }
    async fn run_with_input(&self, exec: Exec, input: Vec<u8>, limit: usize) -> io::Result<Output> {
        self.live()?;
        let mut child = self.command(&exec).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let (written, output) = tokio::join!(
            async move {
                let written = stdin.write_all(&input).await;
                drop(stdin);
                written
            },
            child.wait_with_output()
        );
        match written {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
            _ => {
                let output = output?;
                Ok((limited(output.stdout, limit), limited(output.stderr, limit), ExitStatus { code: output.status.code() }))
            }
        }
    }
    async fn exec(&self, exec: Exec) -> io::Result<ExecEvents> {
        let (stdout, stderr, status) = self.run(exec, usize::MAX).await?;
        let events = [ExecEvent::Stdout(stdout), ExecEvent::Stderr(stderr), ExecEvent::Exit(status)];
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
    async fn stat(&self, path: &Path) -> io::Result<EntryKind> {
        self.live()?;
        let kind = tokio::fs::symlink_metadata(path).await?.file_type();
        Ok(if kind.is_symlink() {
            EntryKind::Symlink
        } else if kind.is_dir() {
            EntryKind::Directory
        } else if kind.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        })
    }
    async fn create_dir(&self, path: &Path, options: &FileOptions) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        self.live()?;
        parents(path, options).await?;
        match tokio::fs::create_dir(path).await {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && self.stat(path).await? == EntryKind::Directory => {}
            result => result?,
        }
        if let Some(mode) = options.mode {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
        }
        Ok(())
    }
    async fn read_bytes(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        self.live()?;
        let bytes = tokio::fs::read(path).await?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, format!("{} exceeds {limit} bytes", path.display())));
        }
        Ok(bytes)
    }
    async fn read_to(&self, path: &Path, destination: &mut (dyn tokio::io::AsyncWrite + Unpin + Send)) -> io::Result<u64> {
        self.live()?;
        let copied = tokio::io::copy(&mut tokio::fs::File::open(path).await?, destination).await?;
        destination.flush().await?;
        Ok(copied)
    }
    async fn write_bytes(&self, path: &Path, content: Vec<u8>, options: &FileOptions) -> io::Result<()> {
        self.write(path, Box::new(std::io::Cursor::new(content)), options).await
    }
    async fn write(&self, path: &Path, mut content: Box<dyn tokio::io::AsyncRead + Unpin + Send>, options: &FileOptions) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        self.live()?;
        parents(path, options).await?;
        if options.exclusive && tokio::fs::symlink_metadata(path).await.is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists", path.display())));
        }
        let mut bytes = Vec::new();
        content.read_to_end(&mut bytes).await?;
        let staged = path.with_file_name(format!(".{}.{}", path.file_name().unwrap_or_default().to_string_lossy(), uuid::Uuid::new_v4()));
        tokio::fs::write(&staged, bytes).await?;
        tokio::fs::set_permissions(&staged, std::fs::Permissions::from_mode(options.mode.unwrap_or(0o600))).await?;
        tokio::fs::rename(&staged, path).await
    }
    async fn symlink(&self, path: &Path, target: &Path, options: &FileOptions) -> io::Result<()> {
        self.live()?;
        parents(path, options).await?;
        let staged = path.with_file_name(format!(".{}.{}", path.file_name().unwrap_or_default().to_string_lossy(), uuid::Uuid::new_v4()));
        tokio::fs::symlink(target, &staged).await?;
        tokio::fs::rename(&staged, path).await
    }
    async fn rename(&self, rename: &Rename) -> io::Result<()> {
        self.live()?;
        if rename.exclusive && tokio::fs::symlink_metadata(&rename.to).await.is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists", rename.to.display())));
        }
        tokio::fs::rename(&rename.from, &rename.to).await
    }
    async fn fetch_artifact(&self, _: &str, _: &Fetch) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the memory agent keeps no artifacts"))
    }
    async fn fetch_artifact_or_push(&self, _: &str, _: &Fetch, _: u64, _: tokio::fs::File) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the memory agent keeps no artifacts"))
    }
    async fn link_artifact(&self, _: &Path, _: &str, _: &Path, _: &FileOptions) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the memory agent keeps no artifacts"))
    }
    async fn put_outward_forward(&self, forward: OutwardForward, endpoint: Endpoint) -> io::Result<()> {
        self.live()?;
        let Endpoint::Unix(host) = endpoint else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "the memory agent reaches Unix endpoints only"));
        };
        let _ = std::fs::remove_file(&forward.unix);
        let listener = tokio::net::UnixListener::bind(&forward.unix)?;
        let task = tokio::spawn(async move {
            while let Ok((mut guest, _)) = listener.accept().await {
                let host = host.clone();
                tokio::spawn(async move {
                    if let Ok(mut host) = tokio::net::UnixStream::connect(&host).await {
                        let _ = tokio::io::copy_bidirectional(&mut guest, &mut host).await;
                    }
                });
            }
        });
        if let Some(previous) = self.forwards.lock().expect("forwards").insert(forward.id, task) {
            previous.abort();
        }
        Ok(())
    }
    async fn remove_outward_forward(&self, id: &str) -> io::Result<()> {
        self.live()?;
        match self.forwards.lock().expect("forwards").remove(id) {
            Some(task) => {
                task.abort();
                Ok(())
            }
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("outward forward {id}"))),
        }
    }
    async fn put_inward_forward(&self, forward: InwardForward) -> io::Result<()> {
        self.live()?;
        self.inwards.lock().expect("inwards").insert(forward.id, forward.endpoint);
        Ok(())
    }
    async fn open_inward(&self, id: &str) -> io::Result<ByteStream> {
        self.live()?;
        let endpoint = self.inwards.lock().expect("inwards").get(id).cloned();
        match endpoint {
            Some(Endpoint::Unix(path)) => Ok(Box::new(tokio::net::UnixStream::connect(path).await?)),
            Some(Endpoint::Tcp(address)) => Ok(Box::new(tokio::net::TcpStream::connect(address).await?)),
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("inward forward {id}"))),
        }
    }
}
