use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{debug, error, warn};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{Command, DaemonEvent, Message, RawResponse, RepoInfo, Snapshot};

/// Std RwLock for local seq tracking — the critical sections are single HashMap
/// operations (no async work while holding the lock), and using a sync lock
/// avoids the race where a spawned seq update hasn't run before the next delta
/// arrives.
type SeqMap = std::sync::RwLock<HashMap<PathBuf, u64>>;

pub struct SocketDaemon {
    writer: Arc<Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>>,
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
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;

        let (read_half, write_half) = stream.into_split();

        let (event_tx, _) = broadcast::channel(256);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let local_seqs: Arc<SeqMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let recovering: Arc<std::sync::Mutex<HashSet<PathBuf>>> =
            Arc::new(std::sync::Mutex::new(HashSet::new()));

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
    local_seqs: &Arc<SeqMap>,
    recovering: &Arc<std::sync::Mutex<HashSet<PathBuf>>>,
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
            // Sync lock: update seq before dispatching event so a
            // quickly-following delta sees the correct local seq.
            local_seqs
                .write()
                .unwrap()
                .insert(snap.repo.clone(), snap.seq);
            let _ = event_tx.send(event);
        }
        DaemonEvent::SnapshotDelta(delta) => {
            let repo = delta.repo.clone();
            let prev_seq = delta.prev_seq;
            let seq = delta.seq;

            // Check seq under sync lock, then spawn only if recovery needed.
            let local_seq = local_seqs.read().unwrap().get(&repo).copied();

            match local_seq {
                Some(ls) if prev_seq == ls => {
                    // Happy path: apply delta (sync lock, no spawn needed)
                    local_seqs.write().unwrap().insert(repo.clone(), seq);
                    debug!(
                        "applied delta for {} (seq {} → {})",
                        repo.display(),
                        prev_seq,
                        seq
                    );
                    let _ = event_tx.send(event);
                }
                _ => {
                    // Seq gap or unknown repo — spawn recovery if not already in progress.
                    // Guard prevents concurrent recoveries for the same repo from
                    // interleaving stale replay events with newer state.
                    let already_recovering = !recovering.lock().unwrap().insert(repo.clone());
                    if already_recovering {
                        debug!(
                            "recovery already in progress for {}, skipping",
                            repo.display()
                        );
                        return;
                    }

                    if let Some(ls) = local_seq {
                        warn!(
                            "seq gap for {} (local={}, delta prev_seq={}), requesting replay",
                            repo.display(),
                            ls,
                            prev_seq
                        );
                    } else {
                        warn!(
                            "received delta for unknown repo {}, requesting replay",
                            repo.display()
                        );
                    }

                    let local_seqs = Arc::clone(local_seqs);
                    let recovering = Arc::clone(recovering);
                    let event_tx = event_tx.clone();
                    let writer = Arc::clone(writer);
                    let pending = Arc::clone(pending);
                    let next_id = Arc::clone(next_id);

                    tokio::spawn(async move {
                        recover_from_gap(
                            &local_seqs,
                            &event_tx,
                            &writer,
                            &pending,
                            &next_id,
                        )
                        .await;
                        recovering.lock().unwrap().remove(&repo);
                    });
                }
            }
        }
        DaemonEvent::RepoRemoved { path } => {
            // Sync lock: evict before dispatching
            local_seqs.write().unwrap().remove(path);
            let _ = event_tx.send(event);
        }
        DaemonEvent::RepoAdded(_)
        | DaemonEvent::CommandStarted { .. }
        | DaemonEvent::CommandFinished { .. } => {
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
    pending: &Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>,
    next_id: &AtomicU64,
) {
    let last_seen = {
        let seqs = local_seqs.read().unwrap();
        seqs.iter()
            .map(|(path, &seq)| (path.clone(), seq))
            .collect::<HashMap<_, _>>()
    };

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
                debug!("gap recovery: got {} replay events", events.len());
                {
                    let mut seqs = local_seqs.write().unwrap();
                    for event in &events {
                        match event {
                            DaemonEvent::SnapshotFull(snap) => {
                                seqs.insert(snap.repo.clone(), snap.seq);
                            }
                            DaemonEvent::SnapshotDelta(delta) => {
                                seqs.insert(delta.repo.clone(), delta.seq);
                            }
                            _ => {}
                        }
                    }
                }
                for event in events {
                    let _ = event_tx.send(event);
                }
            }
            Err(e) => {
                error!("gap recovery: failed to parse replay events: {e}");
            }
        },
        Err(e) => {
            error!("gap recovery: replay_since request failed: {e}");
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

    async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
        let resp = self
            .request(
                "execute",
                serde_json::json!({ "repo": repo, "command": command }),
            )
            .await?;
        resp.parse::<u64>()
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
        let events: Vec<DaemonEvent> = resp.parse()?;

        // Seed local_seqs from replay events so the background reader
        // doesn't trigger spurious gap recovery for the first live delta.
        {
            let mut seqs = self.local_seqs.write().unwrap();
            for event in &events {
                match event {
                    DaemonEvent::SnapshotFull(snap) => {
                        seqs.insert(snap.repo.clone(), snap.seq);
                    }
                    DaemonEvent::SnapshotDelta(delta) => {
                        seqs.insert(delta.repo.clone(), delta.seq);
                    }
                    _ => {}
                }
            }
        }

        Ok(events)
    }
}
