//! OpenSSH transport for Tender control and independent raw stream channels.
//!
//! The remote server publishes ordinary endpoints, without owning their
//! processes. SSH authenticates the account; the signed handshake independently
//! authenticates both Tender instances. ProxyJump and account selection remain
//! in the operator's SSH configuration.

use std::{
    collections::BTreeMap,
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, Mutex as AsyncMutex},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

use crate::{
    memory::MemoryTender, ByteStream, Error, Exposure, Fingerprint, Lease, Publication, PublicationId, PublishRequest, Published, Session,
    Tender, Watch,
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONTROL: usize = 64 * 1024;

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
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
            Ok(mut file) => {
                let identity = Self::generate();
                std::io::Write::write_all(&mut file, &identity.0.to_bytes())?;
                file.sync_all()?;
                Ok(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let bytes: [u8; 32] = std::fs::read(path)?.try_into().map_err(|_| io::Error::other("invalid Tender instance key"))?;
                Ok(Self(SigningKey::from_bytes(&bytes)))
            }
            Err(error) => Err(error),
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.0.verifying_key().to_bytes())
    }
}

fn fingerprint(key: &[u8; 32]) -> Fingerprint {
    Fingerprint(Sha256::digest(key).iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn write_control<T: Serialize>(stream: &mut UnixStream, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_CONTROL {
        return Err(io::Error::other("control frame too large"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

async fn read_control<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
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

async fn client_handshake(stream: &mut UnixStream, identity: &Identity, pin: &Fingerprint) -> Result<(), Error> {
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

async fn server_handshake(stream: &mut UnixStream, identity: &Identity) -> io::Result<Session> {
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
}

impl Server {
    pub fn bind(path: PathBuf, identity: Identity, policy: MemoryTender) -> io::Result<Self> {
        if identity.fingerprint() != policy.host() {
            return Err(io::Error::other("policy host does not match instance key"));
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let registrations = Arc::new(Mutex::new(BTreeMap::new()));
        let authority = policy.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let policy = authority.clone();
                        let identity = identity.clone();
                        let registrations = registrations.clone();
                        connections.spawn(async move {
                            let _ = serve(stream, identity, policy, registrations).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { policy, task, path })
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
        self.policy.restart();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve(mut stream: UnixStream, identity: Identity, policy: MemoryTender, registrations: Registrations) -> io::Result<()> {
    let (session, request) = timeout(OPEN_TIMEOUT, async {
        let session = server_handshake(&mut stream, &identity).await?;
        let request = read_control(&mut stream).await?;
        Ok::<_, io::Error>((session, request))
    })
    .await
    .map_err(io::Error::other)??;
    let response = match request {
        Request::Publish(request) => match policy.publish(&session, request).await {
            Ok(published) => {
                let lease = published.lease;
                registrations.lock().expect("registrations lock").insert(
                    (lease.id.0, lease.generation),
                    Arc::new(Registration { lease: lease.clone(), incoming: AsyncMutex::new(published.incoming) }),
                );
                Response::Lease(lease)
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
                    let mut incoming = registration.incoming.lock().await;
                    // A cancelled accept cannot keep a pending open alive.
                    let mut byte = [0];
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
    };
    write_control(&mut stream, &response).await
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
        format!("StreamLocalBindUnlink={}", match existing_socket {
            ExistingSocket::Unlink => "yes",
            ExistingSocket::Refuse => "no",
        }),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        destination.into(),
    ]);
    args
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
    directory: tempfile::TempDir,
    socket: PathBuf,
}

impl Forward {
    pub async fn start(destination: &str, remote: &Path, ssh_options: &[String]) -> io::Result<Self> {
        if destination.starts_with('-') || destination.is_empty() {
            return Err(io::Error::other("invalid SSH destination"));
        }
        let directory = tempfile::Builder::new().prefix("tender-").tempdir()?;
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
        Ok(Self { child, directory, socket })
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
        // Keep the private directory alive until the child has been killed.
        let _ = self.directory.path();
    }
}

/// Remote Tender operations over an established SSH forward. Credentials are
/// explicit; accepting an arbitrary Session never lets a caller forge a key.
type Exposures = Arc<Mutex<BTreeMap<(Fingerprint, PublicationId), Arc<LocalExposure>>>>;

#[derive(Clone)]
pub struct SshTender {
    route: Arc<Mutex<PathBuf>>,
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

impl Drop for LocalExposure {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SshTender {
    pub fn new(route: PathBuf, pin: Fingerprint, identities: Vec<Identity>) -> io::Result<Self> {
        Ok(Self {
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
            route: Arc::new(Mutex::new(route)),
            credentials: Arc::new(identities.into_iter().map(|identity| (identity.fingerprint(), identity)).collect()),
            pin,
            exposures: Arc::new(Mutex::new(BTreeMap::new())),
            directory: Arc::new(tempfile::Builder::new().prefix("tender-exposures-").tempdir()?),
        })
    }

    /// Recovery affects only fresh connections. Existing sockets are never
    /// transferred to this route and no buffered application bytes are replayed.
    pub fn replace_route(&self, route: PathBuf) {
        *self.route.lock().expect("route lock") = route;
    }

    async fn request(&self, session: &Session, request: Request) -> Result<UnixStream, Error> {
        if session.pinned_host != self.pin {
            return Err(Error::IdentityMismatch);
        }
        if session.via.is_some() {
            return Err(Error::UntrustedIntermediary);
        }
        let identity = self.credentials.get(&session.caller).ok_or(Error::Denied)?;
        let route = self.route.lock().expect("route lock").clone();
        timeout(OPEN_TIMEOUT, async {
            let mut stream = UnixStream::connect(route).await.map_err(|_| Error::Unavailable)?;
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

#[async_trait]
impl Tender for SshTender {
    async fn publish(&self, session: &Session, request: PublishRequest) -> Result<Published, Error> {
        let Response::Lease(lease) = self.reply(session, Request::Publish(request)).await? else { return Err(Error::Unavailable) };
        let (sender, incoming) = mpsc::unbounded_channel();
        let adapter = self.clone();
        let session = session.clone();
        let held_lease = lease.clone();
        tokio::spawn(async move {
            loop {
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
                    stream = accept => {
                        match stream {
                            Ok(stream) => { if sender.send(Box::new(stream) as ByteStream).is_err() { break } }
                            Err(_) => break,
                        }
                    }
                }
            }
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
            Response::Open => Ok(Box::new(stream)),
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
            let task = tokio::spawn(async move {
                let mut health = tokio::time::interval(Duration::from_millis(250));
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
                        _ = health.tick() => {
                            if matches!(adapter.reply(&session, Request::Probe(id)).await, Err(Error::Withdrawn | Error::Denied)) {
                                drop(listener);
                                let _ = std::fs::remove_file(&owned_path);
                                // Expiry forbids new opens, but established streams run to close.
                                while streams.join_next().await.is_some() {}
                                break;
                            }
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
