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
    Command, DaemonEvent, HostListResponse, HostProvidersResponse, HostStatusResponse, Message, ReplayCursor, RepoDetailResponse,
    RepoIdentity, RepoInfo, RepoProvidersResponse, RepoSnapshot, RepoWorkResponse, Request, Response, ResponseResult, StatusResponse,
    TopologyResponse,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    net::UnixStream,
    sync::{broadcast, oneshot, Mutex},
};
use tracing::{debug, error, warn};

/// Std RwLock for local seq tracking — the critical sections are single HashMap
/// operations (no async work while holding the lock), and using a sync lock
/// avoids the race where a spawned seq update hasn't run before the next delta
/// arrives.
type SeqMap = std::sync::RwLock<HashMap<RepoIdentity, u64>>;

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
    writer: Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: Arc<AtomicU64>,
    /// Local snapshot seq per repo, for gap detection.
    /// Updated by replay_since (seeding) and the background reader (live events).
    local_seqs: Arc<SeqMap>,
}

impl SocketDaemon {
    /// Connect to a running daemon at the given Unix socket path.
    ///
    /// Splits the socket into reader/writer halves, spawns a background task
    /// to read incoming messages, and returns `Arc<Self>`.
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;

        let (read_half, write_half) = stream.into_split();

        let (event_tx, _) = broadcast::channel(256);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));

        let writer = Arc::new(Mutex::new(BufWriter::new(write_half)));

        let daemon = Arc::new(Self {
            writer: Arc::clone(&writer),
            pending: Arc::clone(&pending),
            event_tx: event_tx.clone(),
            next_id: Arc::clone(&next_id),
            local_seqs: Arc::clone(&local_seqs),
        });

        // Spawn background reader task
        let reader_pending = Arc::clone(&pending);
        let reader_writer = Arc::clone(&writer);
        let reader_next_id = Arc::clone(&next_id);
        let reader_local_seqs = Arc::clone(&local_seqs);
        let reader_recovering = Arc::clone(&recovering);
        let reader_event_tx = event_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(read_half);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let msg: Message = match serde_json::from_str(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!(err = %e, "failed to parse message from daemon");
                                continue;
                            }
                        };

                        match msg {
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
                                    &reader_writer,
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
                        }
                    }
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
                        error!(err = %e, "error reading from daemon socket");
                        let mut map = reader_pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(ResponseResult::Err { message: format!("daemon read error: {e}") });
                        }
                        break;
                    }
                }
            }
        });

        Ok(daemon)
    }

    /// Send a request to the daemon and wait for the matching response.
    async fn request(&self, request: Request) -> Result<ResponseResult, String> {
        send_request(&self.writer, &self.pending, &self.next_id, request).await
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
    // Redirect stdio, log stderr to file for debugging
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    let log_file = config_dir.join("daemon.log");
    let _ = std::fs::create_dir_all(config_dir);
    let stderr = std::fs::File::create(&log_file).map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null());
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
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
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

    let write_result = async {
        let mut w = writer.lock().await;
        flotilla_protocol::framing::write_message_line(&mut *w, &msg).await
    }
    .await;

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

fn encode_replay_cursors(last_seen: &HashMap<RepoIdentity, u64>) -> Vec<ReplayCursor> {
    last_seen.iter().map(|(repo_identity, &seq)| ReplayCursor { repo_identity: repo_identity.clone(), seq }).collect()
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
    writer: &Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>,
    next_id: &Arc<AtomicU64>,
) {
    match &event {
        DaemonEvent::RepoSnapshot(snap) => {
            debug!(repo_identity = %snap.repo_identity, repo = %snap.repo.display(), seq = snap.seq, "received full snapshot");
            // Sync lock: update seq before dispatching event so a
            // quickly-following delta sees the correct local seq.
            local_seqs.write().unwrap().insert(snap.repo_identity.clone(), snap.seq);
            let _ = event_tx.send(event);
        }
        DaemonEvent::RepoDelta(delta) => {
            let repo = delta.repo.clone();
            let repo_identity = delta.repo_identity.clone();
            let prev_seq = delta.prev_seq;
            let seq = delta.seq;

            // Check seq under sync lock, then spawn only if recovery needed.
            let local_seq = local_seqs.read().unwrap().get(&repo_identity).copied();

            match local_seq {
                Some(ls) if prev_seq == ls => {
                    // Happy path: apply delta (sync lock, no spawn needed)
                    local_seqs.write().unwrap().insert(repo_identity.clone(), seq);
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
                    let writer = Arc::clone(writer);
                    let pending = Arc::clone(pending);
                    let next_id = Arc::clone(next_id);

                    tokio::spawn(async move {
                        recover_from_gap(&local_seqs, &event_tx, &writer, &pending, &next_id).await;
                        // Drain buffered deltas, discarding any that recovery
                        // already covered (their seq <= recovered local_seq).
                        // Only re-process deltas that are genuinely ahead.
                        let buffered = recovering.lock().unwrap().remove(&repo_identity).unwrap_or_default();
                        let recovered_seq = local_seqs.read().unwrap().get(&repo_identity).copied();
                        for buffered_event in buffered {
                            let dominated = match &buffered_event {
                                DaemonEvent::RepoDelta(d) => recovered_seq.is_some_and(|rs| d.seq <= rs),
                                _ => false,
                            };
                            if dominated {
                                debug!("discarding buffered delta already covered by recovery");
                                continue;
                            }
                            handle_event(buffered_event, &local_seqs, &recovering, &event_tx, &writer, &pending, &next_id);
                        }
                    });
                }
            }
        }
        DaemonEvent::RepoUntracked { repo_identity, .. } => {
            // Sync lock: evict before dispatching
            local_seqs.write().unwrap().remove(repo_identity);
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
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>,
    next_id: &AtomicU64,
) {
    let last_seen = {
        let seqs = local_seqs.read().unwrap();
        seqs.iter().map(|(repo_identity, &seq)| (repo_identity.clone(), seq)).collect::<HashMap<_, _>>()
    };

    let last_seen = encode_replay_cursors(&last_seen);
    let resp = send_request(writer, pending, next_id, Request::ReplaySince { last_seen }).await;

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
                                let current = seqs.get(&snap.repo_identity).copied().unwrap_or(0);
                                if snap.seq >= current {
                                    seqs.insert(snap.repo_identity.clone(), snap.seq);
                                }
                            }
                            DaemonEvent::RepoDelta(delta) => {
                                let current = seqs.get(&delta.repo_identity).copied().unwrap_or(0);
                                if delta.seq >= current {
                                    seqs.insert(delta.repo_identity.clone(), delta.seq);
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

    async fn get_state(&self, repo: &Path) -> Result<RepoSnapshot, String> {
        // Always RPC to server — local state only tracks seqs for gap detection,
        // not full snapshots (work_items can't be materialized client-side).
        match into_success_response(self.request(Request::GetState { repo: repo.to_path_buf() }).await?)? {
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

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        match into_success_response(self.request(Request::Cancel { command_id }).await?)? {
            Response::Cancel => Ok(()),
            other => Err(format!("unexpected response for cancel: {other:?}")),
        }
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        match into_success_response(self.request(Request::Refresh { repo: repo.to_path_buf() }).await?)? {
            Response::Refresh => Ok(()),
            other => Err(format!("unexpected response for refresh: {other:?}")),
        }
    }

    async fn add_repo(&self, path: &Path) -> Result<(), String> {
        match into_success_response(self.request(Request::AddRepo { path: path.to_path_buf() }).await?)? {
            Response::AddRepo => Ok(()),
            other => Err(format!("unexpected response for add_repo: {other:?}")),
        }
    }

    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        match into_success_response(self.request(Request::RemoveRepo { path: path.to_path_buf() }).await?)? {
            Response::RemoveRepo => Ok(()),
            other => Err(format!("unexpected response for remove_repo: {other:?}")),
        }
    }

    async fn replay_since(&self, last_seen: &HashMap<RepoIdentity, u64>) -> Result<Vec<DaemonEvent>, String> {
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
                let (identity, seq) = match event {
                    DaemonEvent::RepoSnapshot(snap) => (snap.repo_identity.clone(), snap.seq),
                    DaemonEvent::RepoDelta(delta) => (delta.repo_identity.clone(), delta.seq),
                    _ => continue,
                };
                seqs.entry(identity).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
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

    async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String> {
        match into_success_response(self.request(Request::GetRepoDetail { slug: slug.to_string() }).await?)? {
            Response::GetRepoDetail(detail) => Ok(detail),
            other => Err(format!("unexpected response for get_repo_detail: {other:?}")),
        }
    }

    async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String> {
        match into_success_response(self.request(Request::GetRepoProviders { slug: slug.to_string() }).await?)? {
            Response::GetRepoProviders(providers) => Ok(providers),
            other => Err(format!("unexpected response for get_repo_providers: {other:?}")),
        }
    }

    async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String> {
        match into_success_response(self.request(Request::GetRepoWork { slug: slug.to_string() }).await?)? {
            Response::GetRepoWork(work) => Ok(work),
            other => Err(format!("unexpected response for get_repo_work: {other:?}")),
        }
    }

    async fn list_hosts(&self) -> Result<HostListResponse, String> {
        match into_success_response(self.request(Request::ListHosts).await?)? {
            Response::ListHosts(hosts) => Ok(hosts),
            other => Err(format!("unexpected response for list_hosts: {other:?}")),
        }
    }

    async fn get_host_status(&self, host: &str) -> Result<HostStatusResponse, String> {
        match into_success_response(self.request(Request::GetHostStatus { host: host.to_string() }).await?)? {
            Response::GetHostStatus(status) => Ok(status),
            other => Err(format!("unexpected response for get_host_status: {other:?}")),
        }
    }

    async fn get_host_providers(&self, host: &str) -> Result<HostProvidersResponse, String> {
        match into_success_response(self.request(Request::GetHostProviders { host: host.to_string() }).await?)? {
            Response::GetHostProviders(providers) => Ok(providers),
            other => Err(format!("unexpected response for get_host_providers: {other:?}")),
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
mod tests {
    use flotilla_protocol::{HostName, RepoDelta, RepoIdentity, RepoSnapshot};
    use tokio::net::{unix::OwnedReadHalf, UnixStream};

    use super::*;

    type SharedWriter = Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>;
    type SharedPending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>>;
    type RequestLines = tokio::io::Lines<BufReader<UnixStream>>;

    struct RequestHarness {
        writer: SharedWriter,
        pending: SharedPending,
        next_id: Arc<AtomicU64>,
        lines: RequestLines,
    }

    fn repo_identity() -> RepoIdentity {
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
    }

    fn make_snapshot(repo: &Path, seq: u64) -> RepoSnapshot {
        RepoSnapshot {
            seq,
            repo_identity: repo_identity(),
            repo: repo.to_path_buf(),
            host_name: flotilla_protocol::HostName::new("test-host"),
            work_items: vec![],
            providers: flotilla_protocol::ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        }
    }

    fn make_delta(repo: &Path, prev_seq: u64, seq: u64) -> RepoDelta {
        RepoDelta {
            seq,
            prev_seq,
            repo_identity: repo_identity(),
            repo: repo.to_path_buf(),
            changes: vec![],
            work_items: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        }
    }

    fn request_harness() -> RequestHarness {
        let (client, server) = UnixStream::pair().expect("pair");
        let (_read_half, write_half) = client.into_split();
        RequestHarness {
            writer: Arc::new(Mutex::new(BufWriter::new(write_half))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            lines: BufReader::new(server).lines(),
        }
    }

    fn broken_request_harness() -> (SharedWriter, SharedPending, Arc<AtomicU64>) {
        let (client, server) = UnixStream::pair().expect("pair");
        drop(server);
        let (_read_half, write_half) = client.into_split();
        (Arc::new(Mutex::new(BufWriter::new(write_half))), Arc::new(Mutex::new(HashMap::new())), Arc::new(AtomicU64::new(1)))
    }

    /// Returns a writer/pending/next_id triple for tests that call `handle_event`.
    /// Also returns the server half of the socket pair so it isn't dropped — dropping
    /// it would close the pipe and cause writes on the client half to fail.
    fn event_harness() -> (SharedWriter, SharedPending, Arc<AtomicU64>, OwnedReadHalf) {
        let (client, server) = UnixStream::pair().expect("pair");
        let (server_read, _server_write) = server.into_split();
        let (_read_half, write_half) = client.into_split();
        (Arc::new(Mutex::new(BufWriter::new(write_half))), Arc::new(Mutex::new(HashMap::new())), Arc::new(AtomicU64::new(1)), server_read)
    }

    async fn read_request(lines: &mut RequestLines) -> (u64, Request) {
        let line = lines.next_line().await.expect("read request line").expect("line missing");
        match serde_json::from_str::<Message>(&line).expect("parse request") {
            Message::Request { id, request } => (id, request),
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_request_writes_message_and_returns_pending_response() {
        let mut harness = request_harness();

        let request_writer = Arc::clone(&harness.writer);
        let request_pending = Arc::clone(&harness.pending);
        let request_next_id = Arc::clone(&harness.next_id);
        let request_task =
            tokio::spawn(async move { send_request(&request_writer, &request_pending, &request_next_id, Request::ListRepos).await });

        let (id, request) = read_request(&mut harness.lines).await;
        assert_eq!(request, Request::ListRepos);

        let tx = harness.pending.lock().await.remove(&id).expect("pending sender should exist");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ListRepos(vec![])) }).expect("send response");

        let response = request_task.await.expect("join").expect("send_request");
        assert!(
            matches!(response, ResponseResult::Ok { response } if matches!(&*response, Response::ListRepos(repos) if repos.is_empty()))
        );
    }

    #[tokio::test]
    async fn send_request_returns_cancelled_when_sender_is_dropped() {
        let mut harness = request_harness();

        let request_writer = Arc::clone(&harness.writer);
        let request_pending = Arc::clone(&harness.pending);
        let request_next_id = Arc::clone(&harness.next_id);
        let task =
            tokio::spawn(async move { send_request(&request_writer, &request_pending, &request_next_id, Request::GetTopology).await });

        let (id, request) = read_request(&mut harness.lines).await;
        assert_eq!(request, Request::GetTopology);

        harness.pending.lock().await.remove(&id);
        let err = task.await.expect("join").expect_err("dropping sender should cancel request");
        assert!(err.contains("cancelled"));
    }

    #[tokio::test]
    async fn send_request_cleans_pending_on_write_error() {
        let (writer, pending, next_id) = broken_request_harness();

        let err = send_request(&writer, &pending, &next_id, Request::GetTopology).await.expect_err("closed peer should fail writes");

        assert!(err.contains("failed to"));
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn send_request_cleans_pending_on_cancelled_response() {
        let mut harness = request_harness();

        let request_writer = Arc::clone(&harness.writer);
        let request_pending = Arc::clone(&harness.pending);
        let request_next_id = Arc::clone(&harness.next_id);
        let task = tokio::spawn(async move { send_request(&request_writer, &request_pending, &request_next_id, Request::GetStatus).await });

        let (id, request) = read_request(&mut harness.lines).await;
        assert_eq!(request, Request::GetStatus);

        harness.pending.lock().await.remove(&id);

        let err = task.await.expect("join").expect_err("dropping sender should cancel request");
        assert!(err.contains("cancelled"));
        assert!(harness.pending.lock().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn send_request_cleans_pending_on_timeout() {
        let mut harness = request_harness();

        let request_writer = Arc::clone(&harness.writer);
        let request_pending = Arc::clone(&harness.pending);
        let request_next_id = Arc::clone(&harness.next_id);
        let task = tokio::spawn(async move { send_request(&request_writer, &request_pending, &request_next_id, Request::ListHosts).await });

        let (id, request) = read_request(&mut harness.lines).await;
        assert_eq!(id, 1);
        assert_eq!(request, Request::ListHosts);

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        let err = task.await.expect("join").expect_err("missing response should time out");
        assert!(err.contains("timed out"));
        assert!(harness.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn handle_event_updates_local_seqs_for_full_and_matching_delta() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::RepoSnapshot(Box::new(make_snapshot(&repo, 10))),
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );
        handle_event(
            DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 10, 11))),
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let first = event_rx.recv().await.expect("event");
        assert!(matches!(first, DaemonEvent::RepoSnapshot(_)));
        let second = event_rx.recv().await.expect("event");
        assert!(matches!(second, DaemonEvent::RepoDelta(_)));
        assert_eq!(local_seqs.read().unwrap().get(&repo_identity()), Some(&11));
    }

    #[tokio::test]
    async fn handle_event_buffers_delta_when_recovery_already_running() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().unwrap().insert(repo_identity(), 1);
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        recovering.lock().unwrap().insert(repo_identity(), vec![]);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 99, 100))),
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let buffered = recovering.lock().unwrap().get(&repo_identity()).cloned().expect("buffer exists");
        assert_eq!(buffered.len(), 1);
        assert!(event_rx.try_recv().is_err(), "buffered delta should not dispatch");
    }

    #[tokio::test]
    async fn recover_from_gap_requests_replay_and_applies_seqs() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().unwrap().insert(repo_identity(), 3);
        let (event_tx, mut event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        let replay_events = vec![DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 3, 4)))];
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender for replay");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");
        recover_task.await.expect("join recover");

        let event = event_rx.recv().await.expect("forwarded replay event");
        assert!(matches!(event, DaemonEvent::RepoDelta(_)));
        assert_eq!(local_seqs.read().unwrap().get(&repo_identity()), Some(&4));
    }

    #[test]
    fn acquire_spawn_lock_waiter_blocks_then_returns_none() {
        let unique = format!(
            "flotilla-client-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_nanos()
        );
        let lock_path = std::env::temp_dir().join(unique).join("daemon.sock.lock");
        let holder = acquire_spawn_lock(&lock_path).expect("acquire first lock").expect("first call should become spawner");

        // Use a barrier so we know the waiter thread has started before
        // checking is_finished(). Without this, a short sleep could pass
        // vacuously on a loaded machine (thread not yet scheduled).
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_clone = Arc::clone(&barrier);
        let lock_path_clone = lock_path.clone();
        let waiter = std::thread::spawn(move || {
            barrier_clone.wait();
            acquire_spawn_lock(&lock_path_clone).unwrap()
        });
        barrier.wait();
        // Waiter has started; give it time to enter flock() (a single syscall).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished(), "waiter should block while lock is held");

        drop(holder);
        let waiter_result = waiter.join().expect("join waiter");
        assert!(waiter_result.is_none(), "waiter should return None after spawner releases lock");
        let _ = std::fs::remove_file(&lock_path);
    }

    // --- Gap detection: delta for unknown repo triggers recovery ---

    #[tokio::test]
    async fn handle_event_starts_recovery_for_unknown_repo_delta() {
        let repo = PathBuf::from("/tmp/unknown-repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, _event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        // Delta for a repo we have no local seq for — should start recovery.
        handle_event(
            DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 0, 1))),
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        // Recovery should have been initiated: repo should appear in recovering map.
        let guard = recovering.lock().expect("recovering mutex not poisoned");
        assert!(guard.contains_key(&repo_identity()), "unknown repo delta should start recovery");
    }

    // --- Gap detection: delta with seq gap triggers recovery ---

    #[tokio::test]
    async fn handle_event_starts_recovery_on_seq_gap() {
        let repo = PathBuf::from("/tmp/gap-repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 5);
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, _event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        // Delta with prev_seq=10 but local is 5 — gap.
        handle_event(
            DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 10, 11))),
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let guard = recovering.lock().expect("recovering mutex not poisoned");
        assert!(guard.contains_key(&repo_identity()), "seq gap should start recovery");
        // The triggering delta should be buffered as the first entry.
        let buffered = guard.get(&repo_identity()).expect("buffered events");
        assert_eq!(buffered.len(), 1);
    }

    // --- Non-snapshot events pass through unchanged ---

    #[tokio::test]
    async fn handle_event_forwards_repo_added() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        let repo_info = flotilla_protocol::RepoInfo {
            identity: repo_identity(),
            path: PathBuf::from("/tmp/new-repo"),
            name: "new-repo".into(),
            labels: flotilla_protocol::RepoLabels::default(),
            provider_names: HashMap::new(),
            provider_health: HashMap::new(),
            loading: false,
        };
        handle_event(DaemonEvent::RepoTracked(Box::new(repo_info)), &local_seqs, &recovering, &event_tx, &writer, &pending, &next_id);

        let event = event_rx.try_recv().expect("should receive RepoTracked");
        assert!(matches!(event, DaemonEvent::RepoTracked(_)));
    }

    #[tokio::test]
    async fn handle_event_forwards_command_started() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::CommandStarted {
                command_id: 42,
                host: flotilla_protocol::HostName::new("test"),
                repo: PathBuf::from("/tmp/repo"),
                repo_identity: repo_identity(),
                description: "testing".into(),
            },
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let event = event_rx.try_recv().expect("should receive CommandStarted");
        assert!(matches!(event, DaemonEvent::CommandStarted { command_id: 42, .. }));
    }

    #[tokio::test]
    async fn handle_event_forwards_command_finished() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::CommandFinished {
                command_id: 7,
                host: flotilla_protocol::HostName::new("host"),
                repo: PathBuf::from("/tmp/repo"),
                repo_identity: repo_identity(),
                result: flotilla_protocol::commands::CommandResult::Ok,
            },
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let event = event_rx.try_recv().expect("should receive CommandFinished");
        assert!(matches!(event, DaemonEvent::CommandFinished { command_id: 7, .. }));
    }

    #[tokio::test]
    async fn handle_event_forwards_command_step_update() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::CommandStepUpdate {
                command_id: 3,
                host: flotilla_protocol::HostName::new("host"),
                repo: PathBuf::from("/tmp/repo"),
                repo_identity: repo_identity(),
                step_index: 1,
                step_count: 3,
                description: "step 2".into(),
                status: flotilla_protocol::commands::StepStatus::Started,
            },
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let event = event_rx.try_recv().expect("should receive CommandStepUpdate");
        assert!(matches!(event, DaemonEvent::CommandStepUpdate { command_id: 3, .. }));
    }

    #[tokio::test]
    async fn handle_event_forwards_peer_status_changed() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::PeerStatusChanged {
                host: flotilla_protocol::HostName::new("peer-1"),
                status: flotilla_protocol::PeerConnectionState::Connected,
            },
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        let event = event_rx.try_recv().expect("should receive PeerStatusChanged");
        assert!(matches!(event, DaemonEvent::PeerStatusChanged { .. }));
    }

    // --- RepoRemoved evicts seq and forwards ---

    #[tokio::test]
    async fn handle_event_repo_removed_evicts_seq_and_forwards() {
        let repo = PathBuf::from("/tmp/removed-repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 42);
        let recovering: Arc<std::sync::Mutex<HashMap<RepoIdentity, Vec<DaemonEvent>>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let (writer, pending, next_id, _server) = event_harness();

        handle_event(
            DaemonEvent::RepoUntracked { path: repo.clone(), repo_identity: repo_identity() },
            &local_seqs,
            &recovering,
            &event_tx,
            &writer,
            &pending,
            &next_id,
        );

        // Seq should be evicted.
        assert!(
            local_seqs.read().expect("local_seqs read lock").get(&repo_identity()).is_none(),
            "seq should be evicted for untracked repo"
        );
        // Event should be forwarded.
        let event = event_rx.try_recv().expect("should receive RepoUntracked");
        assert!(matches!(event, DaemonEvent::RepoUntracked { .. }));
    }

    // --- recover_from_gap: parse error in replay response ---

    #[tokio::test]
    async fn recover_from_gap_handles_parse_error_gracefully() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 3);
        let (event_tx, _event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Respond with the wrong success variant.
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ListHosts(HostListResponse { hosts: vec![] })) })
            .expect("send bad response");

        // Should complete without panic.
        recover_task.await.expect("recover_from_gap should not panic on unexpected response variant");
        // Local seqs should remain unchanged.
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&3));
    }

    // --- recover_from_gap: request write failure ---

    #[tokio::test]
    async fn recover_from_gap_handles_request_failure_gracefully() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 5);
        let (event_tx, _event_rx) = broadcast::channel(16);

        let (writer, pending, next_id) = broken_request_harness();

        // recover_from_gap should complete without panic even when the request fails.
        recover_from_gap(&local_seqs, &event_tx, &writer, &pending, &next_id).await;
        // Local seqs should remain unchanged.
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&5));
    }

    // --- recover_from_gap: response with ok=false ---

    #[tokio::test]
    async fn recover_from_gap_handles_error_response_gracefully() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 7);
        let (event_tx, _event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Respond with a protocol error.
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Err { message: "internal error".into() }).expect("send error response");

        recover_task.await.expect("recover_from_gap should not panic on error response");
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&7));
    }

    // --- recover_from_gap: replay with RepoSnapshot updates seqs ---

    #[tokio::test]
    async fn recover_from_gap_applies_full_snapshot_seqs() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 2);
        let (event_tx, mut event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Respond with a full snapshot event.
        let replay_events = vec![DaemonEvent::RepoSnapshot(Box::new(make_snapshot(&repo, 10)))];
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

        recover_task.await.expect("join recover");

        let event = event_rx.recv().await.expect("forwarded replay event");
        assert!(matches!(event, DaemonEvent::RepoSnapshot(_)));
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&10));
    }

    // --- recover_from_gap: monotonic seq update (live event advances while replay in flight) ---

    #[tokio::test]
    async fn recover_from_gap_does_not_regress_seq_from_concurrent_live_update() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 3);
        let (event_tx, _event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let identity = repo_identity();
        let recover_task = tokio::spawn(async move {
            // Set seq high before recovery starts to validate the monotonic guard.
            recover_local_seqs.write().expect("local_seqs write lock").insert(identity, 20);
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Replay returns events with seq=10, which is behind the live update of 20.
        let replay_events = vec![DaemonEvent::RepoDelta(Box::new(make_delta(&repo, 3, 10)))];
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

        recover_task.await.expect("join recover");

        // Seq should not regress: should remain at 20 (the higher value).
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&20));
    }

    // --- recover_from_gap: replay with non-snapshot events (e.g. CommandStarted) ---

    #[tokio::test]
    async fn recover_from_gap_forwards_non_snapshot_replay_events() {
        let repo = PathBuf::from("/tmp/repo");
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 1);
        let (event_tx, mut event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Replay includes a non-snapshot event — it should still be forwarded.
        let replay_events = vec![DaemonEvent::CommandStarted {
            command_id: 99,
            host: flotilla_protocol::HostName::new("test"),
            repo: repo.clone(),
            repo_identity: repo_identity(),
            description: "replayed".into(),
        }];
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

        recover_task.await.expect("join recover");

        let event = event_rx.recv().await.expect("should receive non-snapshot replay event");
        assert!(matches!(event, DaemonEvent::CommandStarted { command_id: 99, .. }));
        // Seq should be unchanged since CommandStarted doesn't carry a seq.
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&1));
    }

    // --- recover_from_gap: empty replay events ---

    #[tokio::test]
    async fn recover_from_gap_handles_empty_replay() {
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        local_seqs.write().expect("local_seqs write lock").insert(repo_identity(), 5);
        let (event_tx, mut event_rx) = broadcast::channel(16);

        let mut harness = request_harness();

        let recover_local_seqs = Arc::clone(&local_seqs);
        let recover_event_tx = event_tx.clone();
        let recover_writer = Arc::clone(&harness.writer);
        let recover_pending = Arc::clone(&harness.pending);
        let recover_next_id = Arc::clone(&harness.next_id);
        let recover_task = tokio::spawn(async move {
            recover_from_gap(&recover_local_seqs, &recover_event_tx, &recover_writer, &recover_pending, &recover_next_id).await;
        });

        let (id, request) = read_request(&mut harness.lines).await;
        assert!(matches!(request, Request::ReplaySince { .. }));

        // Respond with empty event list.
        let replay_events: Vec<DaemonEvent> = vec![];
        let tx = harness.pending.lock().await.remove(&id).expect("pending sender");
        tx.send(ResponseResult::Ok { response: Box::new(Response::ReplaySince(replay_events)) }).expect("send replay response");

        recover_task.await.expect("join recover");

        // No events forwarded.
        assert!(event_rx.try_recv().is_err(), "no events should be forwarded for empty replay");
        // Seq unchanged.
        assert_eq!(local_seqs.read().expect("local_seqs read lock").get(&repo_identity()), Some(&5));
    }

    // --- ResponseResult helper paths ---

    #[test]
    fn into_success_response_returns_error_for_protocol_error() {
        let err = into_success_response(ResponseResult::Err { message: "something broke".into() }).expect_err("should fail");
        assert!(err.contains("something broke"));
    }

    #[test]
    fn into_success_response_returns_response_for_success() {
        let response = into_success_response(ResponseResult::Ok {
            response: Box::new(Response::GetTopology(TopologyResponse { local_host: HostName::new("local"), routes: vec![] })),
        })
        .expect("should succeed");
        assert!(matches!(response, Response::GetTopology(TopologyResponse { routes, .. }) if routes.is_empty()));
    }
}

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
