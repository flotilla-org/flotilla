use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    Command, ConnectionRole, DaemonEvent, LeafFire, Message, NodeId, QueryCursor, QueryId, ReplayCursor, RepoInfo, Request, Response,
    ResponseResult, StatusResponse, StreamKey, SurfaceDeclaration, TopologyResponse, PROTOCOL_FINGERPRINT, PROTOCOL_VERSION,
};
use flotilla_transport::message::{connect_unix_message_session, MessageSession};
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{debug, error, warn};

pub mod launchd;
pub mod reconnect;
pub mod resource;
pub const BUILD_ID: &str = env!("FLOTILLA_BUILD_ID");

/// Std RwLock for local seq tracking — the critical sections are single HashMap
/// operations (no async work while holding the lock), and using a sync lock
/// avoids the race where a spawned seq update hasn't run before the next delta
/// arrives.
type SeqMap = std::sync::RwLock<HashMap<StreamKey, u64>>;

/// Named queries this client is currently subscribed to. Gap recovery
/// re-subscribes with the full set, since `SubscribeQueries` replaces the
/// connection's subscription.
type QuerySet = std::sync::RwLock<HashSet<QueryId>>;

/// RAII guard that holds the daemon spawn flock.
///
/// The lock file remains on disk after release so every contender opens the
/// same inode. Unlinking it during a handoff would let a new client create and
/// lock a replacement inode while an already-queued client owns the old one.
struct SpawnLockGuard {
    _file: std::fs::File,
}

impl SpawnLockGuard {
    fn new(file: std::fs::File) -> Self {
        Self { _file: file }
    }
}

/// Perform the client-side Hello handshake on a `MessageSession`.
///
/// Sends a `Hello` with `ConnectionRole::Client` and a fresh `session_id`,
/// then waits for the server's Hello reply.
async fn do_client_hello(session: &MessageSession) -> Result<Option<String>, String> {
    do_client_hello_with_surface(session, None, WireGenerationPolicy::RequireMatch).await
}

async fn do_client_hello_with_declared_surface(session: &MessageSession, surface: SurfaceDeclaration) -> Result<Option<String>, String> {
    do_client_hello_with_surface(session, Some(surface), WireGenerationPolicy::RequireMatch).await
}

#[derive(Clone, Copy)]
enum WireGenerationPolicy {
    RequireMatch,
    AllowMismatchForShutdown,
}

async fn do_client_hello_with_surface(
    session: &MessageSession,
    surface: Option<SurfaceDeclaration>,
    wire_generation_policy: WireGenerationPolicy,
) -> Result<Option<String>, String> {
    let session_id = uuid::Uuid::new_v4();
    session
        .write(Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: NodeId::new("client"),
            display_name: flotilla_protocol::hello_display_name("client", BUILD_ID, PROTOCOL_FINGERPRINT),
            session_id,
            connection_role: Some(ConnectionRole::Client),
            surface,
        })
        .await
        .map_err(|e| format!("failed to send Hello: {e}"))?;

    match session.read().await.map_err(|e| format!("failed to read Hello reply: {e}"))? {
        Some(Message::Hello { protocol_version, display_name, .. }) if protocol_version != PROTOCOL_VERSION => {
            let daemon_info = flotilla_protocol::hello_build_info(&display_name);
            let daemon_build = daemon_info.map_or("unknown", |info| info.build_id);
            let daemon_fingerprint = daemon_info.map_or("unknown", |info| info.protocol_fingerprint);
            Err(format!(
                "daemon protocol version mismatch: client fingerprint {PROTOCOL_FINGERPRINT} speaks proto {PROTOCOL_VERSION} (build \
                 {BUILD_ID}); daemon fingerprint {daemon_fingerprint} speaks proto {protocol_version} (build {daemon_build})"
            ))
        }
        Some(Message::Hello { display_name, .. }) => {
            let daemon_info = flotilla_protocol::hello_build_info(&display_name);
            let daemon_build = daemon_info.map_or("unknown", |info| info.build_id);
            let daemon_fingerprint = daemon_info.map_or("unknown", |info| info.protocol_fingerprint);
            if daemon_fingerprint != PROTOCOL_FINGERPRINT && matches!(wire_generation_policy, WireGenerationPolicy::RequireMatch) {
                return Err(format!(
                    "wire generation mismatch: client fingerprint {PROTOCOL_FINGERPRINT} speaks proto {PROTOCOL_VERSION} (build \
                     {BUILD_ID}); daemon fingerprint {daemon_fingerprint} speaks proto {PROTOCOL_VERSION} (build {daemon_build})"
                ));
            }
            Ok(Some(daemon_build.to_owned()))
        }
        Some(other) => Err(format!("expected Hello reply, got: {other:?}")),
        None => Err("connection closed before Hello reply".into()),
    }
}

/// Stop an existing daemon through the same-protocol deployment seam.
///
/// Normal client sessions require an identical protocol fingerprint. This path
/// permits a fingerprint mismatch only long enough to send the typed shutdown
/// request and receive its typed acknowledgement. Protocol-version mismatches
/// remain rejected by the Hello handshake.
pub async fn shutdown_existing(socket_path: &Path) -> Result<(), String> {
    let session = connect_unix_message_session(socket_path).await?;
    tokio::time::timeout(
        HELLO_HANDSHAKE_TIMEOUT,
        do_client_hello_with_surface(&session, None, WireGenerationPolicy::AllowMismatchForShutdown),
    )
    .await
    .map_err(|_| {
        format!(
            "daemon at {} accepted the connection but did not complete the Hello handshake within {}s",
            socket_path.display(),
            HELLO_HANDSHAKE_TIMEOUT.as_secs()
        )
    })??;

    const REQUEST_ID: u64 = 1;
    const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(30);
    session.write(Message::Request { id: REQUEST_ID, request: Request::Shutdown }).await?;
    let response = tokio::time::timeout(SHUTDOWN_ACK_TIMEOUT, session.read())
        .await
        .map_err(|_| format!("daemon did not acknowledge shutdown within {}s", SHUTDOWN_ACK_TIMEOUT.as_secs()))??;
    match response {
        Some(Message::Response { id: REQUEST_ID, response }) => match into_success_response(*response)? {
            Response::Shutdown => Ok(()),
            other => Err(format!("unexpected shutdown response: {other:?}")),
        },
        Some(other) => Err(format!("expected shutdown response, got: {other:?}")),
        None => Err("connection closed before shutdown acknowledgement".into()),
    }
}

pub struct SocketDaemon {
    session: Arc<MessageSession>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    event_tx: broadcast::WeakSender<DaemonEvent>,
    wait_event_tx: broadcast::WeakSender<LeafFire>,
    next_id: Arc<AtomicU64>,
    reader_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Local snapshot seq per repo, for gap detection.
    /// Updated by replay_since (seeding) and the background reader (live events).
    local_seqs: Arc<SeqMap>,
    subscribed_queries: Arc<QuerySet>,
    initial_sync_complete: Arc<AtomicBool>,
    daemon_build_id: Option<String>,
}

impl SocketDaemon {
    /// Connect to a running daemon at the given Unix socket path.
    ///
    /// Requires a bounded, stateful Hello handshake before starting the shared
    /// client reader/pending-request machinery. This makes protocol-version
    /// validation part of every real socket connection rather than an opt-in
    /// call-site choice.
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let session = connect_unix_message_session(socket_path).await?;
        from_session_stateful_bounded(socket_path, session, None).await
    }

    pub async fn connect_with_surface(socket_path: &Path, surface: SurfaceDeclaration) -> Result<Arc<Self>, String> {
        let session = connect_unix_message_session(socket_path).await?;
        from_session_stateful_bounded(socket_path, session, Some(&surface)).await
    }

    /// Build a client from an existing `MessageSession`, performing a Hello
    /// handshake with `ConnectionRole::Client` so the server assigns cursor
    /// ownership to our `session_id`.
    pub async fn from_session_stateful(session: MessageSession) -> Result<Arc<Self>, String> {
        let daemon_build_id = do_client_hello(&session).await?;
        Self::from_session_with_build_id(session, daemon_build_id)
    }

    pub async fn from_session_stateful_with_surface(session: MessageSession, surface: SurfaceDeclaration) -> Result<Arc<Self>, String> {
        let daemon_build_id = do_client_hello_with_declared_surface(&session, surface).await?;
        Self::from_session_with_build_id(session, daemon_build_id)
    }

    pub fn from_session(session: MessageSession) -> Result<Arc<Self>, String> {
        Self::from_session_with_build_id(session, None)
    }

    fn from_session_with_build_id(session: MessageSession, daemon_build_id: Option<String>) -> Result<Arc<Self>, String> {
        let session = Arc::new(session);

        let (event_tx, _) = broadcast::channel(256);
        let event_tx_weak = event_tx.downgrade();
        // Wait fires are one-shot and must not be displaced by unrelated
        // daemon traffic on the general event fan-out.
        let (wait_event_tx, _) = broadcast::channel(256);
        let wait_event_tx_weak = wait_event_tx.downgrade();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let subscribed_queries: Arc<QuerySet> = Arc::new(std::sync::RwLock::new(HashSet::new()));
        let initial_sync_complete = Arc::new(AtomicBool::new(false));

        // Spawn background reader task
        let reader_session = Arc::clone(&session);
        let reader_context = EventContext::builder()
            .local_seqs(Arc::clone(&local_seqs))
            .subscribed_queries(Arc::clone(&subscribed_queries))
            .event_tx(event_tx_weak.clone())
            .wait_event_tx(wait_event_tx_weak.clone())
            .session(Arc::clone(&session))
            .pending(Arc::clone(&pending))
            .next_id(Arc::clone(&next_id))
            .build();
        let reader_task = tokio::spawn(async move {
            let _event_channel_guard = event_tx;
            let _wait_event_channel_guard = wait_event_tx;
            loop {
                match reader_session.read().await {
                    Ok(Some(msg)) => match msg {
                        Message::Response { id, response } => {
                            let mut map = reader_context.pending.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let _ = tx.send(*response);
                            } else {
                                warn!(%id, "received response for unknown request id");
                            }
                        }
                        Message::Event { event } => {
                            handle_event(*event, &reader_context);
                        }
                        Message::Request { .. } => {
                            warn!("received unexpected request from daemon");
                        }
                        Message::Hello { .. } => {
                            warn!("received unexpected hello from daemon");
                        }
                        Message::Peer(_) => {
                            warn!("received unexpected peer envelope from daemon");
                        }
                    },
                    Ok(None) => {
                        // EOF — daemon closed connection
                        tracing::info!("daemon connection closed; notifying subscribers");
                        let mut map = reader_context.pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(ResponseResult::Err { message: "daemon connection closed".into() });
                        }
                        break;
                    }
                    Err(e) => {
                        warn!(err = %e, "error reading from daemon session; notifying subscribers");
                        let mut map = reader_context.pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(ResponseResult::Err { message: format!("daemon read error: {e}") });
                        }
                        break;
                    }
                }
            }
        });

        let daemon = Arc::new(Self {
            session,
            pending: Arc::clone(&pending),
            event_tx: event_tx_weak,
            wait_event_tx: wait_event_tx_weak,
            next_id: Arc::clone(&next_id),
            reader_task: std::sync::Mutex::new(Some(reader_task)),
            local_seqs: Arc::clone(&local_seqs),
            subscribed_queries: Arc::clone(&subscribed_queries),
            initial_sync_complete,
            daemon_build_id,
        });

        Ok(daemon)
    }

    /// Deliver a one-shot agent hook through the same generation-checked
    /// connection used by CLI and TUI requests.
    pub async fn send_agent_hook(&self, event: flotilla_protocol::AgentHookEvent) -> Result<(), String> {
        let result = send_request(&self.session, &self.pending, &self.next_id, Request::AgentHook { event }).await?;
        match into_success_response(result)? {
            Response::AgentHook => Ok(()),
            other => Err(format!("unexpected agent hook response: {other:?}")),
        }
    }

    /// Register a connection-owned wait and return a receiver that was opened
    /// before admission, so an immediately true leaf cannot race delivery.
    pub async fn subscribe_wait(
        &self,
        subscription: flotilla_protocol::WaitSubscriptionRequest,
    ) -> Result<(uuid::Uuid, broadcast::Receiver<LeafFire>), String> {
        let events = match self.wait_event_tx.upgrade() {
            Some(event_tx) => event_tx.subscribe(),
            None => {
                let (event_tx, receiver) = broadcast::channel(1);
                drop(event_tx);
                receiver
            }
        };
        match into_success_response(self.request(Request::SubscribeWait { subscription }).await?)? {
            Response::WaitSubscribed { subscription_id } => Ok((subscription_id, events)),
            other => Err(format!("unexpected response for wait subscription: {other:?}")),
        }
    }

    /// Send a request to the daemon and wait for the matching response.
    async fn request(&self, request: Request) -> Result<ResponseResult, String> {
        send_request(self.session.as_ref(), &self.pending, &self.next_id, request).await
    }
}

impl Drop for SocketDaemon {
    fn drop(&mut self) {
        if let Some(reader_task) = self.reader_task.lock().expect("reader task mutex poisoned").take() {
            reader_task.abort();
        }
    }
}

/// Acquire the daemon spawn lock (flock-based, like tmux).
///
/// Returns:
/// - `Ok(Some(file))` — lock acquired, caller should spawn the daemon
/// - `Ok(None)` — another process is spawning; we blocked until they released
/// - `Err(_)` — lock file couldn't be opened
fn acquire_spawn_lock(lock_path: &std::path::Path) -> Result<Option<std::fs::File>, String> {
    use std::os::unix::io::AsRawFd;

    // Ensure parent directory exists (e.g. first run with custom --config-dir).
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file =
        std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(lock_path).map_err(|e| format!("lock open: {e}"))?;

    // Non-blocking try: are we the first?
    let fd = file.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // We got the lock — we're the spawner.
        return Ok(Some(file));
    }

    // Another process holds the lock — block until they release it.
    // The OS releases the lock automatically if the holder dies.
    // Loop on EINTR like tmux does (client_get_lock).
    loop {
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("flock: {err}"));
        }
    }
    // Lock released — the other process's daemon should be running now.
    // Drop the lock immediately; we won't spawn.
    drop(file);
    Ok(None)
}

fn resolve_flotillad_binary() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("FLOTILLAD_BIN") {
        return Ok(PathBuf::from(path));
    }

    let current = std::env::current_exe().map_err(|e| format!("can't find self: {e}"))?;
    let parent = current.parent().ok_or_else(|| "current executable has no parent directory".to_string())?;
    let mut candidates = vec![parent.join("flotillad")];
    if parent.file_name().is_some_and(|name| name == "deps") {
        if let Some(grandparent) = parent.parent() {
            candidates.push(grandparent.join("flotillad"));
        }
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| format!("failed to locate flotillad next to {}", current.display()))
}

fn spawn_daemon(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<(), String> {
    let daemon_binary = resolve_flotillad_binary()?;
    let mut cmd = std::process::Command::new(&daemon_binary);
    cmd.arg("--config-dir").arg(config_dir);
    cmd.arg("--state-dir").arg(state_dir);
    cmd.arg("--socket").arg(socket_path);
    // Detach: own session so Ctrl-C doesn't kill daemon with TUI
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Redirect stdio. Structured logs go to
    // {state_dir}/log/flotillad.jsonl via tracing;
    // stderr catches only panics and pre-init errors.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    let panic_log = config_dir.join("daemon-panic.log");
    let _ = std::fs::create_dir_all(config_dir);
    let stderr = std::fs::File::create(&panic_log).map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null());
    cmd.stderr(stderr);
    cmd.spawn().map_err(|e| format!("failed to spawn daemon: {e}"))?;
    Ok(())
}

pub async fn connect_or_spawn(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<Arc<SocketDaemon>, String> {
    connect_or_spawn_with_optional_surface(socket_path, config_dir, state_dir, None).await
}

pub async fn connect_or_spawn_with_surface(
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    surface: SurfaceDeclaration,
) -> Result<Arc<SocketDaemon>, String> {
    connect_or_spawn_with_optional_surface(socket_path, config_dir, state_dir, Some(surface)).await
}

/// Connect to the host daemon exposed inside a contained environment.
///
/// Unlike [`connect_or_spawn`], this never acquires a spawn lock, removes a
/// stale socket, or starts a daemon. A missing listener means the host-owned
/// control-plane mount is stale or unreachable and must be repaired outside
/// the contained environment.
pub async fn connect_required_host_daemon(socket_path: &Path) -> Result<Arc<SocketDaemon>, String> {
    connect_required_host_daemon_with_optional_surface(socket_path, None).await
}

pub async fn connect_required_host_daemon_with_surface(
    socket_path: &Path,
    surface: SurfaceDeclaration,
) -> Result<Arc<SocketDaemon>, String> {
    connect_required_host_daemon_with_optional_surface(socket_path, Some(surface)).await
}

async fn connect_required_host_daemon_with_optional_surface(
    socket_path: &Path,
    surface: Option<SurfaceDeclaration>,
) -> Result<Arc<SocketDaemon>, String> {
    connect_existing_stateful(socket_path, surface.as_ref()).await?.ok_or_else(|| {
        format!(
            "host daemon socket stale or unreachable at {}; this contained environment requires the host daemon, so a local daemon will not be spawned",
            socket_path.display()
        )
    })
}

async fn connect_or_spawn_with_optional_surface(
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    surface: Option<SurfaceDeclaration>,
) -> Result<Arc<SocketDaemon>, String> {
    connect_or_spawn_with_optional_surface_using(socket_path, config_dir, state_dir, surface, &launchd_startup_owner, &spawn_daemon).await
}

type DaemonSpawner = dyn Fn(&Path, &Path, &Path) -> Result<(), String> + Send + Sync;
type DaemonSupervisor = dyn Fn(&Path, &Path, &Path) -> Result<DaemonStartupOwner, String> + Send + Sync;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonStartupOwner {
    Client,
    LaunchdAgent,
}

fn launchd_startup_owner(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<DaemonStartupOwner, String> {
    if launchd::agent_manages_daemon(socket_path, config_dir, state_dir)? {
        launchd::kickstart_agent()?;
        Ok(DaemonStartupOwner::LaunchdAgent)
    } else {
        Ok(DaemonStartupOwner::Client)
    }
}

async fn connect_or_spawn_with_optional_surface_using(
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    surface: Option<SurfaceDeclaration>,
    supervisor: &DaemonSupervisor,
    spawner: &DaemonSpawner,
) -> Result<Arc<SocketDaemon>, String> {
    // An existing socket must complete the stateful Hello handshake. A
    // handshake failure means a daemon is listening but is incompatible or
    // malformed; surface that error instead of treating the socket as stale
    // and silently spawning a second daemon over the live process.
    //
    // Deliberately no retry at this probe, unlike the two probes below: those
    // sit in windows where a *just-spawned* daemon is expected and an error is
    // probably a startup race, while a steady-state daemon that fails a
    // 5s-bounded handshake is a condition the caller should hear about
    // immediately — this is the interactive path, and retries would only
    // delay an error the user has to act on anyway.
    if let Some(daemon) = connect_existing_stateful(socket_path, surface.as_ref()).await? {
        return Ok(daemon);
    }

    if supervisor(socket_path, config_dir, state_dir)? == DaemonStartupOwner::LaunchdAgent {
        return wait_for_daemon(socket_path, surface.as_ref(), "launchd agent").await;
    }

    // Config identity constrains daemon creation, not client connectivity. A
    // client may deliberately inspect or drive an existing daemon through a
    // socket owned by another root, but it must never create that daemon with
    // its own mismatched config and state directories.
    flotilla_core::path_policy::ensure_daemon_socket_belongs_to_config(socket_path, config_dir)?;

    ensure_no_live_daemon_without_socket(state_dir, socket_path)?;

    // Acquire spawn lock (tmux-style flock). The loser blocks until the
    // winner's daemon is ready, then retries connect.
    // Append ".lock" to the full filename to avoid aliasing when the socket
    // path already ends in ".lock" (with_extension would replace it).
    let lock_path = PathBuf::from(format!("{}.lock", socket_path.display()));
    const MAX_LOCK_RETRIES: u32 = 3;
    let mut _lock_guard: Option<SpawnLockGuard> = None;
    for attempt in 0..MAX_LOCK_RETRIES {
        let lock_path_clone = lock_path.clone();
        let lock_result =
            tokio::task::spawn_blocking(move || acquire_spawn_lock(&lock_path_clone)).await.map_err(|e| format!("spawn_blocking: {e}"))?;
        match lock_result {
            Ok(Some(file)) => {
                _lock_guard = Some(SpawnLockGuard::new(file));
                break;
            }
            Ok(None) => {
                // Another process spawned the daemon — retry connect. A
                // handshake error here is most likely a race with that
                // process's daemon still starting up, so it spends a retry
                // attempt rather than aborting; but it must never fall
                // through to the spawn path, which would delete the socket of
                // a live (if unwell or incompatible) daemon.
                let last_probe_error = match connect_existing_stateful(socket_path, surface.as_ref()).await {
                    Ok(Some(daemon)) => return Ok(daemon),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(attempt = attempt + 1, error = %e, "handshake with peer-spawned daemon failed");
                        Some(e)
                    }
                };
                // Their daemon didn't come up — retry lock acquisition rather than
                // falling through to spawn without mutual exclusion.
                if attempt + 1 < MAX_LOCK_RETRIES {
                    warn!(attempt = attempt + 1, "connect after lock wait failed, retrying lock");
                    continue;
                }
                // Retries exhausted. Only spawn if the last probe found no
                // listener at all; a live daemon that kept failing the
                // handshake is a reportable condition, not a stale socket.
                if let Some(e) = last_probe_error {
                    return Err(format!("a daemon is listening but the handshake kept failing across {MAX_LOCK_RETRIES} lock-wait attempts; last error: {e}"));
                }
                // Exhausted retries — acquire lock ourselves before spawning
                // so we never spawn without mutual exclusion.
                warn!(attempts = MAX_LOCK_RETRIES, "connect after lock wait failed, acquiring lock to spawn");
                let lock_path_clone = lock_path.clone();
                let final_lock = tokio::task::spawn_blocking(move || acquire_spawn_lock(&lock_path_clone))
                    .await
                    .map_err(|e| format!("spawn_blocking: {e}"))?;
                match final_lock {
                    Ok(Some(file)) => {
                        _lock_guard = Some(SpawnLockGuard::new(file));
                        break;
                    }
                    Ok(None) => {
                        // Someone else spawned while we waited — one last connect attempt.
                        if let Some(daemon) = connect_existing_stateful(socket_path, surface.as_ref()).await? {
                            return Ok(daemon);
                        }
                        return Err("daemon spawn failed: all lock attempts exhausted and connect still failing".into());
                    }
                    Err(e) => {
                        return Err(format!("spawn lock failed: {e}"));
                    }
                }
            }
            Err(e) => {
                return Err(format!("spawn lock failed: {e}"));
            }
        }
    }

    // Final probe under the exclusively-held spawn lock. A daemon may have
    // appeared (or kept failing the handshake) while we contended for the
    // lock — in particular, a `continue` after a handshake error re-enters
    // lock acquisition, which the releasing winner has left uncontended, so
    // winning the lock says nothing about the socket being free. The lock
    // makes this check race-free: nothing else may spawn while we hold it.
    match connect_existing_stateful(socket_path, surface.as_ref()).await {
        Ok(Some(daemon)) => return Ok(daemon),
        Ok(None) => {}
        Err(e) => {
            return Err(format!(
                "won the spawn lock, but a live daemon is on the socket and failing the handshake — not spawning over it: {e}"
            ));
        }
    }

    ensure_no_live_daemon_without_socket(state_dir, socket_path)?;

    {
        // Clean up stale socket
        let _ = std::fs::remove_file(socket_path);

        // Spawn daemon process
        spawner(socket_path, config_dir, state_dir)?;
    }

    wait_for_daemon(socket_path, surface.as_ref(), "spawned daemon").await
}

/// Poll for a daemon started through the selected owner with a 10s deadline
/// (soft: the deadline is checked between probes, and a probe can block up to
/// HELLO_HANDSHAKE_TIMEOUT). Handshake errors are retried because the daemon is
/// expected to be in its startup window. The last error is surfaced if the
/// deadline expires without a successful handshake.
async fn wait_for_daemon(socket_path: &Path, surface: Option<&SurfaceDeclaration>, owner: &str) -> Result<Arc<SocketDaemon>, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_handshake_error: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match connect_existing_stateful(socket_path, surface).await {
            Ok(Some(daemon)) => return Ok(daemon),
            Ok(None) => {}
            Err(e) => last_handshake_error = Some(e),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(match last_handshake_error {
                Some(e) => format!("{owner} started the daemon, but its handshake kept failing until the 10s deadline; last error: {e}"),
                None => format!("timed out waiting for {owner} to start the daemon (10s)"),
            });
        }
    }
}

fn ensure_no_live_daemon_without_socket(state_dir: &Path, socket_path: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let lock_path = state_dir.join(flotilla_core::DAEMON_LIFECYCLE_LOCK_FILE);
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to inspect daemon lifecycle lock {}: {error}", lock_path.display())),
    };
    // SAFETY: `file` owns this descriptor for the duration of the flock calls.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // SAFETY: the descriptor is still valid, and explicitly unlocking keeps
        // this read-only health probe from retaining daemon lifecycle authority.
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN) {
        return Err(format!(
            "daemon process is alive (lifecycle lock {} is held), but its socket is missing or unreachable at {}; refusing to spawn a competing daemon — restart the existing daemon",
            lock_path.display(),
            socket_path.display()
        ));
    }
    Err(format!("failed to inspect daemon lifecycle lock {}: {error}", lock_path.display()))
}

/// Deadline for a listening daemon to complete the Hello handshake. Generous
/// for a local Unix socket; only a wedged or badly stalled daemon exceeds it.
const HELLO_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to an existing socket and require the client Hello handshake.
///
/// `Ok(None)` means no daemon accepted the Unix-socket connection, so the
/// caller may use the spawn path. Once connected, handshake failures are
/// returned verbatim: a live but incompatible daemon must not be mistaken for
/// a stale socket. The handshake is bounded — a daemon that accepts the
/// connection but never replies is reported as an error rather than hanging
/// the caller (or, worse, being treated as absent and spawned over).
async fn connect_existing_stateful(socket_path: &Path, surface: Option<&SurfaceDeclaration>) -> Result<Option<Arc<SocketDaemon>>, String> {
    let session = match connect_unix_message_session(socket_path).await {
        Ok(session) => session,
        Err(_) => return Ok(None),
    };
    from_session_stateful_bounded(socket_path, session, surface).await.map(Some)
}

async fn from_session_stateful_bounded(
    socket_path: &Path,
    session: MessageSession,
    surface: Option<&SurfaceDeclaration>,
) -> Result<Arc<SocketDaemon>, String> {
    let result = match surface {
        Some(surface) => {
            tokio::time::timeout(HELLO_HANDSHAKE_TIMEOUT, SocketDaemon::from_session_stateful_with_surface(session, surface.clone())).await
        }
        None => tokio::time::timeout(HELLO_HANDSHAKE_TIMEOUT, SocketDaemon::from_session_stateful(session)).await,
    };
    match result {
        Ok(result) => result,
        Err(_) => Err(format!(
            "daemon at {} accepted the connection but did not complete the Hello handshake within {}s — it may be wedged; check or restart it",
            socket_path.display(),
            HELLO_HANDSHAKE_TIMEOUT.as_secs()
        )),
    }
}

/// Send a request on the wire and wait for the response.
///
/// Extracted as a free function so both the SocketDaemon methods and the
/// background recovery task can use it.
async fn send_request(
    session: &MessageSession,
    pending: &Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>,
    next_id: &AtomicU64,
    request: Request,
) -> Result<ResponseResult, String> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    let (tx, rx) = oneshot::channel();

    {
        let mut map = pending.lock().await;
        map.insert(id, tx);
    }

    let msg = Message::Request { id, request };

    let write_result = session.write(msg).await;

    if let Err(e) = write_result {
        pending.lock().await.remove(&id);
        return Err(e);
    }

    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(raw)) => Ok(raw),
        Ok(Err(_)) => {
            pending.lock().await.remove(&id);
            Err("request cancelled (sender dropped)".to_string())
        }
        Err(_) => {
            pending.lock().await.remove(&id);
            Err("request timed out after 30s".to_string())
        }
    }
}

fn encode_replay_cursors(last_seen: &HashMap<StreamKey, u64>) -> Vec<ReplayCursor> {
    last_seen.iter().map(|(stream, &seq)| ReplayCursor { stream: stream.clone(), seq }).collect()
}

fn into_success_response(result: ResponseResult) -> Result<Response, String> {
    match result {
        ResponseResult::Ok { response } => Ok(*response),
        ResponseResult::Err { message } => Err(message),
    }
}

/// Shared state the background reader threads through event handling and gap
/// recovery: seq tracking, the query subscription set, in-flight recovery
/// buffers, the subscriber fan-out, and the request plumbing recovery needs.
#[derive(Clone, bon::Builder)]
struct EventContext {
    local_seqs: Arc<SeqMap>,
    subscribed_queries: Arc<QuerySet>,
    event_tx: broadcast::WeakSender<DaemonEvent>,
    wait_event_tx: broadcast::WeakSender<LeafFire>,
    session: Arc<MessageSession>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    next_id: Arc<AtomicU64>,
}

fn send_event(event_tx: &broadcast::WeakSender<DaemonEvent>, event: DaemonEvent) {
    if let Some(event_tx) = event_tx.upgrade() {
        let _ = event_tx.send(event);
    }
}

fn send_wait_event(event_tx: &broadcast::WeakSender<LeafFire>, event: LeafFire) {
    if let Some(event_tx) = event_tx.upgrade() {
        let _ = event_tx.send(event);
    }
}

/// Handle a daemon event in the background reader: update local seq tracking,
/// forward to TUI subscribers, and spawn gap recovery if needed.
///
/// This function is non-async and never blocks the reader loop. Gap recovery
/// is spawned on a separate task to avoid deadlocking the reader (which must
/// remain free to route the recovery response).
fn handle_event(event: DaemonEvent, ctx: &EventContext) {
    let EventContext { local_seqs, event_tx, wait_event_tx, .. } = ctx;
    match &event {
        DaemonEvent::HostRemoved { environment_id, seq } => {
            local_seqs.write().expect("sequence lock poisoned").insert(StreamKey::Host { environment_id: environment_id.clone() }, *seq);
            send_event(event_tx, event);
        }
        DaemonEvent::HostSnapshot(snap) => {
            let stream_key = StreamKey::Host { environment_id: snap.environment_id.clone() };
            local_seqs.write().expect("sequence lock poisoned").insert(stream_key, snap.seq);
            send_event(event_tx, event);
        }
        DaemonEvent::ResultSet(result_set) => {
            local_seqs.write().expect("sequence lock poisoned").insert(StreamKey::Query { query: result_set.query() }, result_set.seq);
            send_event(event_tx, event);
        }
        DaemonEvent::ResultDelta(delta) => {
            let query = delta.query();
            let seq = delta.seq;
            let stream_key = StreamKey::Query { query: query.clone() };
            let local_seq = local_seqs.read().expect("sequence lock poisoned").get(&stream_key).copied();

            match local_seq {
                Some(ls) if seq == ls + 1 => {
                    local_seqs.write().expect("sequence lock poisoned").insert(stream_key, seq);
                    debug!(%query, %seq, "applied result delta");
                    send_event(event_tx, event);
                }
                Some(ls) if seq <= ls => {
                    // Already covered by the current result set — e.g. a live
                    // delta that raced ahead of the subscribe replay. Ignore.
                    debug!(%query, local_seq = ls, %seq, "ignoring stale result delta");
                }
                _ => {
                    if let Some(ls) = local_seq {
                        warn!(%query, local_seq = ls, %seq, "result seq gap, resubscribing");
                    } else {
                        warn!(%query, %seq, "received result delta for unknown query, resubscribing");
                    }

                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        recover_query_gap(&ctx).await;
                    });
                }
            }
        }
        DaemonEvent::RepoSnapshot(_)
        | DaemonEvent::RepoDelta(_)
        | DaemonEvent::RepoTracked(_)
        | DaemonEvent::RepoUntracked { .. }
        | DaemonEvent::RepoRefreshCompleted { .. }
        | DaemonEvent::CommandStarted { .. }
        | DaemonEvent::CommandFinished { .. }
        | DaemonEvent::CommandStepUpdate { .. }
        | DaemonEvent::PeerStatusChanged { .. } => {
            send_event(event_tx, event);
        }
        DaemonEvent::LeafFired(fire) => {
            send_wait_event(wait_event_tx, fire.clone());
            send_event(event_tx, event);
        }
    }
}

/// Build subscribe cursors for the currently subscribed queries from local
/// seq tracking.
fn encode_query_cursors(subscribed_queries: &QuerySet, local_seqs: &SeqMap) -> Vec<QueryCursor> {
    let subscribed = subscribed_queries.read().expect("subscribed queries lock poisoned");
    let seqs = local_seqs.read().expect("sequence lock poisoned");
    subscribed
        .iter()
        .map(|query| QueryCursor { query: query.clone(), since: seqs.get(&StreamKey::Query { query: query.clone() }).copied() })
        .collect()
}

/// Seed local seq tracking from subscribe-replay result sets, monotonically —
/// a live event may have advanced a query's seq while the request was in
/// flight.
fn seed_query_seqs(local_seqs: &SeqMap, events: &[DaemonEvent]) {
    let mut seqs = local_seqs.write().expect("sequence lock poisoned");
    for event in events {
        if let DaemonEvent::ResultSet(result_set) = event {
            let key = StreamKey::Query { query: result_set.query() };
            seqs.entry(key).and_modify(|seq| *seq = (*seq).max(result_set.seq)).or_insert(result_set.seq);
        }
    }
}

/// Recover from a result-set seq gap by re-subscribing with current cursors;
/// the daemon replays a full `ResultSet` for each stale query.
///
/// Unlike repo recovery there is deliberately no in-flight coalescing buffer:
/// concurrent resubscribes are idempotent (each replaces the subscription and
/// returns full result sets, seeded monotonically), and stale deltas are
/// dropped in `handle_event` rather than re-triggering recovery.
async fn recover_query_gap(ctx: &EventContext) {
    let EventContext { local_seqs, subscribed_queries, event_tx, session, pending, next_id, .. } = ctx;
    let queries = encode_query_cursors(subscribed_queries, local_seqs);
    // A delta for an unsubscribed query cannot reach this connection, so an
    // unknown-query gap implies a subscription exists; still, guard the
    // degenerate empty case.
    if queries.is_empty() {
        warn!("query gap recovery skipped: no subscribed queries");
        return;
    }
    let resp = send_request(session.as_ref(), pending, next_id, Request::SubscribeQueries { queries }).await;

    match resp {
        Ok(result) => match into_success_response(result) {
            Ok(Response::SubscribeQueries(events)) => {
                debug!(event_count = events.len(), "query gap recovery: got result sets");
                seed_query_seqs(local_seqs, &events);
                for event in events {
                    send_event(event_tx, event);
                }
            }
            Ok(other) => {
                error!(response = ?other, "query gap recovery: unexpected subscribe_queries response");
            }
            Err(e) => {
                error!(err = %e, "query gap recovery: subscribe_queries returned error response");
            }
        },
        Err(e) => {
            error!(err = %e, "query gap recovery: subscribe_queries request failed");
        }
    }
}

#[async_trait]
impl DaemonHandle for SocketDaemon {
    fn build_id(&self) -> Option<&str> {
        self.daemon_build_id.as_deref()
    }

    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        match self.event_tx.upgrade() {
            Some(event_tx) => event_tx.subscribe(),
            None => {
                let (event_tx, receiver) = broadcast::channel(1);
                drop(event_tx);
                receiver
            }
        }
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        match into_success_response(self.request(Request::ListRepos).await?)? {
            Response::ListRepos(repos) => Ok(repos),
            other => Err(format!("unexpected response for list_repos: {other:?}")),
        }
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        match into_success_response(self.request(Request::Execute { command }).await?)? {
            Response::Execute { command_id } => Ok(command_id),
            other => Err(format!("unexpected response for execute: {other:?}")),
        }
    }

    async fn observe_focus(&self, _surface_id: uuid::Uuid, targets: Vec<flotilla_protocol::ResourceRef>) -> Result<(), String> {
        match into_success_response(self.request(Request::ObserveFocus { targets }).await?)? {
            Response::ObserveFocus => Ok(()),
            other => Err(format!("unexpected response for focus observation: {other:?}")),
        }
    }

    /// Execute a query command and return the result directly.
    ///
    /// The `session_id` parameter is ignored by `SocketDaemon` because cursor
    /// ownership uses the Hello-handshake session_id assigned on the server
    /// side. The parameter exists on the `DaemonHandle` trait for
    /// `InProcessDaemon`'s use, where there is no Hello handshake.
    async fn execute_query(&self, command: Command, _session_id: uuid::Uuid) -> Result<flotilla_protocol::commands::CommandValue, String> {
        match into_success_response(self.request(Request::Execute { command }).await?)? {
            Response::QueryResult { value, .. } => Ok(value),
            Response::Execute { command_id } => Err(format!("expected QueryResult, got Execute response for command {command_id}")),
            other => Err(format!("unexpected response for query: {other:?}")),
        }
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        match into_success_response(self.request(Request::Cancel { command_id }).await?)? {
            Response::Cancel => Ok(()),
            other => Err(format!("unexpected response for cancel: {other:?}")),
        }
    }

    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        let last_seen = encode_replay_cursors(last_seen);
        let events = match into_success_response(self.request(Request::ReplaySince { last_seen }).await?)? {
            Response::ReplaySince(events) => events,
            other => return Err(format!("unexpected response for replay_since: {other:?}")),
        };

        // Seed local_seqs from replay events so the background reader
        // doesn't trigger spurious gap recovery for the first live delta.
        // Use monotonic update: a live event processed between subscribe and
        // replay_since may have already advanced the seq further.
        {
            let mut seqs = self.local_seqs.write().expect("sequence lock poisoned");
            for event in &events {
                let (stream_key, seq) = match event {
                    DaemonEvent::HostSnapshot(snap) => (StreamKey::Host { environment_id: snap.environment_id.clone() }, snap.seq),
                    DaemonEvent::HostRemoved { environment_id, seq } => (StreamKey::Host { environment_id: environment_id.clone() }, *seq),
                    DaemonEvent::RepoSnapshot(_)
                    | DaemonEvent::RepoDelta(_)
                    | DaemonEvent::RepoTracked(_)
                    | DaemonEvent::RepoRefreshCompleted { .. }
                    | DaemonEvent::RepoUntracked { .. }
                    | DaemonEvent::CommandStarted { .. }
                    | DaemonEvent::CommandFinished { .. }
                    | DaemonEvent::CommandStepUpdate { .. }
                    | DaemonEvent::PeerStatusChanged { .. }
                    | DaemonEvent::ResultSet(_)
                    | DaemonEvent::ResultDelta(_)
                    | DaemonEvent::LeafFired(_) => continue,
                };
                seqs.entry(stream_key).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
            }
        }
        self.initial_sync_complete.store(true, Ordering::Release);

        Ok(events)
    }

    async fn subscribe_queries(&self, _subscriber_id: uuid::Uuid, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        // Record the subscription before sending so a delta racing ahead of
        // the response finds the query known and recovery can re-subscribe.
        {
            let mut subscribed = self.subscribed_queries.write().expect("subscribed queries lock poisoned");
            *subscribed = queries.iter().map(|cursor| cursor.query.clone()).collect();
        }
        let events = match into_success_response(self.request(Request::SubscribeQueries { queries: queries.to_vec() }).await?)? {
            Response::SubscribeQueries(events) => events,
            other => return Err(format!("unexpected response for subscribe_queries: {other:?}")),
        };
        seed_query_seqs(&self.local_seqs, &events);
        self.initial_sync_complete.store(true, Ordering::Release);
        Ok(events)
    }

    async fn unsubscribe_queries(&self, _subscriber_id: uuid::Uuid) {
        // The server owns this SocketDaemon connection's subscriber identity
        // and tears it down when the connection closes.
    }

    async fn fetch_more(&self, query: &QueryId) -> Result<(), String> {
        match into_success_response(self.request(Request::FetchMore { query: query.clone() }).await?)? {
            Response::FetchMore => Ok(()),
            other => Err(format!("unexpected response for fetch-more: {other:?}")),
        }
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        match into_success_response(self.request(Request::GetStatus).await?)? {
            Response::GetStatus(status) => Ok(status),
            other => Err(format!("unexpected response for get_status: {other:?}")),
        }
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        match into_success_response(self.request(Request::GetTopology).await?)? {
            Response::GetTopology(topology) => Ok(topology),
            other => Err(format!("unexpected response for get_topology: {other:?}")),
        }
    }
}

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;

#[cfg(test)]
mod spawn_lock_tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn spawn_lock_guard_keeps_stable_lock_file_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_path = dir.path().join("test.lock");
        fs::write(&lock_path, "").expect("create lock file");
        let file = fs::File::open(&lock_path).expect("open lock file");
        {
            let _guard = SpawnLockGuard::new(file);
            assert!(lock_path.exists(), "lock file should exist while guard is held");
        }
        assert!(lock_path.exists(), "lock file inode must remain stable across owners");
    }

    #[tokio::test(start_paused = true)]
    async fn launchd_owned_startup_never_calls_the_direct_spawner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = dir.path().join("config");
        let state_dir = dir.path().join("state");
        let socket_path = config_dir.join("run/flotilla.sock");
        let direct_spawns = Arc::new(AtomicUsize::new(0));
        let counted_spawns = Arc::clone(&direct_spawns);
        let supervisor = |_: &Path, _: &Path, _: &Path| Ok(DaemonStartupOwner::LaunchdAgent);
        let spawner = move |_: &Path, _: &Path, _: &Path| {
            counted_spawns.fetch_add(1, Ordering::Relaxed);
            Ok(())
        };

        let result = connect_or_spawn_with_optional_surface_using(&socket_path, &config_dir, &state_dir, None, &supervisor, &spawner).await;
        let Err(error) = result else { panic!("an absent fake launchd daemon should time out") };

        assert!(error.contains("launchd agent"), "unexpected error: {error}");
        assert_eq!(direct_spawns.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn client_owned_startup_reaches_the_direct_spawner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = dir.path().join("config");
        let state_dir = dir.path().join("state");
        let socket_path = config_dir.join("run/flotilla.sock");
        fs::create_dir_all(socket_path.parent().expect("socket parent")).expect("create socket parent");
        fs::create_dir_all(&state_dir).expect("create state dir");
        let direct_spawns = Arc::new(AtomicUsize::new(0));
        let counted_spawns = Arc::clone(&direct_spawns);
        let supervisor = |_: &Path, _: &Path, _: &Path| Ok(DaemonStartupOwner::Client);
        let spawner = move |_: &Path, _: &Path, _: &Path| {
            counted_spawns.fetch_add(1, Ordering::Relaxed);
            Err("direct spawner reached".to_string())
        };

        let result = connect_or_spawn_with_optional_surface_using(&socket_path, &config_dir, &state_dir, None, &supervisor, &spawner).await;
        let Err(error) = result else { panic!("the fake direct spawner should fail") };

        assert_eq!(error, "direct spawner reached");
        assert_eq!(direct_spawns.load(Ordering::Relaxed), 1);
    }
}
