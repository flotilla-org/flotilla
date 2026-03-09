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
    /// Local snapshot seq per repo, for gap detection.
    /// Maintained by the background reader; not used for get_state.
    /// Field is read via Arc::clone in connect() for the reader task.
    #[allow(dead_code)]
    local_seqs: Arc<RwLock<HashMap<PathBuf, u64>>>,
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
        let local_seqs: Arc<RwLock<HashMap<PathBuf, u64>>> = Arc::new(RwLock::new(HashMap::new()));

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
                                    &reader_local_seqs,
                                    &reader_event_tx,
                                    &reader_writer,
                                    &reader_pending,
                                    &reader_next_id,
                                );
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
/// background recovery task can use it.
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

/// Handle a daemon event in the background reader: update local seq tracking,
/// forward to TUI subscribers, and spawn gap recovery if needed.
///
/// This function is non-async and never blocks the reader loop. Gap recovery
/// is spawned on a separate task to avoid deadlocking the reader (which must
/// remain free to route the recovery response).
fn handle_event(
    event: DaemonEvent,
    local_seqs: &Arc<RwLock<HashMap<PathBuf, u64>>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    writer: &Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>>,
    next_id: &Arc<AtomicU64>,
) {
    match &event {
        DaemonEvent::SnapshotFull(snap) => {
            debug!(
                "received full snapshot for {} (seq {})",
                snap.repo.display(),
                snap.seq
            );
            let local_seqs = Arc::clone(local_seqs);
            let repo = snap.repo.clone();
            let seq = snap.seq;
            // Spawn seq update to avoid blocking the reader on the write lock
            tokio::spawn(async move {
                local_seqs.write().await.insert(repo, seq);
            });
            let _ = event_tx.send(event);
        }
        DaemonEvent::SnapshotDelta(delta) => {
            let local_seqs_clone = Arc::clone(local_seqs);
            let repo = delta.repo.clone();
            let prev_seq = delta.prev_seq;
            let seq = delta.seq;
            let event_tx = event_tx.clone();
            let writer = Arc::clone(writer);
            let pending = Arc::clone(pending);
            let next_id = Arc::clone(next_id);

            // Spawn delta processing to avoid blocking the reader.
            // The spawned task checks seq, applies or triggers recovery.
            tokio::spawn(async move {
                let mut seqs = local_seqs_clone.write().await;
                let local_seq = seqs.get(&repo).copied();

                match local_seq {
                    Some(ls) if prev_seq == ls => {
                        // Happy path: apply delta
                        seqs.insert(repo.clone(), seq);
                        drop(seqs);
                        debug!(
                            "applied delta for {} (seq {} → {})",
                            repo.display(),
                            prev_seq,
                            seq
                        );
                        let _ = event_tx.send(event);
                    }
                    Some(ls) => {
                        // Seq gap
                        warn!(
                            "seq gap for {} (local={}, delta prev_seq={}), requesting full snapshot",
                            repo.display(),
                            ls,
                            prev_seq
                        );
                        drop(seqs);
                        recover_from_gap(
                            &repo,
                            &local_seqs_clone,
                            &event_tx,
                            &writer,
                            &pending,
                            &next_id,
                        )
                        .await;
                    }
                    None => {
                        // No local state for this repo
                        warn!(
                            "received delta for unknown repo {}, requesting full snapshot",
                            repo.display()
                        );
                        drop(seqs);
                        recover_from_gap(
                            &repo,
                            &local_seqs_clone,
                            &event_tx,
                            &writer,
                            &pending,
                            &next_id,
                        )
                        .await;
                    }
                }
            });
        }
        DaemonEvent::RepoRemoved { path } => {
            // Evict local seq tracking for removed repos
            let local_seqs = Arc::clone(local_seqs);
            let path = path.clone();
            tokio::spawn(async move {
                local_seqs.write().await.remove(&path);
            });
            let _ = event_tx.send(event);
        }
        DaemonEvent::RepoAdded(_) | DaemonEvent::CommandResult { .. } => {
            let _ = event_tx.send(event);
        }
    }
}

/// Recover from a seq gap by calling `replay_since` with the stale seq,
/// updating local seq tracking, and forwarding replay events to the TUI.
async fn recover_from_gap(
    repo: &Path,
    local_seqs: &RwLock<HashMap<PathBuf, u64>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    writer: &Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: &Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    next_id: &AtomicU64,
) {
    let stale_seq = local_seqs.read().await.get(repo).copied().unwrap_or(0);
    let last_seen = HashMap::from([(repo.to_path_buf(), stale_seq)]);

    let resp = send_request(
        writer,
        pending,
        next_id,
        "replay_since",
        serde_json::json!({ "last_seen": last_seen }),
    )
    .await;

    match resp {
        Ok(raw) => match raw.parse::<Vec<DaemonEvent>>() {
            Ok(events) => {
                debug!(
                    "gap recovery: got {} replay events for {}",
                    events.len(),
                    repo.display()
                );
                for event in events {
                    // Update local seq from replay events
                    match &event {
                        DaemonEvent::SnapshotFull(snap) if snap.repo == repo => {
                            local_seqs
                                .write()
                                .await
                                .insert(repo.to_path_buf(), snap.seq);
                        }
                        DaemonEvent::SnapshotDelta(delta) if delta.repo == repo => {
                            local_seqs
                                .write()
                                .await
                                .insert(repo.to_path_buf(), delta.seq);
                        }
                        _ => {}
                    }
                    let _ = event_tx.send(event);
                }
            }
            Err(e) => {
                error!(
                    "gap recovery: failed to parse replay events for {}: {e}",
                    repo.display()
                );
            }
        },
        Err(e) => {
            error!(
                "gap recovery: failed replay_since for {}: {e}",
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
        // Always RPC to server — local state only tracks seqs for gap detection,
        // not full snapshots (work_items can't be materialized client-side).
        let resp = self
            .request("get_state", serde_json::json!({ "repo": repo }))
            .await?;
        resp.parse()
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
