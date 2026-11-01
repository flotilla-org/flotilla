//! Flotilla's view of the vessel agent's protocol: the operations a vessel
//! handle drives, in the agent's own terms. The composition root supplies the
//! implementation, drydock's client, through an [`AgentLink`].
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::{future::BoxFuture, stream::BoxStream};
pub use rigging_stream::ByteStream;
use tokio::io::{AsyncRead, AsyncWrite};

use super::super::EntryKind;
pub use crate::platform::Platform;

/// The agent's configuration. A configure replaces the whole document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Configuration {
    /// What every command the agent starts runs with.
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}
impl Exec {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { program: program.into(), args: args.into_iter().map(Into::into).collect(), ..Self::default() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
}
impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(ExitStatus),
}
pub type ExecEvents = BoxStream<'static, io::Result<ExecEvent>>;
/// A finished command's collected stdout and stderr, and how it ended.
pub type Output = (Vec<u8>, Vec<u8>, ExitStatus);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOptions {
    /// Permission bits of a created entry; without them the agent's private default.
    pub mode: Option<u32>,
    /// Create missing parent directories.
    pub parents: bool,
    /// Refuse to replace an existing entry.
    pub exclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rename {
    pub from: PathBuf,
    pub to: PathBuf,
    pub exclusive: bool,
    pub parents: bool,
}

/// Where the agent's content store gets an artifact, and how it unpacks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub sources: Vec<ArtifactSource>,
    pub unpack: Unpack,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactSource {
    Upstream {
        url: String,
    },
    /// The caller pushes the bytes.
    Push,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unpack {
    File { mode: u32 },
    TarGz,
}

/// A controller-side endpoint, or one in the vessel.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp(SocketAddr),
}
/// A listener in the vessel whose connections reach the host endpoint
/// registered with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutwardForward {
    pub id: String,
    pub unix: PathBuf,
    /// A loopback address the vessel also listens on.
    pub tcp: Option<SocketAddr>,
}
/// A target in the vessel the host may open byte streams to under its ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InwardForward {
    pub id: String,
    pub endpoint: Endpoint,
}

/// One live connection to a vessel's agent. Errors keep the agent's kind:
/// `NotFound` and `AlreadyExists` mean what they say about the path.
#[async_trait]
pub trait AgentClient: Send + Sync {
    /// The platform the agent's executable was built for; absent from agents
    /// that predate platform reporting.
    fn platform(&self) -> Option<Platform>;
    /// Replace the configuration, returning the tag the agent gave it.
    async fn configure(&self, configuration: Configuration) -> io::Result<String>;
    /// The current configuration's tag. It changes whenever the configuration
    /// does and whenever the agent restarts.
    async fn configuration_tag(&self) -> io::Result<String>;
    /// Run to completion, keeping at most `limit` bytes of each stream.
    async fn run(&self, exec: Exec, limit: usize) -> io::Result<Output>;
    /// Run with `input` on stdin, written while output is read. A process may
    /// exit without reading all of it.
    async fn run_with_input(&self, exec: Exec, input: Vec<u8>, limit: usize) -> io::Result<Output>;
    async fn exec(&self, exec: Exec) -> io::Result<ExecEvents>;
    async fn stat(&self, path: &Path) -> io::Result<EntryKind>;
    async fn create_dir(&self, path: &Path, options: &FileOptions) -> io::Result<()>;
    async fn read_bytes(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>>;
    /// Copy the file at `path` into `destination`.
    async fn read_to(&self, path: &Path, destination: &mut (dyn AsyncWrite + Unpin + Send)) -> io::Result<u64>;
    async fn write_bytes(&self, path: &Path, content: Vec<u8>, options: &FileOptions) -> io::Result<()>;
    async fn write(&self, path: &Path, content: Box<dyn AsyncRead + Unpin + Send>, options: &FileOptions) -> io::Result<()>;
    async fn symlink(&self, path: &Path, target: &Path, options: &FileOptions) -> io::Result<()>;
    async fn rename(&self, rename: &Rename) -> io::Result<()>;
    async fn fetch_artifact(&self, sha256: &str, fetch: &Fetch) -> io::Result<()>;
    /// Fetch, or receive `content` of `length` bytes when every source fails.
    /// An interrupted push resumes where the agent's upload stopped.
    async fn fetch_artifact_or_push(&self, sha256: &str, fetch: &Fetch, length: u64, content: tokio::fs::File) -> io::Result<()>;
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path, options: &FileOptions) -> io::Result<()>;
    /// Every link into a stored entry the store still counts as a use.
    async fn artifact_links(&self) -> io::Result<Vec<PathBuf>>;
    /// Register `forward` reaching `endpoint`. The endpoint stays claimed by
    /// this connection's vessel; a refused upsert releases it.
    async fn put_outward_forward(&self, forward: OutwardForward, endpoint: Endpoint) -> io::Result<()>;
    async fn remove_outward_forward(&self, id: &str) -> io::Result<()>;
    async fn put_inward_forward(&self, forward: InwardForward) -> io::Result<()>;
    async fn open_inward(&self, id: &str) -> io::Result<ByteStream>;
}

/// Opens one transport to the agent. Each connect attempt opens its own.
pub type Open<'a> = dyn FnMut() -> BoxFuture<'a, io::Result<ByteStream>> + Send + 'a;

/// How flotilla reaches vessel agents: the composition root supplies drydock's.
#[async_trait]
pub trait AgentLink: Send + Sync {
    /// A client of the agent `open` reaches, running `executable` (built for
    /// `platform`): an agent running another build is upgraded to it first.
    /// Transient failures are retried until `timeout`.
    async fn connect<'a>(
        &self,
        open: &mut Open<'a>,
        platform: &Platform,
        executable: &[u8],
        timeout: Duration,
    ) -> io::Result<Arc<dyn AgentClient>>;
    /// The tools image, pinned by digest, that vessel agents are installed from.
    fn tools_image(&self) -> Result<String, String>;
}

/// A dedicated run directory belongs to the agent's effective user. Refuse
/// symlinks and sticky or shared directories rather than changing their protection.
pub fn private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let result = (|| {
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: geteuid is a side-effect-free process identity query.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o1000 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "run directory must be an owned, non-sticky directory"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    })();
    result.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

/// Owner-only access to an agent's socket.
pub fn protect_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}
