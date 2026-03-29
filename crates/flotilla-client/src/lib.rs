use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    Command, ConnectionRole, DaemonEvent, HostName, Message, ReplayCursor, RepoIdentity, RepoInfo, RepoSnapshot, Request, Response,
    ResponseResult, StatusResponse, StreamKey, TopologyResponse, PROTOCOL_VERSION,
};
use flotilla_transport::message::{connect_unix_message_session, MessageSession};
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{debug, error, warn};

/// Std RwLock for local seq tracking — the critical sections are single HashMap
/// operations (no async work while holding the lock), and using a sync lock
/// avoids the race where a spawned seq update hasn't run before the next delta
/// arrives.
type SeqMap = std::sync::RwLock<HashMap<StreamKey, u64>>;

/// RAII guard that removes a lock file when dropped.
///
/// Holds the open file handle (which keeps the OS flock) and removes the
/// lock file on drop.  The flock is released *before* the path is unlinked
/// so that concurrent clients racing on the same path always contend on the
/// same inode — unlinking first would let them create a new file and flock
/// a different inode, breaking mutual exclusion.
struct SpawnLockGuard {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl SpawnLockGuard {
    fn new(file: std::fs::File, path: PathBuf) -> Self {
        Self { file: Some(file), path }
    }
}

impl Drop for SpawnLockGuard {
    fn drop(&mut self) {
        // Release the flock before unlinking, preserving the
        // mutual-exclusion contract during the handoff window.
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct SocketDaemon {
    session: Arc<MessageSession>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: Arc<AtomicU64>,
    reader_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Local snapshot seq per repo, for gap detection.
    /// Updated by replay_since (seeding) and the background reader (live events).
    local_seqs: Arc<SeqMap>,
}

impl SocketDaemon {
    /// Connect to a running daemon at the given Unix socket path.
    ///
    /// Builds a session from the socket and then starts the shared client
    /// reader/pending-request machinery on top of it.
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let session = connect_unix_message_session(socket_path).await?;
        Self::from_session(session)
    }

    /// Connect to a running daemon with a stateful Hello handshake.
    ///
    /// Sends a `Hello` with `ConnectionRole::Client` and a fresh `session_id`,
    /// waits for the server's Hello reply, then builds the normal client session.
    /// The `session_id` enables cursor ownership for directed query responses.
    pub async fn connect_stateful(socket_path: &Path) -> Result<Arc<Self>, String> {
        let session = connect_unix_message_session(socket_path).await?;

        let session_id = uuid::Uuid::new_v4();
        session
            .write(Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                host_name: HostName::new("client"),
                session_id,
                connection_role: Some(ConnectionRole::Client),
                environment_id: None,
            })
            .await
            .map_err(|e| format!("failed to send Hello: {e}"))?;

        match session.read().await.map_err(|e| format!("failed to read Hello reply: {e}"))? {
            Some(Message::Hello { .. }) => {}
            Some(other) => return Err(format!("expected Hello reply, got: {other:?}")),
            None => return Err("connection closed before Hello reply".into()),
        }

        Self::from_session(session)
    }

    pub fn from_session(session: MessageSession) -> Result<Arc<Self>, String> {
        let session = Arc::new(session);

        let (event_tx, _) = broadcast::channel(256);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Spawn background reader task
        let reader_session = Arc::clone(&session);
        let reader_pending = Arc::clone(&pending);
        let reader_next_id = Arc::clone(&next_id);
        let reader_local_seqs = Arc::clone(&local_seqs);
        let reader_recovering = Arc::clone(&recovering);
        let reader_event_tx = event_tx.clone();
        let reader_task = tokio::spawn(async move {
            loop {
                match reader_session.read().await {
                    Ok(Some(msg)) => match msg {
                        Message::Response { id, response } => {
                            let mut map = reader_pending.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let _ = tx.send(*response);
                            } else {
                                warn!(%id, "received response for unknown request id");
                            }
                        }
                        Message::Event { event } => {
                            let event = *event;
                            handle_event(
                                event,
                                &reader_local_seqs,
                                &reader_recovering,
                                &reader_event_tx,
                                &reader_session,
                                &reader_pending,
                                &reader_next_id,
                            );
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
                        error!("daemon connection closed (EOF)");
                        let mut map = reader_pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(ResponseResult::Err { message: "daemon connection closed".into() });
                        }
                        break;
                    }
                    Err(e) => {
                        error!(err = %e, "error reading from daemon session");
                        let mut map = reader_pending.lock().await;
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
            event_tx: event_tx.clone(),
            next_id: Arc::clone(&next_id),
            reader_task: std::sync::Mutex::new(Some(reader_task)),
            local_seqs: Arc::clone(&local_seqs),
        });

        Ok(daemon)
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

fn spawn_daemon(config_dir: &Path, config_dir_override: Option<&Path>, socket_override: Option<&Path>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find self: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon");
    if let Some(dir) = config_dir_override {
        cmd.arg("--config-dir").arg(dir);
    }
    if let Some(socket) = socket_override {
        cmd.arg("--socket").arg(socket);
    }
    // Detach: own session so Ctrl-C doesn't kill daemon with TUI
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Redirect stdio. Structured logs go to {state_dir}/daemon.log via tracing;
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

pub async fn connect_or_spawn(
    socket_path: &Path,
    config_dir: &Path,
    config_dir_override: Option<&Path>,
    socket_override: Option<&Path>,
) -> Result<Arc<SocketDaemon>, String> {
    // Try to connect to existing daemon
    if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
        return Ok(daemon);
    }

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
                _lock_guard = Some(SpawnLockGuard::new(file, lock_path.clone()));
                break;
            }
            Ok(None) => {
                // Another process spawned the daemon — retry connect.
                if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
                    return Ok(daemon);
                }
                // Their daemon didn't come up — retry lock acquisition rather than
                // falling through to spawn without mutual exclusion.
                if attempt + 1 < MAX_LOCK_RETRIES {
                    warn!(attempt = attempt + 1, "connect after lock wait failed, retrying lock");
                    continue;
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
                        _lock_guard = Some(SpawnLockGuard::new(file, lock_path.clone()));
                        break;
                    }
                    Ok(None) => {
                        // Someone else spawned while we waited — one last connect attempt.
                        if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
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

    {
        // Clean up stale socket
        let _ = std::fs::remove_file(socket_path);

        // Spawn daemon process
        spawn_daemon(config_dir, config_dir_override, socket_override)?;
    }

    // Poll for connection with a 10s deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
            return Ok(daemon);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for daemon to start (10s)".into());
        }
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

/// Handle a daemon event in the background reader: update local seq tracking,
/// forward to TUI subscribers, and spawn gap recovery if needed.
///
/// This function is non-async and never blocks the reader loop. Gap recovery
/// is spawned on a separate task to avoid deadlocking the reader (which must
/// remain free to route the recovery response).
fn handle_event(
    event: DaemonEvent,
    local_seqs: &Arc<SeqMap>,
    recovering: &Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    session: &Arc<MessageSession>,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    next_id: &Arc<AtomicU64>,
) {
    match &event {
        DaemonEvent::RepoSnapshot(snap) => {
            debug!(repo_identity = %snap.repo_identity, repo = %snap.repo.display(), seq = snap.seq, "received full snapshot");
            // Sync lock: update seq before dispatching event so a
            // quickly-following delta sees the correct local seq.
            local_seqs.write().unwrap().insert(StreamKey::Repo { identity: snap.repo_identity.clone() }, snap.seq);
            let _ = event_tx.send(event);
        }
        DaemonEvent::RepoDelta(delta) => {
            let repo = delta.repo.clone();
            let repo_identity = delta.repo_identity.clone();
            let prev_seq = delta.prev_seq;
            let seq = delta.seq;

            let stream_key = StreamKey::Repo { identity: repo_identity.clone() };

            // Check seq under sync lock, then spawn only if recovery needed.
            let local_seq = local_seqs.read().unwrap().get(&stream_key).copied();

            match local_seq {
                Some(ls) if prev_seq == ls => {
                    // Happy path: apply delta (sync lock, no spawn needed)
                    local_seqs.write().unwrap().insert(stream_key, seq);
                    debug!(repo_identity = %repo_identity, repo = %repo.display(), %prev_seq, %seq, "applied delta");
                    let _ = event_tx.send(event);
                }
                _ => {
                    // Seq gap or unknown repo — spawn recovery if not already in progress.
                    // If recovery is already running, buffer this delta so it can be
                    // re-processed after recovery completes (prevents permanent staleness
                    // when a live delta arrives during the recovery window).
                    let mut guard = recovering.lock().unwrap();
                    if let Some(buf) = guard.get_mut(&repo_identity) {
                        debug!(repo_identity = %repo_identity, repo = %repo.display(), %seq, "recovery in progress, buffering delta");
                        buf.push(event);
                        return;
                    }
                    guard.insert(repo_identity.clone(), vec![event]);
                    drop(guard);

                    if let Some(ls) = local_seq {
                        warn!(repo_identity = %repo_identity, repo = %repo.display(), local_seq = ls, %prev_seq, "seq gap, requesting replay");
                    } else {
                        warn!(repo_identity = %repo_identity, repo = %repo.display(), "received delta for unknown repo, requesting replay");
                    }

                    let local_seqs = Arc::clone(local_seqs);
                    let recovering = Arc::clone(recovering);
                    let event_tx = event_tx.clone();
                    let session = Arc::clone(session);
                    let pending = Arc::clone(pending);
                    let next_id = Arc::clone(next_id);

                    tokio::spawn(async move {
                        recover_from_gap(&local_seqs, &event_tx, &session, &pending, &next_id).await;
                        // Drain buffered deltas, discarding any that recovery
                        // already covered (their seq <= recovered local_seq).
                        // Only re-process deltas that are genuinely ahead.
                        let buffered = recovering.lock().unwrap().remove(&repo_identity).unwrap_or_default();
                        let stream_key = StreamKey::Repo { identity: repo_identity };
                        let recovered_seq = local_seqs.read().unwrap().get(&stream_key).copied();
                        for buffered_event in buffered {
                            let dominated = match &buffered_event {
                                DaemonEvent::RepoDelta(d) => recovered_seq.is_some_and(|rs| d.seq <= rs),
                                _ => false,
                            };
                            if dominated {
                                debug!("discarding buffered delta already covered by recovery");
                                continue;
                            }
                            handle_event(buffered_event, &local_seqs, &recovering, &event_tx, &session, &pending, &next_id);
                        }
                    });
                }
            }
        }
        DaemonEvent::RepoUntracked { repo_identity, .. } => {
            // Sync lock: evict before dispatching
            local_seqs.write().unwrap().remove(&StreamKey::Repo { identity: repo_identity.clone() });
            let _ = event_tx.send(event);
        }
        DaemonEvent::HostRemoved { host, seq } => {
            local_seqs.write().unwrap().insert(StreamKey::Host { host_name: host.clone() }, *seq);
            let _ = event_tx.send(event);
        }
        DaemonEvent::HostSnapshot(snap) => {
            let stream_key = StreamKey::Host { host_name: snap.host_name.clone() };
            local_seqs.write().unwrap().insert(stream_key, snap.seq);
            let _ = event_tx.send(event);
        }
        DaemonEvent::RepoTracked(_)
        | DaemonEvent::CommandStarted { .. }
        | DaemonEvent::CommandFinished { .. }
        | DaemonEvent::CommandStepUpdate { .. }
        | DaemonEvent::PeerStatusChanged { .. } => {
            let _ = event_tx.send(event);
        }
    }
}

/// Recover from a seq gap by calling `replay_since` with the stale seq,
/// updating local seq tracking, and forwarding replay events to the TUI.
async fn recover_from_gap(
    local_seqs: &SeqMap,
    event_tx: &broadcast::Sender<DaemonEvent>,
    session: &Arc<MessageSession>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>,
    next_id: &AtomicU64,
) {
    let last_seen = {
        let seqs = local_seqs.read().unwrap();
        seqs.clone()
    };

    let last_seen = encode_replay_cursors(&last_seen);
    let resp = send_request(session.as_ref(), pending, next_id, Request::ReplaySince { last_seen }).await;

    match resp {
        Ok(result) => match into_success_response(result) {
            Ok(Response::ReplaySince(events)) => {
                debug!(event_count = events.len(), "gap recovery: got replay events");
                // Update seqs monotonically — a live event may have advanced
                // a repo's seq while this replay was in flight.
                {
                    let mut seqs = local_seqs.write().unwrap();
                    for event in &events {
                        match event {
                            DaemonEvent::RepoSnapshot(snap) => {
                                let key = StreamKey::Repo { identity: snap.repo_identity.clone() };
                                let current = seqs.get(&key).copied().unwrap_or(0);
                                if snap.seq >= current {
                                    seqs.insert(key, snap.seq);
                                }
                            }
                            DaemonEvent::RepoDelta(delta) => {
                                let key = StreamKey::Repo { identity: delta.repo_identity.clone() };
                                let current = seqs.get(&key).copied().unwrap_or(0);
                                if delta.seq >= current {
                                    seqs.insert(key, delta.seq);
                                }
                            }
                            DaemonEvent::HostSnapshot(snap) => {
                                let key = StreamKey::Host { host_name: snap.host_name.clone() };
                                let current = seqs.get(&key).copied().unwrap_or(0);
                                if snap.seq >= current {
                                    seqs.insert(key, snap.seq);
                                }
                            }
                            DaemonEvent::HostRemoved { host, seq } => {
                                let key = StreamKey::Host { host_name: host.clone() };
                                let current = seqs.get(&key).copied().unwrap_or(0);
                                if *seq >= current {
                                    seqs.insert(key, *seq);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                for event in events {
                    let _ = event_tx.send(event);
                }
            }
            Ok(other) => {
                error!(response = ?other, "gap recovery: unexpected replay_since response");
            }
            Err(e) => {
                error!(err = %e, "gap recovery: replay_since returned error response");
            }
        },
        Err(e) => {
            error!(err = %e, "gap recovery: replay_since request failed");
        }
    }
}

#[async_trait]
impl DaemonHandle for SocketDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoSnapshot, String> {
        // Always RPC to server — local state only tracks seqs for gap detection,
        // not full snapshots (work_items can't be materialized client-side).
        let repo_path = match repo {
            flotilla_protocol::RepoSelector::Path(p) => p.clone(),
            other => return Err(format!("get_state requires a path selector, got: {other:?}")),
        };
        match into_success_response(self.request(Request::GetState { repo: repo_path }).await?)? {
            Response::GetState(snapshot) => Ok(*snapshot),
            other => Err(format!("unexpected response for get_state: {other:?}")),
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
            let mut seqs = self.local_seqs.write().unwrap();
            for event in &events {
                let (stream_key, seq) = match event {
                    DaemonEvent::RepoSnapshot(snap) => (StreamKey::Repo { identity: snap.repo_identity.clone() }, snap.seq),
                    DaemonEvent::RepoDelta(delta) => (StreamKey::Repo { identity: delta.repo_identity.clone() }, delta.seq),
                    DaemonEvent::HostSnapshot(snap) => (StreamKey::Host { host_name: snap.host_name.clone() }, snap.seq),
                    DaemonEvent::HostRemoved { host, seq } => (StreamKey::Host { host_name: host.clone() }, *seq),
                    _ => continue,
                };
                seqs.entry(stream_key).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
            }
        }

        Ok(events)
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
    use std::fs;

    use super::*;

    #[test]
    fn spawn_lock_guard_removes_file_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_path = dir.path().join("test.lock");
        fs::write(&lock_path, "").expect("create lock file");
        let file = fs::File::open(&lock_path).expect("open lock file");
        {
            let _guard = SpawnLockGuard::new(file, lock_path.clone());
            assert!(lock_path.exists(), "lock file should exist while guard is held");
        }
        assert!(!lock_path.exists(), "lock file should be removed after guard drops");
    }
}
