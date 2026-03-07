use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{error, warn};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{Command, CommandResult, DaemonEvent, Message, RawResponse, RepoInfo, Snapshot};

pub struct SocketDaemon {
    writer: Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: AtomicU64,
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

        let daemon = Arc::new(Self {
            writer: Mutex::new(BufWriter::new(write_half)),
            pending: Arc::clone(&pending),
            event_tx: event_tx.clone(),
            next_id: AtomicU64::new(1),
        });

        // Spawn background reader task
        let reader_pending = Arc::clone(&pending);
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
                                // event is Box<DaemonEvent>, unbox before sending
                                let _ = event_tx.send(*event);
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();

        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }

        let msg = Message::Request {
            id,
            method: method.to_string(),
            params,
        };

        let line = serde_json::to_string(&msg)
            .map_err(|e| format!("failed to serialize request: {e}"))?;

        {
            let mut writer = self.writer.lock().await;
            writer
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("failed to write to daemon socket: {e}"))?;
            writer
                .write_all(b"\n")
                .await
                .map_err(|e| format!("failed to write newline to daemon socket: {e}"))?;
            writer
                .flush()
                .await
                .map_err(|e| format!("failed to flush daemon socket: {e}"))?;
        }

        rx.await.map_err(|_| "request cancelled (sender dropped)".to_string())
    }
}

#[async_trait]
impl DaemonHandle for SocketDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        let resp = self
            .request("get_state", serde_json::json!({ "repo": repo }))
            .await?;
        resp.parse::<Snapshot>()
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let resp = self
            .request("list_repos", serde_json::json!({}))
            .await?;
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
}
