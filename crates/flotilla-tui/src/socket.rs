use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use tracing::{debug, error, warn};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    Command, CommandResult, DaemonEvent, Message, RawResponse, RepoInfo, Snapshot,
};

pub struct SocketDaemon {
    writer: Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: Arc<AtomicU64>,
    /// Local snapshot state per repo, maintained by the background reader.
    local_state: Arc<RwLock<HashMap<PathBuf, Snapshot>>>,
}

impl SocketDaemon {
    /// Connect to a running daemon at the given Unix socket path.
    ///
    /// Splits the socket into reader/writer halves, spawns a background task
    /// to read incoming messages, and returns `Arc<Self>`.
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;

        let (read_half, write_half) = stream.into_split();

        let (event_tx, _) = broadcast::channel(256);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let local_state: Arc<RwLock<HashMap<PathBuf, Snapshot>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let writer = Arc::new(Mutex::new(BufWriter::new(write_half)));

        let daemon = Arc::new(Self {
            writer: Arc::clone(&writer),
            pending: Arc::clone(&pending),
            event_tx: event_tx.clone(),
            next_id: Arc::clone(&next_id),
            local_state: Arc::clone(&local_state),
        });

        // Spawn background reader task
        let reader_pending = Arc::clone(&pending);
        let reader_writer = Arc::clone(&writer);
        let reader_next_id = Arc::clone(&next_id);
        let reader_local_state = Arc::clone(&local_state);
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
                                warn!("failed to parse message from daemon: {e}");
                                continue;
                            }
                        };

                        match msg {
                            Message::Response {
                                id,
                                ok,
                                data,
                                error,
                            } => {
                                let raw = RawResponse { ok, data, error };
                                let mut map = reader_pending.lock().await;
                                if let Some(tx) = map.remove(&id) {
                                    let _ = tx.send(raw);
                                } else {
                                    warn!("received response for unknown request id {id}");
                                }
                            }
                            Message::Event { event } => {
                                let event = *event;
                                handle_event(
                                    event,
                                    &reader_local_state,
                                    &reader_event_tx,
                                    &reader_writer,
                                    &reader_pending,
                                    &reader_next_id,
                                )
                                .await;
                            }
                            Message::Request { .. } => {
                                warn!("received unexpected request from daemon");
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — daemon closed connection
                        error!("daemon connection closed (EOF)");
                        let mut map = reader_pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(RawResponse {
                                ok: false,
                                data: None,
                                error: Some("daemon connection closed".into()),
                            });
                        }
                        break;
                    }
                    Err(e) => {
                        error!("error reading from daemon socket: {e}");
                        let mut map = reader_pending.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(RawResponse {
                                ok: false,
                                data: None,
                                error: Some(format!("daemon read error: {e}")),
                            });
                        }
                        break;
                    }
                }
            }
        });

        Ok(daemon)
    }

    /// Send a request to the daemon and wait for the matching response.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RawResponse, String> {
        send_request(&self.writer, &self.pending, &self.next_id, method, params).await
    }
}

/// Send a request on the wire and wait for the response.
///
/// Extracted as a free function so both the SocketDaemon methods and the
/// background reader (for gap recovery) can use it.
async fn send_request(
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    next_id: &AtomicU64,
    method: &str,
    params: serde_json::Value,
) -> Result<RawResponse, String> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    let (tx, rx) = oneshot::channel();

    {
        let mut map = pending.lock().await;
        map.insert(id, tx);
    }

    let msg = Message::Request {
        id,
        method: method.to_string(),
        params,
    };

    let line =
        serde_json::to_string(&msg).map_err(|e| format!("failed to serialize request: {e}"))?;

    {
        let mut w = writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("failed to write to daemon socket: {e}"))?;
        w.write_all(b"\n")
            .await
            .map_err(|e| format!("failed to write newline to daemon socket: {e}"))?;
        w.flush()
            .await
            .map_err(|e| format!("failed to flush daemon socket: {e}"))?;
    }

    tokio::time::timeout(std::time::Duration::from_secs(30), rx)
        .await
        .map_err(|_| "request timed out after 30s".to_string())?
        .map_err(|_| "request cancelled (sender dropped)".to_string())
}

/// Handle a daemon event in the background reader: update local state and
/// forward to TUI subscribers.
async fn handle_event(
    event: DaemonEvent,
    local_state: &RwLock<HashMap<PathBuf, Snapshot>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    next_id: &AtomicU64,
) {
    match &event {
        DaemonEvent::SnapshotFull(snap) => {
            debug!(
                "received full snapshot for {} (seq {})",
                snap.repo.display(),
                snap.seq
            );
            let mut state = local_state.write().await;
            state.insert(snap.repo.clone(), (**snap).clone());
            let _ = event_tx.send(event);
        }
        DaemonEvent::SnapshotDelta(delta) => {
            let mut state = local_state.write().await;
            if let Some(snapshot) = state.get_mut(&delta.repo) {
                if delta.prev_seq == snapshot.seq {
                    // Happy path: apply delta
                    flotilla_core::delta::apply_snapshot_delta(snapshot, delta);
                    debug!(
                        "applied delta for {} (seq {} → {})",
                        delta.repo.display(),
                        delta.prev_seq,
                        delta.seq
                    );
                    let _ = event_tx.send(event);
                } else {
                    // Seq gap: request full snapshot from server
                    warn!(
                        "seq gap for {} (local={}, delta prev_seq={}), requesting full snapshot",
                        delta.repo.display(),
                        snapshot.seq,
                        delta.prev_seq
                    );
                    drop(state);
                    recover_from_gap(&delta.repo, local_state, event_tx, writer, pending, next_id)
                        .await;
                }
            } else {
                // No local state for this repo — request full snapshot
                warn!(
                    "received delta for unknown repo {}, requesting full snapshot",
                    delta.repo.display()
                );
                recover_from_gap(&delta.repo, local_state, event_tx, writer, pending, next_id)
                    .await;
            }
        }
        DaemonEvent::RepoAdded(_) | DaemonEvent::RepoRemoved { .. } => {
            let _ = event_tx.send(event);
        }
        DaemonEvent::CommandResult { .. } => {
            let _ = event_tx.send(event);
        }
    }
}

/// Recover from a seq gap by requesting a full snapshot from the server,
/// updating local state, and emitting a SnapshotFull event to the TUI.
async fn recover_from_gap(
    repo: &Path,
    local_state: &RwLock<HashMap<PathBuf, Snapshot>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    next_id: &AtomicU64,
) {
    let resp = send_request(
        writer,
        pending,
        next_id,
        "get_state",
        serde_json::json!({ "repo": repo }),
    )
    .await;

    match resp {
        Ok(raw) => match raw.parse::<Snapshot>() {
            Ok(snapshot) => {
                debug!(
                    "gap recovery: got full snapshot for {} (seq {})",
                    repo.display(),
                    snapshot.seq
                );
                let mut state = local_state.write().await;
                state.insert(repo.to_path_buf(), snapshot.clone());
                let _ = event_tx.send(DaemonEvent::SnapshotFull(Box::new(snapshot)));
            }
            Err(e) => {
                error!(
                    "gap recovery: failed to parse snapshot for {}: {e}",
                    repo.display()
                );
            }
        },
        Err(e) => {
            error!(
                "gap recovery: failed to request snapshot for {}: {e}",
                repo.display()
            );
        }
    }
}

#[async_trait]
impl DaemonHandle for SocketDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        // Serve from local state if available
        {
            let state = self.local_state.read().await;
            if let Some(snapshot) = state.get(repo) {
                return Ok(snapshot.clone());
            }
        }
        // Fall back to server RPC
        let resp = self
            .request("get_state", serde_json::json!({ "repo": repo }))
            .await?;
        let snapshot: Snapshot = resp.parse()?;
        // Cache the result
        {
            let mut state = self.local_state.write().await;
            state.insert(repo.to_path_buf(), snapshot.clone());
        }
        Ok(snapshot)
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let resp = self.request("list_repos", serde_json::json!({})).await?;
        resp.parse::<Vec<RepoInfo>>()
    }

    async fn execute(&self, repo: &Path, command: Command) -> Result<CommandResult, String> {
        let resp = self
            .request(
                "execute",
                serde_json::json!({ "repo": repo, "command": command }),
            )
            .await?;
        resp.parse::<CommandResult>()
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        let resp = self
            .request("refresh", serde_json::json!({ "repo": repo }))
            .await?;
        resp.parse_empty()
    }

    async fn add_repo(&self, path: &Path) -> Result<(), String> {
        let resp = self
            .request("add_repo", serde_json::json!({ "path": path }))
            .await?;
        resp.parse_empty()
    }

    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let resp = self
            .request("remove_repo", serde_json::json!({ "path": path }))
            .await?;
        resp.parse_empty()
    }

    async fn replay_since(
        &self,
        last_seen: &HashMap<PathBuf, u64>,
    ) -> Result<Vec<DaemonEvent>, String> {
        let resp = self
            .request(
                "replay_since",
                serde_json::json!({ "last_seen": last_seen }),
            )
            .await?;
        resp.parse::<Vec<DaemonEvent>>()
    }
}
