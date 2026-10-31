//! OpenSSH transport for Tender control and independent raw stream channels.
//!
//! The remote server publishes ordinary endpoints, without owning their
//! processes. SSH authenticates the account; the signed handshake independently
//! authenticates both Tender instances. ProxyJump and account selection remain
//! in the operator's SSH configuration. The client reaches its first instance
//! through a [`Dial`], so an OpenSSH forward is one route among others, and
//! authenticated relays carry a caller onward to hosts it cannot dial.

use std::{
    collections::BTreeMap,
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use async_trait::async_trait;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, Mutex as AsyncMutex, Semaphore},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

use crate::{
    memory::MemoryTender,
    runtime::{Clock, FileStore},
    ByteStream, Dial, Error, Exposure, Fingerprint, Lease, Publication, PublicationId, PublishRequest, Published, Router, Session, Tender,
    Watch,
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONTROL: usize = 64 * 1024;
const MAX_CHANNELS: usize = 256;
/// Bounds a relay chain, so a routing loop between relays fails promptly.
const MAX_RELAY_HOPS: u8 = 8;

/// Persist this key independently of SSH keys, account names, and routes.
#[derive(Clone)]
pub struct Identity(SigningKey);

impl Identity {
    pub fn from_secret(bytes: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&bytes))
    }

    pub fn generate() -> Self {
        Self(SigningKey::generate(&mut OsRng))
    }

    /// Create once, with user-only permissions. Never replace an unreadable or
    /// malformed key: replacement would silently change the instance identity.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        let identity = Self::generate();
        match Self::install_key(path, |file| std::io::Write::write_all(file, &identity.0.to_bytes())) {
            Ok(()) => Ok(identity),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(path)?;
                if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::other("Tender key must be a user-only regular file"));
                }
                let bytes: [u8; 32] = std::fs::read(path)?.try_into().map_err(|_| io::Error::other("invalid Tender instance key"))?;
                Ok(Self(SigningKey::from_bytes(&bytes)))
            }
            Err(error) => Err(error),
        }
    }

    // Install only after a complete, synced write; injected writer models filesystem failure.
    fn install_key(path: &Path, write: impl FnOnce(&mut std::fs::File) -> io::Result<()>) -> io::Result<()> {
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        write(temporary.as_file_mut())?;
        temporary.as_file().sync_all()?;
        temporary.persist_noclobber(path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.0.verifying_key().to_bytes())
    }
}

fn fingerprint(key: &[u8; 32]) -> Fingerprint {
    Fingerprint(Sha256::digest(key).iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn write_control<T: Serialize>(stream: &mut (impl AsyncWrite + Unpin), value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_CONTROL {
        return Err(io::Error::other("control frame too large"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

async fn read_control<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<T> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_CONTROL {
        return Err(io::Error::other("control frame too large"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[derive(Serialize, Deserialize)]
struct Challenge {
    key: [u8; 32],
    nonce: [u8; 32],
    signature: Vec<u8>,
}

fn transcript(client_nonce: &[u8; 32], server_nonce: &[u8; 32], server_key: &[u8; 32]) -> Vec<u8> {
    [b"tender-ssh-v1".as_slice(), client_nonce, server_nonce, server_key].concat()
}

async fn client_handshake(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), identity: &Identity, pin: &Fingerprint) -> Result<(), Error> {
    let mut nonce = [0; 32];
    OsRng.fill_bytes(&mut nonce);
    write_control(stream, &nonce).await.map_err(|_| Error::Unavailable)?;
    let challenge: Challenge = read_control(stream).await.map_err(|_| Error::Unavailable)?;
    if &fingerprint(&challenge.key) != pin {
        return Err(Error::IdentityMismatch);
    }
    let proof = transcript(&nonce, &challenge.nonce, &challenge.key);
    let key = VerifyingKey::from_bytes(&challenge.key).map_err(|_| Error::IdentityMismatch)?;
    let signature = Signature::from_slice(&challenge.signature).map_err(|_| Error::IdentityMismatch)?;
    key.verify(&proof, &signature).map_err(|_| Error::IdentityMismatch)?;
    let reply = Challenge {
        key: identity.0.verifying_key().to_bytes(),
        nonce,
        signature: identity.0.sign(&[b"caller".as_slice(), &proof].concat()).to_bytes().to_vec(),
    };
    write_control(stream, &reply).await.map_err(|_| Error::Unavailable)
}

async fn server_handshake(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), identity: &Identity) -> io::Result<Session> {
    let client_nonce = read_control(stream).await?;
    let mut nonce = [0; 32];
    OsRng.fill_bytes(&mut nonce);
    let key = identity.0.verifying_key().to_bytes();
    let proof = transcript(&client_nonce, &nonce, &key);
    write_control(stream, &Challenge { key, nonce, signature: identity.0.sign(&proof).to_bytes().to_vec() }).await?;
    let reply: Challenge = read_control(stream).await?;
    let caller = VerifyingKey::from_bytes(&reply.key).map_err(io::Error::other)?;
    let signature = Signature::from_slice(&reply.signature).map_err(io::Error::other)?;
    caller.verify(&[b"caller".as_slice(), &proof].concat(), &signature).map_err(io::Error::other)?;
    Ok(Session { caller: fingerprint(&reply.key), pinned_host: identity.fingerprint(), via: None })
}

#[derive(Serialize, Deserialize)]
enum Request {
    Publish(PublishRequest),
    Accept(Lease),
    Browse,
    Watch,
    Connect(PublicationId),
    Expose(PublicationId),
    Probe(PublicationId),
    Disconnect(Lease),
    Withdraw(Lease),
    /// Carry the stream to `host`. The requester authenticated as itself; once
    /// the stream opens, the original caller authenticates end to end.
    Relay {
        host: Fingerprint,
        hops: u8,
    },
}

#[derive(Serialize, Deserialize)]
enum Response {
    Lease(Lease),
    Publications(Vec<Publication>),
    Exposure(Exposure),
    Open,
    Done,
    Error(Error),
}

type Registrations = Arc<Mutex<BTreeMap<(u64, u64), Arc<Registration>>>>;

struct Registration {
    lease: Lease,
    incoming: AsyncMutex<mpsc::UnboundedReceiver<ByteStream>>,
}

/// A host-side listener. Its policy controls are the slice-1 host authority;
/// incoming callers are constructed from verified keys, never wire assertions.
pub struct Server {
    pub policy: MemoryTender,
    task: JoinHandle<()>,
    path: PathBuf,
    router: Relays,
}

type Relays = Arc<Mutex<Option<Arc<dyn Router>>>>;

impl Server {
    /// Host a durable Tender authority. The private state directory owns both
    /// the independently persisted instance key and versioned policy store.
    pub fn open(path: PathBuf, directory: &Path, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let identity = Identity::load_or_create(&directory.join("identity"))?;
        let store = Arc::new(FileStore::new(directory.join("authority.json"))?);
        let policy = MemoryTender::hosted(identity.fingerprint(), clock, store)?;
        // The exclusive policy-store lock is held before stale socket recovery.
        // A live listener or any non-socket path is never removed.
        let parent = path.parent().ok_or_else(|| io::Error::other("listener needs a private parent directory"))?;
        if std::fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("Tender listener parent directory must be user-only"));
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => match std::os::unix::net::UnixStream::connect(&path) {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(&path)?,
                _ => return Err(io::Error::other("Tender control socket is already in use")),
            },
            Ok(_) => return Err(io::Error::other("Tender control path is not a socket")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Self::bind(path, identity, policy)
    }

    /// Bind an explicitly supplied authority, primarily for adapter rigs. This
    /// does not upgrade a manual-clock policy; use [`Self::open`] for deployment.
    pub fn bind(path: PathBuf, identity: Identity, policy: MemoryTender) -> io::Result<Self> {
        if identity.fingerprint() != policy.host() {
            return Err(io::Error::other("policy host does not match instance key"));
        }
        let parent = path.parent().ok_or_else(|| io::Error::other("listener needs a private parent directory"))?;
        if std::fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("Tender listener parent directory must be user-only"));
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let registrations = Arc::new(Mutex::new(BTreeMap::new()));
        let authority = policy.clone();
        let router: Relays = Arc::new(Mutex::new(None));
        let relays = router.clone();
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(MAX_CHANNELS));
            let mut cleanup = tokio::time::interval(Duration::from_secs(1));
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            tracing::debug!(max_channels = MAX_CHANNELS, "Tender channel admission limit reached");
                            continue
                        };
                        let policy = authority.clone();
                        let identity = identity.clone();
                        let registrations = registrations.clone();
                        let router = relays.lock().expect("router lock").clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            let _ = serve(stream, identity, policy, registrations, router).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                    _ = cleanup.tick() => {
                        registrations.lock().expect("registrations lock").retain(|_, registration| authority.lease_active(&registration.lease));
                    }
                }
            }
        });
        Ok(Self { policy, task, path, router })
    }

    /// Forward relay requests for other hosts along the hosting application's
    /// routes. Without a router this instance only terminates relays addressed
    /// to it. Established relayed streams keep the route they opened on.
    pub fn route_relays(&self, router: Arc<dyn Router>) {
        *self.router.lock().expect("router lock") = Some(router);
    }

    /// Publish an already-running ordinary Unix endpoint. Neither publication
    /// nor forwarding starts, cleans, or otherwise manages that service.
    pub async fn publish_endpoint(&self, session: &Session, request: PublishRequest, endpoint: PathBuf) -> Result<Lease, Error> {
        let mut published = self.policy.publish(session, request).await?;
        let lease = published.lease.clone();
        let authority = self.policy.clone();
        let session = session.clone();
        tokio::spawn(async move {
            while let Some(mut stream) = published.incoming.recv().await {
                let endpoint = endpoint.clone();
                let authority = authority.clone();
                let session = session.clone();
                let lease = published.lease.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(mut service)) = timeout(OPEN_TIMEOUT, UnixStream::connect(endpoint)).await {
                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut service).await;
                    } else {
                        // A forward listener alone is not endpoint health. Keep
                        // the identity reserved, close streams, and report loss.
                        let _ = authority.disconnect(&session, &lease).await;
                    }
                });
            }
        });
        Ok(lease)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = self.policy.restart();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve(
    mut stream: impl AsyncRead + AsyncWrite + Unpin + Send,
    identity: Identity,
    policy: MemoryTender,
    registrations: Registrations,
    router: Option<Arc<dyn Router>>,
) -> io::Result<()> {
    let (session, request) = timeout(OPEN_TIMEOUT, async {
        let session = server_handshake(&mut stream, &identity).await?;
        let request = read_control(&mut stream).await?;
        Ok::<_, io::Error>((session, request))
    })
    .await
    .map_err(io::Error::other)??;
    let (session, request) = match request {
        Request::Relay { host, .. } if host == identity.fingerprint() => {
            if let Err(error) = policy.check_intermediary(&session) {
                return write_control(&mut stream, &Response::Error(error)).await;
            }
            write_control(&mut stream, &Response::Open).await?;
            // The relay is recorded from its own handshake; the caller cannot
            // assert it, and cannot nest another relay inside this one.
            let (caller, request) = timeout(OPEN_TIMEOUT, async {
                let caller = server_handshake(&mut stream, &identity).await?;
                let request = read_control(&mut stream).await?;
                Ok::<_, io::Error>((caller, request))
            })
            .await
            .map_err(io::Error::other)??;
            if matches!(request, Request::Relay { .. }) {
                return write_control(&mut stream, &Response::Error(Error::UntrustedIntermediary)).await;
            }
            (Session { via: Some(session.caller), ..caller }, request)
        }
        Request::Relay { host, hops } => return forward(stream, &identity, &policy, router, &session, host, hops).await,
        request => (session, request),
    };
    let response = match request {
        Request::Publish(request) => match policy.publish(&session, request).await {
            Ok(published) => {
                let lease = published.lease;
                registrations.lock().expect("registrations lock").insert(
                    (lease.id.0, lease.generation),
                    Arc::new(Registration { lease: lease.clone(), incoming: AsyncMutex::new(published.incoming) }),
                );
                // Cleanup also runs when the client cancels before receiving
                // its lease response: a failed acknowledgement cannot orphan it.
                let control = async {
                    write_control(&mut stream, &Response::Lease(lease.clone())).await?;
                    let mut byte = [0];
                    let mut health = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            result = stream.read(&mut byte) => { result?; break }
                            _ = health.tick() => { if !policy.lease_active(&lease) { break } }
                        }
                    }
                    Ok::<_, io::Error>(())
                }
                .await;
                let _ = policy.disconnect(&session, &lease).await;
                registrations.lock().expect("registrations lock").remove(&(lease.id.0, lease.generation));
                return control;
            }
            Err(error) => Response::Error(error),
        },
        Request::Browse => match policy.browse(&session).await {
            Ok(publications) => Response::Publications(publications),
            Err(error) => Response::Error(error),
        },
        Request::Watch => match policy.watch(&session).await {
            Ok(mut watch) => {
                let mut byte = [0];
                loop {
                    tokio::select! {
                        publications = watch.recv() => {
                            let Some(publications) = publications else { break };
                            write_control(&mut stream, &Response::Publications(publications)).await?;
                        }
                        _ = stream.read(&mut byte) => break,
                    }
                }
                return Ok(());
            }
            Err(error) => Response::Error(error),
        },
        Request::Probe(id) => match policy.check_connect(&session, id) {
            Ok(()) => Response::Done,
            Err(error) => Response::Error(error),
        },
        Request::Expose(id) => match policy.expose_local(&session, id).await {
            Ok(exposure) => Response::Exposure(exposure),
            Err(error) => Response::Error(error),
        },
        Request::Connect(id) => match policy.connect(&session, id).await {
            Ok(mut service) => {
                write_control(&mut stream, &Response::Open).await?;
                tokio::io::copy_bidirectional(&mut stream, &mut service).await?;
                return Ok(());
            }
            Err(error) => Response::Error(error),
        },
        Request::Accept(lease) => {
            let registration = registrations.lock().expect("registrations lock").get(&(lease.id.0, lease.generation)).cloned();
            match registration {
                Some(registration) if registration.lease == lease && lease.publisher == session.caller => {
                    let mut byte = [0];
                    // A duplicate accept must observe its own cancellation
                    // while waiting for the incumbent's receiver lock.
                    let mut incoming = tokio::select! {
                        incoming = registration.incoming.lock() => incoming,
                        _ = stream.read(&mut byte) => return Ok(()),
                    };
                    // The held Publish control channel owns publisher lifetime.
                    tokio::select! {
                        service = incoming.recv() => {
                            match service {
                                Some(mut service) => {
                                    drop(incoming);
                                    write_control(&mut stream, &Response::Open).await?;
                                    tokio::io::copy_bidirectional(&mut stream, &mut service).await?;
                                    return Ok(());
                                }
                                None => Response::Error(Error::Unavailable),
                            }
                        }
                        _ = stream.read(&mut byte) => {
                        let _ = policy.disconnect(&session, &lease).await;
                        registrations.lock().expect("registrations lock").remove(&(lease.id.0, lease.generation));
                        return Ok(());
                    },
                    }
                }
                _ => Response::Error(Error::Denied),
            }
        }
        Request::Disconnect(lease) => match policy.disconnect(&session, &lease).await {
            Ok(()) => {
                registrations.lock().expect("registrations lock").remove(&(lease.id.0, lease.generation));
                Response::Done
            }
            Err(error) => Response::Error(error),
        },
        Request::Withdraw(lease) => match policy.withdraw(&session, &lease).await {
            Ok(()) => {
                registrations.lock().expect("registrations lock").remove(&(lease.id.0, lease.generation));
                Response::Done
            }
            Err(error) => Response::Error(error),
        },
        Request::Relay { .. } => Response::Error(Error::UntrustedIntermediary),
    };
    write_control(&mut stream, &response).await
}

/// Relay one stream toward `host`, authenticating to the next instance as this
/// one. After `Open` the stream is a byte splice: the caller's handshake and
/// every later byte pass through unread. Like a direct connect, an open splice
/// has no idle bound and holds one channel permit until either side closes;
/// idle policy belongs to the hosting application. Refusals reach the caller
/// unchanged, but a hop that stalls surfaces as the caller's own `Deadline`.
async fn forward(
    mut upstream: impl AsyncRead + AsyncWrite + Unpin,
    identity: &Identity,
    policy: &MemoryTender,
    router: Option<Arc<dyn Router>>,
    session: &Session,
    host: Fingerprint,
    hops: u8,
) -> io::Result<()> {
    let opened = async {
        policy.check_forward(session)?;
        let remaining = hops.checked_sub(1).ok_or(Error::Unavailable)?;
        let router = router.ok_or(Error::Unavailable)?;
        let hop = router.next_hop(&host).ok_or(Error::Unavailable)?;
        if hop == identity.fingerprint() {
            return Err(Error::Unavailable);
        }
        timeout(OPEN_TIMEOUT, async {
            let mut downstream = router.dial(&hop).await.map_err(|_| Error::Unavailable)?;
            client_handshake(&mut downstream, identity, &hop).await?;
            write_control(&mut downstream, &Request::Relay { host, hops: remaining }).await.map_err(|_| Error::Unavailable)?;
            match read_control(&mut downstream).await.map_err(|_| Error::Unavailable)? {
                Response::Open => Ok(downstream),
                Response::Error(error) => Err(error),
                _ => Err(Error::Unavailable),
            }
        })
        .await
        .map_err(|_| Error::Deadline)?
    }
    .await;
    // A failed open drops the downstream stream, with any partial handshake,
    // and reports only the classified error upstream. Once spliced, a transport
    // error ends this connection's task and closes both sides; there is no
    // channel left on which to report it.
    match opened {
        Ok(mut downstream) => {
            write_control(&mut upstream, &Response::Open).await?;
            tokio::io::copy_bidirectional(&mut upstream, &mut downstream).await?;
            Ok(())
        }
        Err(error) => write_control(&mut upstream, &Response::Error(error)).await,
    }
}

/// Generic stream-local forward specification extracted from the peer
/// transport: it carries paths, not NodeId, peer messages, or daemon discovery.
pub fn forward_spec(local: &Path, remote: &Path) -> String {
    format!("{}:{}", local.display(), remote.display())
}

/// Whether an owned legacy forward may replace its stale socket file.
#[derive(Clone, Copy)]
pub enum ExistingSocket {
    Refuse,
    Unlink,
}

/// OpenSSH's forwarding-only argument list, shared with the retiring peer
/// transport. Reverse forwarding is optional and carries only endpoint paths.
pub fn forwarding_arguments(
    destination: &str,
    local: &Path,
    remote: &Path,
    reverse: Option<(&Path, &Path)>,
    existing_socket: ExistingSocket,
) -> Vec<String> {
    let mut args = vec!["-N".into(), "-L".into(), forward_spec(local, remote)];
    if let Some((remote_listener, local_endpoint)) = reverse {
        args.extend(["-R".into(), forward_spec(remote_listener, local_endpoint)]);
    }
    args.extend([
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        format!(
            "StreamLocalBindUnlink={}",
            match existing_socket {
                ExistingSocket::Unlink => "yes",
                ExistingSocket::Refuse => "no",
            }
        ),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        destination.into(),
    ]);
    args
}

/// OpenSSH arguments that publish a local endpoint at a remote socket path and
/// nothing else: no local forward and no remote command. The remote listener is
/// bound by sshd, whose own `StreamLocalBindUnlink` decides whether a stale file
/// is replaced, so callers remove a stale remote socket before dialling.
pub fn reverse_forwarding_arguments(destination: &str, remote_listener: &Path, local_endpoint: &Path) -> Vec<String> {
    vec![
        "-N".into(),
        "-T".into(),
        "-R".into(),
        forward_spec(remote_listener, local_endpoint),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        destination.into(),
    ]
}

/// Wait for a local forward, detecting early process exit. The process probe
/// keeps this reusable by Flotilla's injected runner without importing it.
pub async fn wait_for_socket(
    socket: &Path,
    bound: Duration,
    mut process_status: impl FnMut() -> Result<Option<std::process::ExitStatus>, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = process_status()? {
            return Err(format!("ssh exited prematurely with {status}"));
        }
        if socket.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for forwarded socket at {}", socket.display()));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// A held SSH transport. The private forward is separate from stable public
/// exposures so an SSH outage cannot rebind an exposure to a different service.
pub struct Forward {
    child: Child,
    _directory: tempfile::TempDir,
    socket: PathBuf,
}

impl Forward {
    /// Destination and extra SSH options are trusted operator configuration,
    /// not publisher input. This never accepts a remote command argument.
    pub async fn start(destination: &str, remote: &Path, ssh_options: &[String]) -> io::Result<Self> {
        if destination.starts_with('-') || destination.is_empty() {
            return Err(io::Error::other("invalid SSH destination"));
        }
        let directory = tempfile::Builder::new().prefix("tender-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir()?;
        let socket = directory.path().join("route");
        let mut child = Command::new("ssh")
            .args(["-o", "StreamLocalBindMask=0177", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"])
            .args(ssh_options)
            .args(forwarding_arguments(destination, &socket, remote, None, ExistingSocket::Refuse))
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        if let Err(error) = wait_for_socket(&socket, OPEN_TIMEOUT, || child.try_wait().map_err(|error| error.to_string())).await {
            let _ = child.kill().await;
            return Err(io::Error::other(error));
        }
        Ok(Self { child, _directory: directory, socket })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn process_id(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn stop(&mut self) -> io::Result<()> {
        self.child.kill().await?;
        let _ = std::fs::remove_file(&self.socket);
        Ok(())
    }

    pub fn is_alive(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Remote Tender operations over an established SSH forward. Credentials are
/// explicit; accepting an arbitrary Session never lets a caller forge a key.
type ExposureMap = BTreeMap<(Fingerprint, PublicationId), Arc<LocalExposure>>;
type Exposures = Arc<Mutex<ExposureMap>>;

/// Dials one Unix socket path whichever instance is named, such as an OpenSSH
/// stream-local forward or a host's own Tender listener. A single path cannot
/// reach a distinct relay: a session naming one in `via` fails as
/// `IdentityMismatch` when the host answers instead.
pub struct UnixDial(pub PathBuf);

#[async_trait]
impl Dial for UnixDial {
    async fn dial(&self, _: &Fingerprint) -> io::Result<ByteStream> {
        Ok(Box::new(UnixStream::connect(&self.0).await?))
    }
}

#[derive(Clone)]
pub struct SshTender {
    route: Arc<Mutex<Arc<dyn Dial>>>,
    credentials: Arc<BTreeMap<Fingerprint, Identity>>,
    pin: Fingerprint,
    exposures: Exposures,
    directory: Arc<tempfile::TempDir>,
    snapshots: Arc<Mutex<BTreeMap<Fingerprint, Vec<Publication>>>>,
}

struct LocalExposure {
    path: PathBuf,
    task: JoinHandle<()>,
}

struct ExposureCleanup {
    path: PathBuf,
    entries: Weak<Mutex<ExposureMap>>,
    key: (Fingerprint, PublicationId),
}

impl Drop for ExposureCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(entries) = self.entries.upgrade() {
            // Drop the removed handle outside the lock.
            let removed = entries.lock().expect("exposures lock").remove(&self.key);
            drop(removed);
        }
    }
}

impl Drop for LocalExposure {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SshTender {
    /// Dial one socket path. It reaches only the pinned host, never a relay a
    /// session names in `via`; use [`Self::with_dial`] for relayed sessions.
    pub fn new(route: PathBuf, pin: Fingerprint, identities: Vec<Identity>) -> io::Result<Self> {
        Self::with_dial(Arc::new(UnixDial(route)), pin, identities)
    }

    /// Each operation dials a fresh stream to its first instance: the pinned
    /// host, or the relay a session names in `via`.
    pub fn with_dial(dial: Arc<dyn Dial>, pin: Fingerprint, identities: Vec<Identity>) -> io::Result<Self> {
        Ok(Self {
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            route: Arc::new(Mutex::new(dial)),
            credentials: Arc::new(identities.into_iter().map(|identity| (identity.fingerprint(), identity)).collect()),
            pin,
            exposures: Arc::new(Mutex::new(BTreeMap::new())),
            directory: Arc::new(
                tempfile::Builder::new().prefix("tender-exposures-").permissions(std::fs::Permissions::from_mode(0o700)).tempdir()?,
            ),
        })
    }

    /// Recovery affects only fresh connections. Existing sockets are never
    /// transferred to this route and no buffered application bytes are replayed.
    pub fn replace_route(&self, route: PathBuf) {
        self.replace_dial(Arc::new(UnixDial(route)));
    }

    /// As [`Self::replace_route`], for any dialer.
    pub fn replace_dial(&self, dial: Arc<dyn Dial>) {
        *self.route.lock().expect("route lock") = dial;
    }

    async fn request(&self, session: &Session, request: Request) -> Result<ByteStream, Error> {
        if session.pinned_host != self.pin {
            return Err(Error::IdentityMismatch);
        }
        let identity = self.credentials.get(&session.caller).ok_or(Error::Denied)?;
        let dial = self.route.lock().expect("route lock").clone();
        timeout(OPEN_TIMEOUT, async {
            let mut stream = dial.dial(session.via.as_ref().unwrap_or(&self.pin)).await.map_err(|_| Error::Unavailable)?;
            if let Some(relay) = &session.via {
                // The caller pins its chosen relay before trusting it with the
                // stream; the host independently decides whether to accept it.
                client_handshake(&mut stream, identity, relay).await?;
                write_control(&mut stream, &Request::Relay { host: self.pin.clone(), hops: MAX_RELAY_HOPS })
                    .await
                    .map_err(|_| Error::Unavailable)?;
                match read_control(&mut stream).await.map_err(|_| Error::Unavailable)? {
                    Response::Open => {}
                    Response::Error(error) => return Err(error),
                    _ => return Err(Error::Unavailable),
                }
            }
            client_handshake(&mut stream, identity, &self.pin).await?;
            write_control(&mut stream, &request).await.map_err(|_| Error::Unavailable)?;
            Ok(stream)
        })
        .await
        .map_err(|_| Error::Deadline)?
    }

    async fn reply(&self, session: &Session, request: Request) -> Result<Response, Error> {
        let mut stream = self.request(session, request).await?;
        let response =
            timeout(OPEN_TIMEOUT, read_control(&mut stream)).await.map_err(|_| Error::Deadline)?.map_err(|_| Error::Unavailable)?;
        match response {
            Response::Error(error) => Err(error),
            response => Ok(response),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum RetryPause {
    Elapsed,
    Cancelled,
}

async fn pause_publisher_retry(
    sender: &mpsc::UnboundedSender<ByteStream>,
    registration: &mut (impl AsyncRead + Unpin),
    delay: Duration,
) -> RetryPause {
    let mut byte = [0];
    tokio::select! {
        _ = sender.closed() => RetryPause::Cancelled,
        _ = registration.read(&mut byte) => RetryPause::Cancelled,
        _ = sleep(delay) => RetryPause::Elapsed,
    }
}

#[async_trait]
impl Tender for SshTender {
    async fn publish(&self, session: &Session, request: PublishRequest) -> Result<Published, Error> {
        let mut registration_stream = self.request(session, Request::Publish(request)).await?;
        let response = timeout(OPEN_TIMEOUT, read_control(&mut registration_stream))
            .await
            .map_err(|_| Error::Deadline)?
            .map_err(|_| Error::Unavailable)?;
        let lease = match response {
            Response::Lease(lease) => lease,
            Response::Error(error) => return Err(error),
            _ => return Err(Error::Unavailable),
        };
        let (sender, incoming) = mpsc::unbounded_channel();
        let adapter = self.clone();
        let session = session.clone();
        let held_lease = lease.clone();
        tokio::spawn(async move {
            let mut byte = [0];
            let mut retry_delay = Duration::from_millis(100);
            'publisher: loop {
                let accept = async {
                    let mut stream = adapter.request(&session, Request::Accept(held_lease.clone())).await?;
                    match read_control(&mut stream).await.map_err(|_| Error::Unavailable)? {
                        Response::Open => Ok(stream),
                        Response::Error(error) => Err(error),
                        _ => Err(Error::Unavailable),
                    }
                };
                tokio::select! {
                    _ = sender.closed() => break,
                    _ = registration_stream.read(&mut byte) => break,
                    stream = accept => {
                        match stream {
                            Ok(stream) => {
                                retry_delay = Duration::from_millis(100);
                                if sender.send(stream).is_err() { break }
                            }
                            Err(Error::Unavailable | Error::Deadline) => {
                                // Backoff cannot delay lease loss or owner cancellation.
                                if pause_publisher_retry(&sender, &mut registration_stream, retry_delay).await == RetryPause::Cancelled {
                                    break 'publisher;
                                }
                                retry_delay = (retry_delay * 2).min(Duration::from_secs(8));
                            },
                            Err(_) => break,
                        }
                    }
                }
            }
            drop(registration_stream);
            let _ = adapter.disconnect(&session, &held_lease).await;
        });
        Ok(Published { lease, incoming })
    }

    async fn browse(&self, session: &Session) -> Result<Vec<Publication>, Error> {
        match self.reply(session, Request::Browse).await {
            Ok(Response::Publications(publications)) => {
                self.snapshots.lock().expect("snapshots lock").insert(session.caller.clone(), publications.clone());
                Ok(publications)
            }
            Err(Error::Unavailable) => {
                let snapshots = self.snapshots.lock().expect("snapshots lock");
                let mut remembered = snapshots.get(&session.caller).cloned().ok_or(Error::Unavailable)?;
                for publication in &mut remembered {
                    if publication.availability == crate::Availability::Available {
                        publication.availability = crate::Availability::Unavailable;
                    }
                }
                Ok(remembered)
            }
            Err(error) => Err(error),
            _ => Err(Error::Unavailable),
        }
    }

    async fn watch(&self, session: &Session) -> Result<Watch, Error> {
        let mut stream = self.request(session, Request::Watch).await?;
        let first = timeout(OPEN_TIMEOUT, read_control(&mut stream)).await.map_err(|_| Error::Deadline)?.map_err(|_| Error::Unavailable)?;
        let Response::Publications(first) = first else {
            return match first {
                Response::Error(error) => Err(error),
                _ => Err(Error::Unavailable),
            };
        };
        self.snapshots.lock().expect("snapshots lock").insert(session.caller.clone(), first.clone());
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(first.clone()).expect("new watch receiver");
        let adapter = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let mut remembered = first;
            loop {
                tokio::select! {
                    _ = sender.closed() => break,
                    response = read_control(&mut stream) => {
                        match response {
                            Ok(Response::Publications(publications)) => {
                                adapter.snapshots.lock().expect("snapshots lock").insert(session.caller.clone(), publications.clone());
                                remembered = publications.clone();
                                if sender.send(publications).is_err() { break }
                            }
                            _ => {
                                for publication in &mut remembered {
                                    if publication.availability == crate::Availability::Available {
                                        publication.availability = crate::Availability::Unavailable;
                                    }
                                }
                                if sender.send(remembered.clone()).is_err() { break }
                                loop {
                                    tokio::select! {
                                        _ = sender.closed() => return,
                                        _ = sleep(Duration::from_millis(100)) => {}
                                    }
                                    match adapter.request(&session, Request::Watch).await {
                                        Ok(reconnected) => { stream = reconnected; break }
                                        Err(Error::IdentityMismatch | Error::Denied | Error::UntrustedIntermediary) => return,
                                        Err(_) => {}
                                    }
                                }
                            },
                        }
                    }
                }
            }
        });
        Ok(receiver)
    }

    async fn connect(&self, session: &Session, id: PublicationId) -> Result<ByteStream, Error> {
        let mut stream = self.request(session, Request::Connect(id)).await?;
        match timeout(OPEN_TIMEOUT, read_control(&mut stream)).await.map_err(|_| Error::Deadline)?.map_err(|_| Error::Unavailable)? {
            Response::Open => Ok(stream),
            Response::Error(error) => Err(error),
            _ => Err(Error::Unavailable),
        }
    }

    async fn expose_local(&self, session: &Session, id: PublicationId) -> Result<Exposure, Error> {
        match self.reply(session, Request::Expose(id)).await? {
            Response::Exposure(_) => {}
            _ => return Err(Error::Unavailable),
        }
        match self.reply(session, Request::Probe(id)).await {
            Ok(Response::Done) | Err(Error::Unavailable) => {}
            Err(error) => return Err(error),
            _ => return Err(Error::Unavailable),
        }
        let key = (session.caller.clone(), id);
        let caller_hash: String = Sha256::digest(session.caller.0.as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect();
        let path = self.directory.path().join(format!("{caller_hash}-{}", id.0));
        if !self.exposures.lock().expect("exposures lock").contains_key(&key) {
            let listener = UnixListener::bind(&path).map_err(|_| Error::Unavailable)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|_| Error::Unavailable)?;
            // Avoid a reference cycle between the adapter and its owned task.
            let adapter = Self { exposures: Arc::new(Mutex::new(BTreeMap::new())), ..self.clone() };
            let session = session.clone();
            let owned_path = path.clone();
            let cleanup = ExposureCleanup { path: path.clone(), entries: Arc::downgrade(&self.exposures), key: key.clone() };
            let task = tokio::spawn(async move {
                let _cleanup = cleanup;
                let mut health_delay = Duration::from_secs(1);
                let mut next_health = Instant::now() + health_delay;
                let mut streams = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((mut local, _)) = accepted else { break };
                            let adapter = adapter.clone();
                            let session = session.clone();
                            streams.spawn(async move {
                                if let Ok(mut remote) = adapter.connect(&session, id).await {
                                    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                                }
                            });
                        }
                        _ = streams.join_next(), if !streams.is_empty() => {}
                        _ = sleep(next_health.saturating_duration_since(Instant::now())) => {
                            let result = adapter.reply(&session, Request::Probe(id)).await;
                            if matches!(result, Err(Error::Withdrawn | Error::Denied)) {
                                drop(listener);
                                let _ = std::fs::remove_file(&owned_path);
                                // Expiry forbids new opens, but established streams run to close.
                                while streams.join_next().await.is_some() {}
                                break;
                            }
                            health_delay = if result.is_err() { (health_delay * 2).min(Duration::from_secs(8)) } else { Duration::from_secs(1) };
                            next_health = Instant::now() + health_delay;
                        }
                    }
                }
            });
            self.exposures.lock().expect("exposures lock").insert(key, Arc::new(LocalExposure { path: path.clone(), task }));
        }
        Ok(Exposure { host: self.pin.clone(), publication: id, local_name: path.to_string_lossy().into_owned() })
    }

    async fn open_exposure(&self, session: &Session, exposure: &Exposure) -> Result<ByteStream, Error> {
        if exposure.host != self.pin {
            return Err(Error::IdentityMismatch);
        }
        // The explicit interface preserves classified errors; ordinary socket
        // clients instead observe close, without injected diagnostic bytes.
        self.connect(session, exposure.publication).await
    }

    async fn disconnect(&self, session: &Session, lease: &Lease) -> Result<(), Error> {
        match self.reply(session, Request::Disconnect(lease.clone())).await? {
            Response::Done => Ok(()),
            _ => Err(Error::Unavailable),
        }
    }

    async fn withdraw(&self, session: &Session, lease: &Lease) -> Result<(), Error> {
        match self.reply(session, Request::Withdraw(lease.clone())).await? {
            Response::Done => Ok(()),
            _ => Err(Error::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3618: a reverse forward publishes only the remote listener. It carries
    // no local forward or remote command, and the destination is the last word.
    #[test]
    fn reverse_forward_carries_only_the_remote_listener() {
        let args = reverse_forwarding_arguments("b1", Path::new("/run/user/1000/f.sock"), Path::new("/tmp/env/f.sock"));
        assert_eq!(args.iter().filter(|arg| *arg == "-R").count(), 1);
        assert!(!args.iter().any(|arg| arg == "-L"));
        assert_eq!(args[args.iter().position(|arg| arg == "-R").expect("-R") + 1], "/run/user/1000/f.sock:/tmp/env/f.sock");
        assert!(args.windows(2).any(|pair| pair == ["-o", "ExitOnForwardFailure=yes"]));
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert_eq!(args.last().map(String::as_str), Some("b1"));
        assert_eq!(args[0], "-N");
    }

    // A failed partial write must not install an invalid permanent key file.
    #[test]
    fn failed_key_write_leaves_no_destination() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("key");
        let result = Identity::install_key(&path, |file| {
            std::io::Write::write_all(file, b"partial")?;
            Err(io::Error::other("injected filesystem write failure"))
        });
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(Identity::load_or_create(&path).is_ok());
    }

    // A duplicate pending accept observes EOF even while another accept holds
    // the receiver; cancelling the duplicate must not disconnect the incumbent.
    #[tokio::test]
    async fn duplicate_accept_can_cancel_while_receiver_is_locked() {
        let host = Identity::generate();
        let publisher = Identity::generate();
        let policy = MemoryTender::new(host.fingerprint());
        let caller = publisher.fingerprint();
        policy.allow_browse(caller.clone()).expect("host policy");
        policy
            .grant(crate::Grant {
                grantee: caller.clone(),
                namespace: crate::Namespace("n".into()),
                audience_ceiling: std::collections::BTreeSet::from([caller.clone()]),
                expires_at: 100,
            })
            .expect("host policy");
        let session = Session { caller: caller.clone(), pinned_host: host.fingerprint(), via: None };
        let published = policy
            .publish(
                &session,
                PublishRequest {
                    namespace: crate::Namespace("n".into()),
                    name: "service".into(),
                    audience: std::collections::BTreeSet::from([caller]),
                    reclaim: None,
                },
            )
            .await
            .expect("publish");
        let lease = published.lease;
        let registration = Arc::new(Registration { lease: lease.clone(), incoming: AsyncMutex::new(published.incoming) });
        let incumbent = registration.incoming.lock().await;
        let registrations = Arc::new(Mutex::new(BTreeMap::from([((lease.id.0, lease.generation), registration.clone())])));
        let (mut client, server) = UnixStream::pair().expect("transport pair");
        let authority = policy.clone();
        let server_identity = host.clone();
        let pending = tokio::spawn(async move { serve(server, server_identity, authority, registrations, None).await });
        client_handshake(&mut client, &publisher, &host.fingerprint()).await.expect("authenticate publisher");
        write_control(&mut client, &Request::Accept(lease)).await.expect("duplicate accept request");
        drop(client);
        timeout(Duration::from_secs(1), pending)
            .await
            .expect("duplicate cancelled without releasing incumbent lock")
            .expect("server task")
            .expect("cancellation");
        assert_eq!(policy.browse(&session).await.expect("host metadata")[0].availability, crate::Availability::Available);
        drop(incumbent);
    }

    // The host refuses an untrusted relay at its own handshake, before any
    // relayed caller can authenticate or send a request through it; a trusted
    // relay is answered with Open.
    #[tokio::test]
    async fn host_admits_only_trusted_relays_before_the_caller() {
        let host = Identity::generate();
        let policy = MemoryTender::new(host.fingerprint());
        let trusted = Identity::generate();
        policy.trust_intermediary(trusted.fingerprint()).expect("host policy");
        for (relay, admitted) in [(trusted, true), (Identity::generate(), false)] {
            let (mut client, server) = UnixStream::pair().expect("transport pair");
            let pending = tokio::spawn(serve(server, host.clone(), policy.clone(), Arc::new(Mutex::new(BTreeMap::new())), None));
            client_handshake(&mut client, &relay, &host.fingerprint()).await.expect("authenticate relay");
            write_control(&mut client, &Request::Relay { host: host.fingerprint(), hops: 1 }).await.expect("relay request");
            match read_control(&mut client).await.expect("admission") {
                Response::Open => assert!(admitted, "untrusted relay opened"),
                Response::Error(error) => assert!(!admitted && error == Error::UntrustedIntermediary, "{error:?}"),
                _ => panic!("unexpected admission response"),
            }
            drop(client);
            let _ = timeout(Duration::from_secs(1), pending).await.expect("host closes");
        }
    }

    // A stale file cannot hide a dead forward, and process-probe errors are
    // surfaced instead of falsely certifying readiness from the file alone.
    #[tokio::test]
    async fn socket_waiter_checks_process_before_stale_file() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir().expect("directory");
        let socket = directory.path().join("stale");
        std::fs::write(&socket, b"").expect("stale file");
        assert!(wait_for_socket(&socket, Duration::from_secs(1), || Ok(Some(std::process::ExitStatus::from_raw(256)))).await.is_err());
        assert_eq!(wait_for_socket(&socket, Duration::from_secs(1), || Err("probe failed".into())).await, Err("probe failed".into()));
    }

    // Both cancellation sources interrupt even the maximum outage backoff;
    // the real Unix pair models the registration channel, not application data.
    #[tokio::test]
    async fn maximum_publisher_backoff_remains_cancellable() {
        for cause in ["owner", "registration"] {
            let (sender, receiver) = mpsc::unbounded_channel();
            let (mut registration, remote) = UnixStream::pair().expect("registration pair");
            let paused = tokio::spawn(async move { pause_publisher_retry(&sender, &mut registration, Duration::from_secs(8)).await });
            match cause {
                "owner" => drop(receiver),
                "registration" => drop(remote),
                _ => unreachable!("generator covers the two cancellation sources"),
            }
            assert_eq!(
                timeout(Duration::from_secs(1), paused).await.expect("prompt backoff cancellation").expect("retry task"),
                RetryPause::Cancelled
            );
        }
    }
}
