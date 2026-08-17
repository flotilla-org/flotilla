mod client_connection;
pub mod environment_sockets;
mod peer_connection;
mod peer_runtime;
mod remote_commands;
mod replicator;
mod request_dispatch;
mod resource_http;
mod shared;
#[cfg(feature = "test-support")]
pub mod test_support;

use std::{
    collections::HashMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use flotilla_core::{
    agents::SharedAgentStateStore, config::ConfigStore, in_process::InProcessDaemon, providers::discovery::DiscoveryRuntime,
};
use flotilla_protocol::{
    ConfigLabel, ConnectionRole, EnvironmentId, GoodbyeReason, HostName, Message, NodeId, PROTOCOL_FINGERPRINT, PROTOCOL_VERSION,
};
use flotilla_resources::{ResourceBackend, SqliteBackend};
use flotilla_transport::message::{unix_message_session_with_prefix, MessageSession};
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
    task::JoinSet,
};
use tracing::{error, info, warn};

use self::{
    client_connection::ClientConnection,
    environment_sockets::EnvironmentSocketRegistry,
    peer_connection::PeerConnection,
    peer_runtime::PeerRuntime,
    remote_commands::{ForwardedCommandMap, PendingRemoteCancelMap, PendingRemoteCommandMap, RemoteCommandRouter},
    shared::{sync_peer_query_state, SocketPeerSender},
};
use crate::{
    peer::{ConnectionDirection, ConnectionMeta, InboundPeerEnvelope, PeerManager, SshTransport, SshTransportPaths},
    DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH,
};

const CONNECTION_PREFACE_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_ERROR_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(5);

struct BoundSocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl BoundSocketGuard {
    fn new(path: PathBuf) -> Result<Self, String> {
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| format!("failed to identify bound socket {}: {error}", path.display()))?;
        Ok(Self { path, device: metadata.dev(), inode: metadata.ino() })
    }
}

impl Drop for BoundSocketGuard {
    fn drop(&mut self) {
        let still_owned = std::fs::symlink_metadata(&self.path)
            .map(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
            .unwrap_or(false);
        if still_owned {
            if let Err(error) = std::fs::remove_file(&self.path) {
                warn!(path = %self.path.display(), %error, "failed to remove owned socket file on shutdown");
            }
        } else {
            warn!(path = %self.path.display(), "daemon socket path no longer names this server's socket; leaving it untouched");
        }
    }
}

fn is_reverse_peer_resource_socket_name(name: &str) -> bool {
    name.strip_prefix(".peer-").is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn cleanup_reverse_peer_resource_sockets(socket_dir: &Path) -> usize {
    let entries = match std::fs::read_dir(socket_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            warn!(socket_dir = %socket_dir.display(), %error, "failed to inspect daemon socket directory for stale reverse peer sockets");
            return 0;
        }
    };

    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(socket_dir = %socket_dir.display(), %error, "failed to inspect daemon socket directory entry during stale reverse peer socket cleanup");
                continue;
            }
        };
        if !is_reverse_peer_resource_socket_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warn!(path = %entry.path().display(), %error, "failed to inspect stale reverse peer socket during startup cleanup");
                continue;
            }
        };
        if file_type.is_dir() {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(path = %entry.path().display(), %error, "failed to remove stale reverse peer socket during startup cleanup");
            }
        }
    }
    removed
}

#[derive(Debug)]
struct AcceptErrorBackoff {
    next: Duration,
}

impl Default for AcceptErrorBackoff {
    fn default() -> Self {
        Self { next: ACCEPT_ERROR_INITIAL_BACKOFF }
    }
}

impl AcceptErrorBackoff {
    fn reset(&mut self) {
        self.next = ACCEPT_ERROR_INITIAL_BACKOFF;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(ACCEPT_ERROR_MAX_BACKOFF);
        delay
    }
}

/// Notification sent from connection sites to the outbound task when a
/// peer connects or reconnects. The outbound task responds by sending
/// current local state for all repos to the specific peer.
///
/// Visibility is promoted to `pub` with the `test-support` feature so
/// integration tests can construct notices to drive the outbound task.
#[cfg_attr(feature = "test-support", visibility::make(pub))]
pub(crate) struct PeerConnectedNotice {
    pub peer: NodeId,
    pub generation: u64,
    pub resource_socket_path: Option<PathBuf>,
}

/// Lifecycle event sent from connection sites to the outbound task.
///
/// `Connected` drives outbound state sync and starts resource replicators
/// for the peer's generation. `Disconnected` is sent only from a
/// connection-owning task that has no retry loop of its own (inbound
/// `PeerConnection::run`, or the outbound per-target task's terminal
/// shutdown branch) — never from a transient, still-retrying disconnect —
/// so the outbound task can cancel and drop that peer's resource
/// replicators. The generation is checked against the currently tracked
/// generation before acting, so a stale/displaced connection's belated
/// teardown can't cancel a newer, already-reconnected generation.
#[cfg_attr(feature = "test-support", visibility::make(pub))]
pub(crate) enum PeerConnectionEvent {
    Connected(PeerConnectedNotice),
    Disconnected { peer: NodeId, generation: u64 },
}

fn build_remote_command_router(daemon: &Arc<InProcessDaemon>, peer_manager: &Arc<Mutex<PeerManager>>) -> RemoteCommandRouter {
    let pending_remote_commands: PendingRemoteCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_commands: ForwardedCommandMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_remote_cancels: PendingRemoteCancelMap = Arc::new(Mutex::new(HashMap::new()));
    RemoteCommandRouter::new(
        Arc::clone(daemon),
        Arc::clone(peer_manager),
        pending_remote_commands,
        forwarded_commands,
        pending_remote_cancels,
        Arc::new(AtomicU64::new(1 << 62)),
    )
}

fn build_peer_manager(
    daemon: &Arc<InProcessDaemon>,
    config: &ConfigStore,
    local_daemon_socket_path: &Path,
) -> Result<Arc<Mutex<PeerManager>>, String> {
    let host_name = daemon.host_name().clone();
    let local_node_id = daemon.node_id().clone();
    let hosts_config = config.load_hosts()?;

    let peer_count = hosts_config.hosts.len();
    let mut peer_manager = PeerManager::new(local_node_id.clone());
    let command_runner = daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
    for (name, host_config) in hosts_config.hosts {
        let expected_host_name = HostName::new(&host_config.expected_host_name);
        let expected_node_id = host_config.expected_node_id.clone();
        match SshTransport::new(
            local_node_id.clone(),
            host_name.to_string(),
            ConfigLabel(name.clone()),
            host_config,
            expected_node_id.clone(),
            daemon.session_id(),
            Arc::clone(&command_runner),
            SshTransportPaths { state_dir: config.state_dir().as_path(), daemon_socket: local_daemon_socket_path },
        ) {
            Ok(transport) => {
                peer_manager.add_configured_target(ConfigLabel(name), expected_host_name, expected_node_id, Box::new(transport));
            }
            Err(e) => {
                warn!(host = %name, err = %e, "skipping peer with invalid host name");
            }
        }
    }

    info!(host = %host_name, %peer_count, "initialized PeerManager");

    Ok(Arc::new(Mutex::new(peer_manager)))
}

async fn build_embedded_resource_backend(config: &ConfigStore) -> Result<ResourceBackend, String> {
    let path = config.state_dir().as_path().join("resources.sqlite");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("create embedded resource store directory {}: {err}", parent.display()))?;
    }
    SqliteBackend::open_async(&path)
        .await
        .map(ResourceBackend::Sqlite)
        .map_err(|err| format!("open embedded resource store {}: {err}", path.display()))
}

pub fn spawn_embedded_peer_networking(daemon: Arc<InProcessDaemon>, config: &ConfigStore) -> Result<tokio::task::JoinHandle<()>, String> {
    let local_daemon_socket_path = flotilla_core::path_policy::daemon_socket_path(config.base_path().as_path());
    let peer_manager = build_peer_manager(&daemon, config, local_daemon_socket_path.as_path())?;
    {
        let daemon = Arc::clone(&daemon);
        let peer_manager = Arc::clone(&peer_manager);
        tokio::spawn(async move {
            sync_peer_query_state(&peer_manager, &daemon).await;
        });
    }
    let (inbound_peer_tx, inbound_peer_rx) = mpsc::channel(256);
    let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
    {
        let router = remote_command_router.clone();
        tokio::spawn(async move { router.resume_pending_crew_completions().await });
    }
    let (handle, _peer_connected_tx) = spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        Some(inbound_peer_rx),
        inbound_peer_tx,
        remote_command_router,
        Some(config.state_dir().as_path().join("peers")),
    );
    Ok(handle)
}

/// Spawn the peer networking runtime with pre-built components.
///
/// Test-only entry point: callers provide a PeerManager with pre-configured
/// senders (e.g. CapturePeerSender). Passes `None` for `inbound_peer_rx` to skip
/// the inbound connection task — tests drive the outbound task via the returned
/// `PeerConnectionEvent` sender.
#[cfg(feature = "test-support")]
pub fn spawn_test_peer_networking(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) {
    // Receiver dropped intentionally — None is passed for the inbound task,
    // so no messages are forwarded; the sender satisfies the runtime signature.
    let (inbound_peer_tx, _inbound_peer_rx) = mpsc::channel(256);
    let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
    spawn_peer_networking_runtime(
        daemon,
        peer_manager,
        None, // No inbound task — test drives outbound via PeerConnectionEvent
        inbound_peer_tx,
        remote_command_router,
        None,
    )
}

/// The daemon server that listens on a Unix socket and dispatches requests
/// to an `InProcessDaemon`.
#[derive(bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::server)"))]
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    socket_discovery_path: PathBuf,
    idle_timeout: Duration,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    shutdown_request_rx: Option<mpsc::UnboundedReceiver<()>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Channel for inbound peer wire messages tagged with connection authority.
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    inbound_peer_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    /// Manages live connections and routing between remote peer hosts.
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    peer_resource_socket_dir: PathBuf,
    agent_state_store: SharedAgentStateStore,
    /// Registry of per-environment Unix sockets. Initialized on startup and
    /// populated when environments are created (wired in Phase D).
    pub environment_sockets: Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `config` — daemon configuration store, used for hostname and peer config.
    /// `discovery` — discovery runtime used to initialize tracked repos.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last active connection disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let socket_discovery_path = config.base_path().as_path().join(DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH);
        Self::new_with_socket_discovery_path(repo_paths, config, discovery, socket_path, socket_discovery_path, idle_timeout).await
    }

    pub async fn new_with_socket_discovery_path(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        socket_path: PathBuf,
        socket_discovery_path: PathBuf,
        idle_timeout: Duration,
    ) -> Result<Self, String> {
        let daemon_config = config.load_daemon_config()?;
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);
        let resource_backend = build_embedded_resource_backend(&config).await?;
        let daemon =
            InProcessDaemon::new_with_resource_backend(repo_paths, Arc::clone(&config), discovery, host_name.clone(), resource_backend)
                .await;
        let peer_manager = build_peer_manager(&daemon, &config, &socket_path)?;
        sync_peer_query_state(&peer_manager, &daemon).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (shutdown_request_tx, shutdown_request_rx) = mpsc::unbounded_channel();
        let (inbound_peer_tx, inbound_peer_rx) = mpsc::channel(256);

        let agent_state_store = Arc::clone(daemon.agent_state_store());
        let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
        let peer_resource_socket_dir = config.state_dir().as_path().join("peers");
        Ok(Self {
            daemon,
            socket_path,
            socket_discovery_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_request_tx,
            shutdown_request_rx: Some(shutdown_request_rx),
            shutdown_tx,
            shutdown_rx,
            inbound_peer_tx,
            inbound_peer_rx: Some(inbound_peer_rx),
            peer_manager,
            remote_command_router,
            peer_resource_socket_dir,
            agent_state_store,
            environment_sockets: Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        })
    }

    /// Take the receiver for inbound peer wire messages.
    ///
    /// Returns `Some` on the first call, `None` thereafter. The PeerManager
    /// consumes this to process data arriving from peer daemons.
    pub fn take_inbound_peer_rx(&mut self) -> Option<mpsc::Receiver<InboundPeerEnvelope>> {
        self.inbound_peer_rx.take()
    }

    pub fn daemon(&self) -> Arc<InProcessDaemon> {
        Arc::clone(&self.daemon)
    }

    /// Run the server, accepting connections until idle timeout or shutdown signal.
    pub async fn run(mut self) -> Result<(), String> {
        // Clean up stale socket file before binding
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).map_err(|e| format!("failed to remove stale socket: {e}"))?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create socket directory: {e}"))?;
            let removed = cleanup_reverse_peer_resource_sockets(parent);
            if removed > 0 {
                info!(socket_dir = %parent.display(), removed, "removed stale reverse peer sockets");
            }
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| format!("failed to bind socket: {e}"))?;
        let _socket_guard = BoundSocketGuard::new(self.socket_path.clone())?;

        publish_socket_path(&self.socket_discovery_path, &self.socket_path)?;

        info!(path = %self.socket_path.display(), "daemon listening");

        // Tell the InProcessDaemon where the socket is so terminal sessions
        // can get FLOTILLA_DAEMON_SOCKET injected.
        self.daemon.set_daemon_socket_path(self.socket_path.clone()).await;

        // Take the inbound receiver before destructuring self.
        let inbound_peer_rx = self.take_inbound_peer_rx();

        let daemon = self.daemon;
        let client_count = self.client_count;
        let shutdown_tx = self.shutdown_tx;
        let mut shutdown_rx = self.shutdown_rx;
        let shutdown_request_tx = self.shutdown_request_tx;
        let mut shutdown_request_rx = self.shutdown_request_rx.take().expect("shutdown request receiver available once");
        let idle_timeout = self.idle_timeout;
        let client_notify = self.client_notify;
        let inbound_peer_tx = self.inbound_peer_tx;
        let agent_state_store = self.agent_state_store;
        let remote_command_router = self.remote_command_router;
        let peer_resource_socket_dir = self.peer_resource_socket_dir;

        remote_command_router.resume_pending_crew_completions().await;

        let idle_client_count = Arc::clone(&client_count);
        let idle_shutdown_request_tx = shutdown_request_tx.clone();
        let idle_notify = Arc::clone(&client_notify);
        tokio::spawn(async move {
            loop {
                while idle_client_count.load(Ordering::SeqCst) != 0 {
                    idle_notify.notified().await;
                }
                info!(timeout_secs = idle_timeout.as_secs(), "no active connections, waiting before shutdown");
                tokio::select! {
                    () = tokio::time::sleep(idle_timeout) => {
                        if idle_client_count.load(Ordering::SeqCst) == 0 {
                            info!("idle timeout reached, shutting down");
                            let _ = idle_shutdown_request_tx.send(());
                            return;
                        }
                    }
                    () = idle_notify.notified() => {}
                }
            }
        });

        let peer_manager = self.peer_manager;
        let (peer_runtime_handle, peer_connected_tx) = spawn_peer_networking_runtime(
            Arc::clone(&daemon),
            Arc::clone(&peer_manager),
            inbound_peer_rx,
            inbound_peer_tx.clone(),
            remote_command_router.clone(),
            Some(peer_resource_socket_dir),
        );

        // SIGTERM handler
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to register SIGTERM handler");

        // Accept loop
        let mut accept_error_backoff = AcceptErrorBackoff::default();
        let mut accept_retry_at = None;
        let mut connection_tasks = JoinSet::new();
        loop {
            tokio::select! {
                accept_result = listener.accept(), if accept_retry_at.is_none() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            accept_error_backoff.reset();
                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();
                            let inbound_peer_tx = inbound_peer_tx.clone();
                            let peer_manager = Arc::clone(&peer_manager);
                            let remote_command_router = remote_command_router.clone();
                            let peer_connected_tx = peer_connected_tx.clone();
                            let agent_state_store = Arc::clone(&agent_state_store);

                            let shutdown_request_tx = shutdown_request_tx.clone();
                            connection_tasks.spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_request_tx,
                                    shutdown_rx,
                                    inbound_peer_tx,
                                    peer_manager,
                                    remote_command_router,
                                    client_count,
                                    client_notify,
                                    peer_connected_tx,
                                    agent_state_store,
                                    None,
                                )
                                .await;
                            });
                        }
                        Err(e) => {
                            let delay = accept_error_backoff.next_delay();
                            error!(
                                err = %e,
                                delay_ms = delay.as_millis() as u64,
                                fd_exhausted = e.raw_os_error().is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE),
                                "failed to accept connection; backing off"
                            );
                            accept_retry_at = Some(tokio::time::Instant::now() + delay);
                        }
                    }
                }
                () = async {
                    match accept_retry_at {
                        Some(retry_at) => tokio::time::sleep_until(retry_at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    accept_retry_at = None;
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("shutdown signal received");
                        break;
                    }
                }
                Some(()) = shutdown_request_rx.recv() => {
                    info!("graceful shutdown request received");
                    break;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("received SIGINT — shutting down");
                    break;
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM — shutting down");
                    break;
                }
            }
        }

        // Stop peer reconnect loops deliberately and tell every connected peer
        // why the transport is closing before connection tasks see shutdown.
        let peer_senders = peer_manager.lock().await.active_peer_senders();
        for (peer, sender) in peer_senders {
            if let Err(error) = sender.retire(GoodbyeReason::Shutdown).await {
                warn!(%peer, %error, "failed to send shutdown goodbye to peer");
            }
        }
        peer_manager.lock().await.disconnect_all().await;
        peer_runtime_handle.abort();
        let _ = peer_runtime_handle.await;

        // SIGINT/SIGTERM enter the common path without having touched the
        // channel. Broadcasting here closes client, peer, handshake, and HTTP
        // connection handlers after any request they are currently serving.
        let _ = shutdown_tx.send(true);
        while let Some(result) = connection_tasks.join_next().await {
            if let Err(error) = result {
                warn!(%error, "daemon connection task failed while draining");
            }
        }

        info!("daemon server stopped");
        Ok(())
    }
}

fn publish_socket_path(discovery_path: &Path, socket_path: &Path) -> Result<(), String> {
    let parent =
        discovery_path.parent().ok_or_else(|| format!("daemon socket discovery path has no parent: {}", discovery_path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("failed to create daemon socket discovery directory: {error}"))?;
    let temporary_path = parent.join(format!(".socket-path-{}.tmp", uuid::Uuid::new_v4()));
    let advertised_path = socket_path.strip_prefix(parent).unwrap_or(socket_path);
    std::fs::write(&temporary_path, format!("{}\n", advertised_path.display()))
        .map_err(|error| format!("failed to write daemon socket discovery file at {}: {error}", temporary_path.display()))?;
    if let Err(error) = std::fs::rename(&temporary_path, discovery_path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(format!("failed to publish daemon socket path at {}: {error}", discovery_path.display()));
    }
    Ok(())
}

fn spawn_peer_networking_runtime(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
    inbound_peer_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    remote_command_router: RemoteCommandRouter,
    resource_socket_dir: Option<PathBuf>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) {
    PeerRuntime::new(daemon, peer_manager, inbound_peer_rx, inbound_peer_tx, remote_command_router, resource_socket_dir).spawn()
}

/// Handle a single client connection.
///
/// `environment_context` — when `Some(id)`, this connection was accepted on a
/// per-environment socket; if the Hello message carries a mismatched
/// `environment_id` the connection is dropped.  `None` means the main socket
/// (forward-compatible with HTTP transport).
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    mut stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    mut shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    let mut first_byte = [0_u8; 1];
    match tokio::time::timeout(CONNECTION_PREFACE_TIMEOUT, tokio::io::AsyncReadExt::read_exact(&mut stream, &mut first_byte)).await {
        Err(_) => {
            warn!(timeout_secs = CONNECTION_PREFACE_TIMEOUT.as_secs(), "connection timed out before sending a preface");
            return;
        }
        Ok(Ok(_)) if first_byte[0].is_ascii_uppercase() => {
            tokio::select! {
                result = resource_http::serve_resource_http(stream, first_byte[0], daemon.resource_backend().clone()) => {
                    if let Err(error) = result {
                        warn!(%error, "resource HTTP connection failed");
                    }
                }
                _ = shutdown_rx.changed() => {}
            }
            return;
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            warn!(%error, "failed to read connection preface");
            return;
        }
    }
    handle_client_session(
        unix_message_session_with_prefix(stream, first_byte.to_vec()),
        daemon,
        shutdown_request_tx,
        shutdown_rx,
        inbound_peer_tx,
        peer_manager,
        remote_command_router,
        client_count,
        client_notify,
        peer_connected_tx,
        agent_state_store,
        environment_context,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_session(
    session: MessageSession,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    mut shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    let session = Arc::new(session);
    let first_msg = tokio::select! {
        message_result = tokio::time::timeout(HELLO_HANDSHAKE_TIMEOUT, session.read()) => {
            match message_result {
                Ok(Ok(msg)) => msg,
                Ok(Err(e)) => {
                    error!(err = %e, "error reading first message from client");
                    None
                }
                Err(_) => {
                    warn!(timeout_secs = HELLO_HANDSHAKE_TIMEOUT.as_secs(), "connection timed out before completing Hello handshake");
                    None
                }
            }
        }
        _ = shutdown_rx.changed() => None,
    };

    const BUILD_ID: &str = env!("FLOTILLA_BUILD_ID");

    let Some(first_msg) = first_msg else {
        return;
    };

    match first_msg {
        Message::Hello { protocol_version, node_id, display_name, session_id, connection_role, surface } => {
            if environment_context.is_some() {
                warn!("peer/client hello on per-environment socket is unsupported");
                return;
            }

            if connection_role == Some(ConnectionRole::Client) {
                // Stateful client handshake: reply with server Hello, then enter
                // stateful client loop (session_id used for cursor ownership).
                // The reply is sent even on a generation/version mismatch so
                // the client can report which two binaries disagreed.
                if session
                    .write(Message::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        node_id: daemon.node_id().clone(),
                        display_name: flotilla_protocol::hello_display_name(daemon.host_name().as_str(), BUILD_ID, PROTOCOL_FINGERPRINT),
                        session_id: daemon.session_id(),
                        connection_role: Some(ConnectionRole::Client),
                        surface: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                let client_info = flotilla_protocol::hello_build_info(&display_name);
                let client_build = client_info.map_or("unknown", |info| info.build_id);
                let client_fingerprint = client_info.map_or("unknown", |info| info.protocol_fingerprint);
                if protocol_version != PROTOCOL_VERSION {
                    warn!(expected = PROTOCOL_VERSION, got = protocol_version, %node_id, "rejecting client with protocol version mismatch");
                    return;
                }
                if client_fingerprint != PROTOCOL_FINGERPRINT {
                    warn!(
                        expected_fingerprint = PROTOCOL_FINGERPRINT,
                        got_fingerprint = client_fingerprint,
                        expected_build = BUILD_ID,
                        got_build = client_build,
                        %node_id,
                        "restricting client with protocol fingerprint mismatch to shutdown"
                    );
                    run_shutdown_only_session(&session, &shutdown_request_tx, &mut shutdown_rx).await;
                    return;
                }
                let surface = match surface {
                    Some(surface) => surface,
                    None => flotilla_protocol::SurfaceDeclaration::focal_for_namespace(daemon.provisioning_namespace().await),
                };
                ClientConnection::new(
                    daemon,
                    shutdown_request_tx,
                    shutdown_rx,
                    remote_command_router,
                    client_count,
                    client_notify,
                    agent_state_store,
                )
                .run_stateful(Arc::clone(&session), session_id, surface)
                .await;
            } else {
                // Peer path (ConnectionRole::Peer or None) — existing behavior.
                PeerConnection::new(daemon, shutdown_rx, inbound_peer_tx, peer_manager, peer_connected_tx, client_count, client_notify)
                    .run(session, protocol_version, node_id, display_name, session_id)
                    .await;
            }
        }
        other => {
            warn!(msg = ?other, "rejecting connection without Hello handshake");
        }
    }
}

/// Serve the stable deployment seam available to same-protocol clients from a
/// different build. No other request crosses the wire-generation gate.
async fn run_shutdown_only_session(
    session: &MessageSession,
    shutdown_request_tx: &mpsc::UnboundedSender<()>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    tokio::select! {
        message = session.read() => {
            match message {
                Ok(Some(Message::Request { id, request: flotilla_protocol::Request::Shutdown })) => {
                    if session.write(Message::ok_response(id, flotilla_protocol::Response::Shutdown)).await.is_ok() {
                        info!("graceful shutdown requested by same-version client with a different protocol fingerprint");
                        let _ = shutdown_request_tx.send(());
                    }
                }
                Ok(Some(Message::Request { id, .. })) => {
                    let _ = session
                        .write(Message::error_response(
                            id,
                            "protocol fingerprint mismatch: only daemon shutdown is available",
                        ))
                        .await;
                }
                Ok(Some(other)) => warn!(msg = ?other, "unexpected message from shutdown-only client"),
                Ok(None) => {}
                Err(error) => warn!(%error, "failed to read request from shutdown-only client"),
            }
        }
        _ = shutdown_rx.changed() => {}
    }
}

#[cfg(test)]
mod tests;
