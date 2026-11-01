//! Drydock's vessel agent client behind flotilla's [`AgentClient`], and the
//! [`AgentLink`] that connects it.
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_environment::environment::{
    agent::client::{self as flotilla, AgentClient, AgentLink, ByteStream, Open, Output},
    EntryKind,
};

/// Drydock's own link to vessel agents.
pub struct DrydockLink;

/// The link the composition root hands to every environment provider.
pub fn link() -> Arc<dyn AgentLink> {
    Arc::new(DrydockLink)
}

#[async_trait]
impl AgentLink for DrydockLink {
    async fn connect<'a>(
        &self,
        open: &mut Open<'a>,
        platform: &flotilla::Platform,
        executable: &[u8],
        timeout: Duration,
    ) -> io::Result<Arc<dyn AgentClient>> {
        let client = connect_current(transports(open), &platform_of(platform), executable, timeout).await?;
        Ok(Arc::new(DrydockClient(client)))
    }
    fn tools_image(&self) -> Result<String, String> {
        drydock::launch::build_tools_digest().map(str::to_owned).map_err(|error| error.to_string())
    }
}

pub(crate) fn platform_of(platform: &flotilla::Platform) -> drydock_protocol::Platform {
    drydock_protocol::Platform::new(platform.os.clone(), platform.architecture.clone())
}

/// First pause between connect attempts, doubled up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Whether trying again could succeed. A socket that is not listening yet or
/// a hello that has not arrived are transient; a refusal from the agent that
/// states the request itself cannot be accepted (an upload already in
/// progress, too large, no space) is the same on every attempt.
fn is_transient(error: &io::Error) -> bool {
    use drydock_agent::{AgentError, ErrorKind};
    if let Some(refusal) = AgentError::of(error) {
        // No wildcard: a kind drydock adds fails to compile here at the repin, so someone
        // decides whether retrying it can help instead of it defaulting to "retry".
        return match refusal.kind {
            ErrorKind::Conflict
            | ErrorKind::AlreadyExists
            | ErrorKind::FileTooLarge
            | ErrorKind::PayloadTooLarge
            | ErrorKind::StorageFull
            | ErrorKind::QuotaExceeded
            | ErrorKind::PermissionDenied
            | ErrorKind::InvalidData
            | ErrorKind::InvalidInput
            | ErrorKind::Unsupported => false,
            // Unchanged from the former hand list: kinds that may in fact be permanent refusals
            // (InvalidFilename, DigestMismatch, ReadOnlyFilesystem, ...) still retry; see #4257.
            ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::AddrInUse
            | ErrorKind::AddrNotAvailable
            | ErrorKind::NetworkDown
            | ErrorKind::BrokenPipe
            | ErrorKind::WouldBlock
            | ErrorKind::NotADirectory
            | ErrorKind::IsADirectory
            | ErrorKind::DirectoryNotEmpty
            | ErrorKind::ReadOnlyFilesystem
            | ErrorKind::TimedOut
            | ErrorKind::WriteZero
            | ErrorKind::NotSeekable
            | ErrorKind::ResourceBusy
            | ErrorKind::ExecutableFileBusy
            | ErrorKind::Deadlock
            | ErrorKind::CrossesDevices
            | ErrorKind::TooManyLinks
            | ErrorKind::InvalidFilename
            | ErrorKind::ArgumentListTooLong
            | ErrorKind::Interrupted
            | ErrorKind::UnexpectedEof
            | ErrorKind::OutOfMemory
            | ErrorKind::DigestMismatch
            | ErrorKind::PreconditionFailed
            | ErrorKind::RangeNotSatisfiable
            | ErrorKind::UnsupportedMediaType
            | ErrorKind::MethodNotAllowed
            | ErrorKind::Other => true,
        };
    }
    // A plain PermissionDenied is not listed: an ssh-level denial can clear
    // while the host is still being provisioned. The agent's own refusal is final.
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::Unsupported
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::FileTooLarge
            | io::ErrorKind::StorageFull
            | io::ErrorKind::QuotaExceeded
    )
}

/// The reason a vessel's artifact cannot land because its content store is
/// full (#4087). Drydock's refusal names the store's path and free bytes.
pub const CONTENT_STORE_FULL: &str = "ContentStoreFull";

/// Name a full content store's refusal, which no retry can change.
fn store_full(error: io::Error) -> io::Error {
    match drydock_agent::AgentError::of(&error) {
        Some(refusal) if matches!(refusal.kind, drydock_agent::ErrorKind::StorageFull | drydock_agent::ErrorKind::QuotaExceeded) => {
            io::Error::new(error.kind(), format!("{CONTENT_STORE_FULL}: {refusal}"))
        }
        _ => error,
    }
}

/// Initial socket/hello failures are normal while a provider starts the
/// detached supervisor. Drydock itself handles mismatches and post-upgrade
/// reconnects. `platform` is the one the vessel's `executable` was built for.
///
/// Transient failures are retried with exponential backoff until `timeout`;
/// a failure that cannot change (see [`is_transient`]) is returned at once.
/// When the deadline passes, the error is the last one an attempt reported,
/// not the timeout that cut the final attempt short. Each attempt opens the
/// transports drydock needs: a transport is consumed by its hello and an
/// upgrade restarts the agent, so none can be shared between attempts.
pub async fn connect_current<F, Fut>(
    mut open: F,
    platform: &drydock_protocol::Platform,
    executable: &[u8],
    timeout: Duration,
) -> io::Result<drydock_agent::Client>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<drydock_transport::ByteStream>>,
{
    if executable.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "current agent executable is empty"));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last: Option<io::Error> = None;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(deadline_error(last));
        }
        match drydock_agent::Client::connect_current(&mut open, platform, executable, remaining).await {
            Ok(client) => return Ok(client),
            Err(error) if !is_transient(&error) => return Err(error),
            Err(error) => {
                // The client's own timeout, once the deadline has passed, is
                // the deadline cutting the attempt short; it says less than
                // what failed before. A transport timeout earlier is real.
                let cut = error.kind() == io::ErrorKind::TimedOut
                    && drydock_agent::AgentError::of(&error).is_none()
                    && tokio::time::Instant::now() >= deadline;
                if !cut || last.is_none() {
                    last = Some(error);
                }
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(backoff.min(remaining)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// `open` as the transport factory drydock's client takes.
fn transports<'o, 'a>(
    open: &'o mut Open<'a>,
) -> impl FnMut() -> futures::future::BoxFuture<'a, io::Result<drydock_transport::ByteStream>> + 'o {
    move || {
        let opened = open();
        Box::pin(async move { opened.await.map(|stream| Box::new(stream) as drydock_transport::ByteStream) })
    }
}

fn deadline_error(last: Option<io::Error>) -> io::Error {
    match last {
        Some(error) => io::Error::new(io::ErrorKind::TimedOut, format!("agent hello: {error}")),
        None => io::Error::new(io::ErrorKind::TimedOut, "agent hello: no hello"),
    }
}

/// One connection to a vessel's drydock agent.
pub struct DrydockClient(pub drydock_agent::Client);

fn exec(exec: flotilla::Exec) -> drydock_agent::Exec {
    drydock_agent::Exec { cwd: exec.cwd, ..drydock_agent::Exec::new(exec.program, exec.args) }
}
fn status(status: drydock_agent::ExitStatus) -> flotilla::ExitStatus {
    flotilla::ExitStatus { code: status.code }
}
fn options(options: &flotilla::FileOptions) -> drydock_agent::FileOptions {
    drydock_agent::FileOptions { mode: options.mode, parents: options.parents, exclusive: options.exclusive, ..Default::default() }
}
fn fetch(fetch: &flotilla::Fetch) -> drydock_agent::Fetch {
    drydock_agent::Fetch {
        sources: fetch
            .sources
            .iter()
            .map(|source| match source {
                flotilla::ArtifactSource::Upstream { url } => drydock_agent::ArtifactSource::Upstream { url: url.clone() },
                flotilla::ArtifactSource::Push => drydock_agent::ArtifactSource::Push,
            })
            .collect(),
        unpack: match fetch.unpack {
            flotilla::Unpack::File { mode } => drydock_agent::Unpack::File { mode },
            flotilla::Unpack::TarGz => drydock_agent::Unpack::TarGz,
        },
    }
}
fn endpoint(endpoint: flotilla::Endpoint) -> drydock_agent::Endpoint {
    match endpoint {
        flotilla::Endpoint::Unix(path) => drydock_agent::Endpoint::Unix(path),
        flotilla::Endpoint::Tcp(address) => drydock_agent::Endpoint::Tcp(address),
    }
}
fn collected((stdout, stderr, exit): (Vec<u8>, Vec<u8>, drydock_agent::ExitStatus)) -> Output {
    (stdout, stderr, status(exit))
}

#[async_trait]
impl AgentClient for DrydockClient {
    fn platform(&self) -> Option<flotilla::Platform> {
        self.0.hello().platform.as_ref().map(|platform| flotilla::Platform::new(&platform.os, &platform.architecture))
    }
    async fn configure(&self, configuration: flotilla::Configuration) -> io::Result<String> {
        self.0.configure(drydock_agent::Configuration { environment: configuration.environment, ..Default::default() }).await
    }
    async fn configuration_tag(&self) -> io::Result<String> {
        self.0.configuration().await.map(|current| current.etag)
    }
    async fn run(&self, request: flotilla::Exec, limit: usize) -> io::Result<Output> {
        self.0.run(exec(request), limit).await.map(collected)
    }
    async fn run_with_input(&self, request: flotilla::Exec, input: Vec<u8>, limit: usize) -> io::Result<Output> {
        let process = self.0.spawn(drydock_agent::Exec { stdin: true, ..exec(request) }).await?;
        let (written, collected) = tokio::join!(process.write_stdin_bytes(input), async { process.output().await?.collect(limit).await });
        // Forget the handle, so the agent holds no output for it.
        let removed = process.remove().await;
        // A process may exit without reading all of its input, as with a
        // redirect: the broken pipe that leaves is not a failure.
        match written {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
            _ => collected.and_then(|collected| removed.map(|()| self::collected(collected))),
        }
    }
    async fn exec(&self, request: flotilla::Exec) -> io::Result<flotilla::ExecEvents> {
        let output = self.0.exec(exec(request)).await?;
        Ok(Box::pin(futures::stream::unfold(Some(output), |output| async move {
            let mut output = output?;
            let event = match output.next().await {
                Ok(None) => return None,
                Ok(Some(drydock_agent::ExecEvent::Stdout(bytes))) => Ok(flotilla::ExecEvent::Stdout(bytes.to_vec())),
                Ok(Some(drydock_agent::ExecEvent::Stderr(bytes))) => Ok(flotilla::ExecEvent::Stderr(bytes.to_vec())),
                Ok(Some(drydock_agent::ExecEvent::Exit(exit))) => Ok(flotilla::ExecEvent::Exit(status(exit))),
                Err(error) => return Some((Err(error), None)),
            };
            Some((event, Some(output)))
        })))
    }
    async fn stat(&self, path: &Path) -> io::Result<EntryKind> {
        Ok(match self.0.stat(path).await?.kind {
            drydock_agent::EntryKind::File => EntryKind::File,
            drydock_agent::EntryKind::Directory => EntryKind::Directory,
            drydock_agent::EntryKind::Symlink => EntryKind::Symlink,
            drydock_agent::EntryKind::Other => EntryKind::Other,
        })
    }
    async fn create_dir(&self, path: &Path, file: &flotilla::FileOptions) -> io::Result<()> {
        self.0.create_dir(path, &options(file)).await.map(drop)
    }
    async fn read_bytes(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        self.0.read_bytes(path, limit).await
    }
    async fn read_to(&self, path: &Path, mut destination: &mut (dyn tokio::io::AsyncWrite + Unpin + Send)) -> io::Result<u64> {
        self.0.read(path).await?.copy_to(&mut destination).await
    }
    async fn write_bytes(&self, path: &Path, content: Vec<u8>, file: &flotilla::FileOptions) -> io::Result<()> {
        self.0.write_bytes(path, content, &options(file)).await.map(drop)
    }
    async fn write(
        &self,
        path: &Path,
        content: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        file: &flotilla::FileOptions,
    ) -> io::Result<()> {
        self.0.write(path, content, &options(file)).await.map(drop)
    }
    async fn symlink(&self, path: &Path, target: &Path, file: &flotilla::FileOptions) -> io::Result<()> {
        self.0.symlink(path, target, &options(file)).await
    }
    async fn rename(&self, rename: &flotilla::Rename) -> io::Result<()> {
        let rename = drydock_agent::Rename {
            from: rename.from.clone(),
            to: rename.to.clone(),
            exclusive: rename.exclusive,
            parents: rename.parents,
        };
        self.0.rename(&rename).await
    }
    async fn fetch_artifact(&self, sha256: &str, request: &flotilla::Fetch) -> io::Result<()> {
        self.0.fetch_artifact(sha256, &fetch(request)).await.map(drop).map_err(store_full)
    }
    async fn fetch_artifact_or_push(
        &self,
        sha256: &str,
        request: &flotilla::Fetch,
        length: u64,
        content: tokio::fs::File,
    ) -> io::Result<()> {
        self.0.fetch_artifact_or_push(sha256, &fetch(request), length, content).await.map(drop).map_err(store_full)
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path, file: &flotilla::FileOptions) -> io::Result<()> {
        self.0.link_artifact(path, sha256, member, &options(file)).await
    }
    async fn artifacts(&self) -> io::Result<Vec<(String, Vec<PathBuf>)>> {
        Ok(self.0.artifacts().await?.into_iter().map(|artifact| (artifact.sha256, artifact.links)).collect())
    }
    async fn put_outward_forward(&self, forward: flotilla::OutwardForward, host: flotilla::Endpoint) -> io::Result<()> {
        let forward = drydock_agent::OutwardForward { id: forward.id, unix: forward.unix, tcp: forward.tcp };
        self.0.put_outward_forward(forward, endpoint(host)).await
    }
    async fn remove_outward_forward(&self, id: &str) -> io::Result<()> {
        self.0.remove_outward_forward(id).await
    }
    async fn put_inward_forward(&self, forward: flotilla::InwardForward) -> io::Result<()> {
        self.0.put_inward_forward(drydock_agent::InwardForward { id: forward.id, endpoint: endpoint(forward.endpoint) }).await
    }
    async fn open_inward(&self, id: &str) -> io::Result<ByteStream> {
        Ok(Box::new(self.0.open_inward(id).await?))
    }
}

#[cfg(test)]
pub(crate) mod tests;
