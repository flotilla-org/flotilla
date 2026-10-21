use flotilla_store_sqlite::SqliteBackend;
mod caller;
pub mod change_stream;
mod client_connection;
mod environment_connection;
pub mod environment_sockets;
mod environment_supervision;
mod http;
mod peer_connection;
pub mod peer_runtime;
pub mod remote_commands;
mod replication;
mod request_dispatch;
mod shared;
mod tables;

pub use http::{HookEndpoint, HookLaunch};

use std::{
    collections::HashMap,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use flotilla_agent::SharedAgentStateStore;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_protocol::{
    ConfigLabel, ConnectionRole, EnvironmentId, GoodbyeReason, HostName, Message, NodeId, PROTOCOL_FINGERPRINT, PROTOCOL_VERSION,
};

use flotilla_store::ResourceBackend;
use flotilla_transport::message::{unix_message_session_with_prefix, MessageSession};
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};

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
    startup::phase,
    DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH,
};

const CONNECTION_PREFACE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long HTTP connections may finish once shutdown begins. Watch and change-stream
/// responses never end on their own, so they are cut at this deadline.
const HTTP_DRAIN_DEADLINE: Duration = Duration::from_secs(2);
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
/// Public construction lets dev-only testkits and integration tests drive
/// the outbound task.
#[visibility::make(pub)]
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
#[visibility::make(pub)]
pub(crate) enum PeerConnectionEvent {
    Connected(PeerConnectedNotice),
    Disconnected { peer: NodeId, generation: u64 },
}

pub fn build_remote_command_router(daemon: &Arc<InProcessDaemon>, peer_manager: &Arc<Mutex<PeerManager>>) -> RemoteCommandRouter {
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

    let peer_count = hosts_config.hosts.values().filter(|host| !host.agentless_ssh).count();
    let mut peer_manager = PeerManager::new(local_node_id.clone());
    let command_runner = daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
    for (name, host_config) in hosts_config.hosts {
        if host_config.agentless_ssh {
            continue;
        }
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

async fn build_embedded_resource_backend(
    config: &ConfigStore,
) -> Result<(ResourceBackend, Arc<flotilla_store::load_metrics::StoreLoadMetrics>), String> {
    let path = config.state_dir().as_path().join("resources.sqlite");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("create embedded resource store directory {}: {err}", parent.display()))?;
    }
    SqliteBackend::open_async(&path)
        .await
        .map(|store| {
            let metrics = store.load_metrics();
            (ResourceBackend::new(store), metrics)
        })
        .map_err(|err| format!("open embedded resource store {}: {err}", path.display()))
}

pub fn spawn_embedded_peer_networking(daemon: Arc<InProcessDaemon>, config: &ConfigStore) -> Result<tokio::task::JoinHandle<()>, String> {
    let local_daemon_socket_path = flotilla_paths::path_policy::daemon_socket_path(config.base_path().as_path());
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

/// A declared loopback HTTP listen address, resolved and checked at startup (ADR 0065).
pub struct HttpListenAddress(http::ListenAddress);

impl HttpListenAddress {
    /// Resolve `host:port` without blocking, refusing unless every address is loopback.
    pub async fn resolve(spec: &str) -> Result<Self, String> {
        http::ListenAddress::resolve(spec).await.map(Self)
    }
}

/// Resolves once the daemon begins shutting down (or its shutdown sender is gone).
async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopping| *stopping).await;
}

/// Serve one HTTP connection or listener until it ends, or until shutdown plus
/// [`HTTP_DRAIN_DEADLINE`]: rigging drains in-flight requests, and this bounds the drain.
async fn serve_until_drained(serving: impl std::future::Future<Output = std::io::Result<()>>, shutdown: watch::Receiver<bool>, what: &str) {
    tokio::select! {
        result = serving => if let Err(error) = result {
            warn!(%error, "{what} HTTP serving failed");
        },
        () = async {
            shutdown_signal(shutdown).await;
            tokio::time::sleep(HTTP_DRAIN_DEADLINE).await;
        } => {}
    }
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
    startup_ready: watch::Sender<bool>,
    /// Channel for inbound peer wire messages tagged with connection authority.
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    inbound_peer_rx: Option<mpsc::Receiver<InboundPeerEnvelope>>,
    /// Manages live connections and routing between remote peer hosts.
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    peer_resource_socket_dir: PathBuf,
    agent_state_store: SharedAgentStateStore,
    /// Registry of per-environment sockets, populated by provisioning/adoption
    /// and retired by teardown through EnvironmentSocketService.
    pub environment_sockets: Arc<tokio::sync::Mutex<EnvironmentSocketRegistry>>,
    /// The one HTTP router (ADR 0065), served on the main socket, environment sockets and
    /// the optional loopback TCP listener.
    http_router: axum::Router,
    load_metrics: Arc<http::metrics::LoadMetrics>,
    logins: Arc<http::LoginStore>,
    tcp_origin: http::ListenerOrigin,
    hook_endpoint: http::HookEndpoint,
    http_listen: Option<http::ListenAddress>,
    /// Where the TCP listener's token file lives.
    state_dir: PathBuf,
}

struct ClientActivityLease {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientActivityLease {
    fn acquire(count: Arc<AtomicUsize>, notify: Arc<Notify>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        notify.notify_one();
        Self { count, notify }
    }
}

impl Drop for ClientActivityLease {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

impl DaemonServer {
    pub fn environment_socket_service(&self) -> environment_sockets::EnvironmentSocketService {
        let daemon = self.daemon.clone();
        let router = self.remote_command_router.clone();
        let client_count = self.client_count.clone();
        let client_notify = self.client_notify.clone();
        let shutdown = self.shutdown_rx.clone();
        let http_router = self.http_router.clone();
        let directory = flotilla_protocol::DaemonHostPath::new(self.socket_path.with_extension("env"));
        environment_sockets::EnvironmentSocketService::new(
            self.environment_sockets.clone(),
            directory,
            move |listener, environment, reach| {
                let daemon = daemon.clone();
                let router = router.clone();
                let http_router = http_router.clone();
                let client_count = client_count.clone();
                let client_notify = client_notify.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut connections = JoinSet::new();
                    loop {
                        tokio::select! {
                            result = listener.accept() => match result {
                                Ok((stream, _)) => {
                                    let peer = caller::reach_evidence(reach, caller::socket_peer_credential(&stream));
                                    let daemon = daemon.clone();
                                    let router = router.clone();
                                    let environment = environment.clone();
                                    let shutdown = shutdown.clone();
                                    let http_router = http_router.clone();
                                    let lease = ClientActivityLease::acquire(client_count.clone(), client_notify.clone());
                                    connections.spawn(async move {
                                        let _lease = lease;
                                        let mut stream = stream;
                                        let Some(first_byte) = read_preface(&mut stream).await else { return };
                                        if http::is_http_preface(first_byte) {
                                            // The socket binds the caller to its environment, as the framed protocol does.
                                            let bound = environment.clone();
                                            let caller = move |kernel_peer| http::CallerAuth::Environment {
                                                environment: bound,
                                                peer: caller::reach_evidence(reach, Some(kernel_peer)),
                                            };
                                            let serving = http::serve_sniffed(stream, first_byte, http_router, caller, shutdown_signal(shutdown.clone()));
                                            serve_until_drained(serving, shutdown, "environment socket").await;
                                            return;
                                        }
                                        let claims = match tokio::task::spawn_blocking(move || caller::claims_from_peer(peer)).await {
                                            Ok(claims) => Some(claims),
                                            Err(error) => { warn!(%error, "environment peer evidence unavailable"); return; }
                                        };
                                        let session = unix_message_session_with_prefix(stream, vec![first_byte]);
                                        environment_connection::serve(session, daemon, router, environment, claims, shutdown).await;
                                    });
                                }
                                Err(error) => { warn!(%error, "environment socket accept failed"); return; }
                            },
                            _ = shutdown.changed() => return,
                            _ = connections.join_next(), if !connections.is_empty() => {},
                        }
                    }
                })
            },
        )
    }

    /// Attach the runtime registry before serving; controller families are absent until installed.
    /// Installing a second registry is a composition error, diagnosed in debug builds.
    pub fn install_controller_metrics(&self, metrics: Arc<flotilla_store::load_metrics::ControllerMetricsRegistry>) {
        let installed = self.load_metrics.controllers.set(metrics).is_ok();
        debug_assert!(installed, "controller metrics registry already installed");
    }

    pub fn install_blob_store(&self, store: Arc<crate::blob_store::TieredBlobStore>) -> Result<(), String> {
        self.remote_command_router.install_blob_store(store)
    }
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
        let (resource_backend, store_metrics) = phase("open_resource_backend", build_embedded_resource_backend(&config)).await?;
        let load_metrics = Arc::new(http::metrics::LoadMetrics::new(store_metrics));
        let daemon = phase(
            "initialize_in_process_daemon",
            InProcessDaemon::new_with_resource_backend(
                repo_paths,
                Arc::clone(&config),
                discovery,
                host_name.clone(),
                resource_backend,
                std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
            ),
        )
        .await;
        let peer_manager = build_peer_manager(&daemon, &config, &socket_path)?;
        phase("sync_peer_query_state", sync_peer_query_state(&peer_manager, &daemon)).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (shutdown_request_tx, shutdown_request_rx) = mpsc::unbounded_channel();
        let (inbound_peer_tx, inbound_peer_rx) = mpsc::channel(256);

        let agent_state_store = Arc::clone(daemon.agent_state_store());
        let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
        let peer_resource_socket_dir = config.state_dir().as_path().join("peers");
        let logins = Arc::new(http::LoginStore::new(daemon.clock_for_scenarios()));
        let tcp_origin = http::ListenerOrigin::default();
        let http_router = http::HttpRouter::for_daemon(Arc::clone(&daemon), Arc::clone(&logins), tcp_origin.clone())
            .with_load_metrics(Arc::clone(&load_metrics), daemon_config.http.metrics)
            .with_commands(remote_command_router.clone(), shutdown_request_tx.clone(), socket_path.clone())
            .into_router();
        let state_dir = config.state_dir().as_path().to_path_buf();
        Ok(Self {
            daemon,
            load_metrics,
            socket_path,
            socket_discovery_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_request_tx,
            shutdown_request_rx: Some(shutdown_request_rx),
            shutdown_tx,
            shutdown_rx,
            startup_ready: watch::channel(false).0,
            inbound_peer_tx,
            inbound_peer_rx: Some(inbound_peer_rx),
            peer_manager,
            remote_command_router,
            peer_resource_socket_dir,
            agent_state_store,
            environment_sockets: Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
            http_router,
            logins,
            tcp_origin,
            hook_endpoint: http::HookEndpoint::default(),
            http_listen: None,
            state_dir,
        })
    }

    /// Declare the loopback TCP listener `run` binds. Listeners come only from startup
    /// configuration; nothing opens one at runtime (ADR 0065).
    pub fn with_http_listener(mut self, address: HttpListenAddress) -> Self {
        self.http_listen = Some(address.0);
        self
    }

    /// Take the receiver for inbound peer wire messages.
    ///
    /// Returns `Some` on the first call, `None` thereafter. The PeerManager
    /// consumes this to process data arriving from peer daemons.
    pub fn take_inbound_peer_rx(&mut self) -> Option<mpsc::Receiver<InboundPeerEnvelope>> {
        self.inbound_peer_rx.take()
    }

    /// The hook endpoint host-direct crew launches deliver Claude Code hooks to, once the
    /// loopback TCP listener binds.
    pub fn hook_endpoint(&self) -> http::HookEndpoint {
        self.hook_endpoint.clone()
    }

    /// Signals only after binding, publishing discovery, and preparing acceptance.
    /// A closed channel with no readiness means the server failed to start.
    pub fn startup_ready(&self) -> watch::Receiver<bool> {
        self.startup_ready.subscribe()
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
        // The resource API and image archives share the local administrative
        // trust boundary. Only this daemon's Unix user may access the socket.
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("failed to protect daemon socket: {error}"))?;

        // A declared TCP listener that cannot bind refuses startup, before discovery
        // advertises this daemon.
        let tcp_listener = match &self.http_listen {
            Some(listen) => {
                let listener = http::TcpListener::bind(listen, &self.state_dir, Arc::clone(&self.logins)).await?;
                self.tcp_origin.set(listener.origin().to_owned());
                self.hook_endpoint.publish(&listener);
                Some(listener)
            }
            None => None,
        };

        publish_socket_path(&self.socket_discovery_path, &self.socket_path)?;

        info!(path = %self.socket_path.display(), "daemon listening");
        let tcp_serving = tcp_listener.map(|listener| {
            if let Ok(address) = listener.local_addr() {
                info!(%address, origin = listener.origin(), "daemon HTTP listening");
            }
            let count = self.client_count.clone();
            let notify = self.client_notify.clone();
            // TCP clients use short requests as well as persistent connections. Count
            // admitted requests and restart the idle interval when each one finishes;
            // a browser session alone must not keep an unused daemon alive forever.
            let router = self.http_router.clone().layer(axum::middleware::from_fn(move |request, next: axum::middleware::Next| {
                let count = count.clone();
                let notify = notify.clone();
                async move {
                    let _lease = ClientActivityLease::acquire(count, notify);
                    next.run(request).await
                }
            }));
            let shutdown = self.shutdown_rx.clone();
            tokio::spawn(async move {
                let serving = listener.serve(router, shutdown_signal(shutdown.clone()));
                serve_until_drained(serving, shutdown, "TCP").await;
            })
        });

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
        let http_router = self.http_router;

        // This only reads local pending records and registers asynchronous retry
        // workers. Keep the snapshot before admission so replay cannot replace a
        // newer client retry with stale completion data.
        phase("resume_pending_crew_completions", remote_command_router.resume_pending_crew_completions()).await;

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
        let (peer_runtime_handle, peer_connected_tx) = PeerRuntime::new(
            Arc::clone(&daemon),
            Arc::clone(&peer_manager),
            inbound_peer_rx,
            inbound_peer_tx.clone(),
            remote_command_router.clone(),
            Some(peer_resource_socket_dir),
        )
        .with_replication_metrics(Arc::clone(&self.load_metrics.replication))
        .spawn();

        // SIGTERM handler
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to register SIGTERM handler");

        // Accept loop
        let mut accept_error_backoff = AcceptErrorBackoff::default();
        let mut accept_retry_at = None;
        let mut connection_tasks = JoinSet::new();
        self.startup_ready.send_replace(true);
        loop {
            tokio::select! {
                accept_result = next_connection_event(async {
                    if accept_retry_at.is_none() {
                        listener.accept().await
                    } else {
                        std::future::pending().await
                    }
                }, &mut connection_tasks) => {
                    let Some(accept_result) = accept_result else { continue };
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
                            let http_router = http_router.clone();

                            let shutdown_request_tx = shutdown_request_tx.clone();
                            connection_tasks.spawn(async move {
                                handle_client_with_caller(
                                    stream,
                                    http_router,
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
        if let Some(serving) = tcp_serving {
            if let Err(error) = serving.await {
                warn!(%error, "daemon HTTP listener task failed while draining");
            }
        }

        info!("daemon server stopped");
        Ok(())
    }
}

/// Read a connection's first byte, which chooses HTTP or the framed protocol. A peer that
/// sends nothing within [`CONNECTION_PREFACE_TIMEOUT`] is dropped.
async fn read_preface(stream: &mut tokio::net::UnixStream) -> Option<u8> {
    let mut first_byte = [0_u8; 1];
    match tokio::time::timeout(CONNECTION_PREFACE_TIMEOUT, tokio::io::AsyncReadExt::read_exact(stream, &mut first_byte)).await {
        Err(_) => {
            warn!(timeout_secs = CONNECTION_PREFACE_TIMEOUT.as_secs(), "connection timed out before sending a preface");
            None
        }
        Ok(Ok(_)) => Some(first_byte[0]),
        // A peer that connects and closes without a byte (a liveness probe) is not a fault.
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            debug!("connection closed before sending a preface");
            None
        }
        Ok(Err(error)) => {
            warn!(%error, "failed to read connection preface");
            None
        }
    }
}

/// Wait for an accept or a completed connection, including during accept backoff.
/// Returning `None` lets the accept loop continue after releasing a task allocation.
async fn next_connection_event<T>(accept: impl std::future::Future<Output = T>, connection_tasks: &mut JoinSet<()>) -> Option<T> {
    tokio::select! {
        result = accept => Some(result),
        result = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
            if let Some(Err(error)) = result {
                warn!(%error, "daemon connection task failed");
            }
            None
        }
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
#[cfg(test)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    let logins = Arc::new(http::LoginStore::new(daemon.clock_for_scenarios()));
    let http_router = http::HttpRouter::for_daemon(Arc::clone(&daemon), logins, http::ListenerOrigin::default()).into_router();
    handle_client_with_caller(
        stream,
        http_router,
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
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_with_caller(
    mut stream: tokio::net::UnixStream,
    http_router: axum::Router,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
    caller: Option<flotilla_protocol::CommandCaller>,
) {
    let Some(first_byte) = read_preface(&mut stream).await else { return };
    if http::is_http_preface(first_byte) {
        // The main socket is a full-trust context (ADR 0065); a per-environment context
        // binds the caller to that environment.
        let caller = move |peer| match environment_context {
            Some(environment) => http::CallerAuth::Environment { environment, peer: Some(peer) },
            None => http::CallerAuth::Socket(Some(peer)),
        };
        let serving = http::serve_sniffed(stream, first_byte, http_router, caller, shutdown_signal(shutdown_rx.clone()));
        serve_until_drained(serving, shutdown_rx, "main socket").await;
        return;
    }
    let peer = caller::socket_peer_credential(&stream);
    let socket_path = stream.local_addr().ok().and_then(|address| address.as_pathname().map(Path::to_path_buf));
    handle_client_session_with_identity(
        unix_message_session_with_prefix(stream, vec![first_byte]),
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
        caller.map_or(CallerIdentity::UnixPeer { peer, socket_path }, |caller| CallerIdentity::Supplied(Some(Box::new(caller)))),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
async fn handle_client_session(
    session: MessageSession,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
) {
    handle_client_session_with_caller(
        session,
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
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_client_session_with_caller(
    session: MessageSession,
    daemon: Arc<InProcessDaemon>,
    shutdown_request_tx: mpsc::UnboundedSender<()>,
    shutdown_rx: watch::Receiver<bool>,
    inbound_peer_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectionEvent>,
    agent_state_store: SharedAgentStateStore,
    environment_context: Option<EnvironmentId>,
    caller: Option<flotilla_protocol::CommandCaller>,
) {
    handle_client_session_with_identity(
        session,
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
        CallerIdentity::Supplied(caller.map(Box::new)),
    )
    .await;
}

fn bind_surface_principal(
    surface: &mut flotilla_protocol::SurfaceDeclaration,
    caller: &flotilla_protocol::CommandCaller,
    transport_bound: bool,
) {
    if transport_bound {
        surface.principal_ref = caller.principal_ref.clone();
    }
}

enum CallerIdentity {
    // In-process callers have already crossed their authentication boundary.
    Supplied(Option<Box<flotilla_protocol::CommandCaller>>),
    UnixPeer { peer: Option<caller::PeerCredential>, socket_path: Option<PathBuf> },
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_session_with_identity(
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
    identity: CallerIdentity,
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

    let build_id = flotilla_daemon_api::build_info::build_id();

    let Some(first_msg) = first_msg else {
        return;
    };

    match first_msg {
        // The main socket derives identity from kernel peer evidence; in-band
        // claims are honoured only by environment sockets.
        Message::Hello { protocol_version, node_id, display_name, session_id, connection_role, surface, crew_claims: _ } => {
            if environment_context.is_some() {
                warn!("peer/client hello on per-environment socket is unsupported");
                return;
            }

            if connection_role == Some(ConnectionRole::Client) {
                // Only framed clients use crew identity. Peer and HTTP authority
                // remain on their existing transport-specific paths.
                let (caller, transport_bound) = match identity {
                    CallerIdentity::UnixPeer { peer, socket_path } => {
                        let namespace = daemon.provisioning_namespace().await;
                        let evidence = tokio::task::spawn_blocking(move || {
                            let claims = caller::claims_from_peer(peer);
                            let caller = caller::caller_from_peer(peer, &namespace)?;
                            let mounts = caller
                                .process
                                .as_ref()
                                .filter(|process| process.container.is_some())
                                .map(|process| caller::peer_mounts(process.pid, socket_path.as_deref()))
                                .transpose()?;
                            Ok::<_, String>((caller, claims, mounts))
                        })
                        .await;
                        let resolved = match evidence {
                            Ok(Ok((evidence, claims, mounts))) => {
                                caller::resolve_main_caller(&daemon, evidence, &claims, mounts.as_ref()).await
                            }
                            Ok(Err(error)) => Err(error),
                            Err(error) => Err(format!("CallerIdentityUnavailable: {error}")),
                        };
                        match resolved {
                            Ok(resolution) => (Some(resolution.caller), resolution.transport_bound),
                            Err(error) => {
                                warn!(%error, "main socket caller refused");
                                let _ = session.write(Message::error_response(0, error)).await;
                                return;
                            }
                        }
                    }
                    CallerIdentity::Supplied(caller) => (caller.map(|caller| *caller), false),
                };
                // Stateful client handshake: reply with server Hello, then enter
                // stateful client loop (session_id used for cursor ownership).
                // The reply is sent even on a generation/version mismatch so
                // the client can report which two binaries disagreed.
                if session
                    .write(Message::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        node_id: daemon.node_id().clone(),
                        display_name: flotilla_protocol::hello_display_name(daemon.host_name().as_str(), build_id, PROTOCOL_FINGERPRINT),
                        session_id: daemon.session_id(),
                        connection_role: Some(ConnectionRole::Client),
                        surface: None,
                        crew_claims: Default::default(),
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
                        expected_build = build_id,
                        got_build = client_build,
                        %node_id,
                        "restricting client with protocol fingerprint mismatch to shutdown"
                    );
                    run_shutdown_only_session(&session, &shutdown_request_tx, &mut shutdown_rx).await;
                    return;
                }
                let mut surface = match surface {
                    Some(surface) => surface,
                    None => flotilla_protocol::SurfaceDeclaration::focal_for_namespace(daemon.provisioning_namespace().await),
                };
                let caller = caller
                    .map(|mut caller| {
                        bind_surface_principal(&mut surface, &caller, transport_bound);
                        caller.principal_ref = surface.principal_ref.clone();
                        caller
                    })
                    .unwrap_or_else(|| flotilla_protocol::CommandCaller {
                        principal_ref: surface.principal_ref.clone(),
                        process: None,
                        crew: None,
                    });
                ClientConnection::new(
                    daemon,
                    shutdown_request_tx,
                    shutdown_rx,
                    remote_command_router,
                    client_count,
                    client_notify,
                    agent_state_store,
                    caller,
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

#[cfg(test)]
pub(crate) use tests::dispatch_request_with_state;
