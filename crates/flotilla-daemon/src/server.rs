use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{Command, Message};

/// The daemon server that listens on a Unix socket and dispatches requests
/// to an `InProcessDaemon`.
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    client_count: Arc<AtomicUsize>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last client disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Self {
        let config = Arc::new(ConfigStore::new());
        let daemon = InProcessDaemon::new(repo_paths, config).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            daemon,
            socket_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Run the server, accepting connections until idle timeout or shutdown signal.
    pub async fn run(self) -> Result<(), String> {
        // Clean up stale socket file before binding
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| format!("failed to remove stale socket: {e}"))?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create socket directory: {e}"))?;
        }

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| format!("failed to bind socket: {e}"))?;

        info!("daemon listening on {}", self.socket_path.display());

        let daemon = self.daemon;
        let client_count = self.client_count;
        let shutdown_tx = self.shutdown_tx;
        let mut shutdown_rx = self.shutdown_rx;
        let idle_timeout = self.idle_timeout;
        let socket_path = self.socket_path.clone();

        // Spawn idle timeout watcher
        let idle_client_count = Arc::clone(&client_count);
        let idle_shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            // Wait until at least one client has connected and then disconnected,
            // or if we start with zero clients, begin the idle timer immediately.
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let count = idle_client_count.load(Ordering::SeqCst);
                if count == 0 {
                    info!(
                        "no clients connected, waiting {} seconds before shutdown",
                        idle_timeout.as_secs()
                    );
                    tokio::time::sleep(idle_timeout).await;
                    // Re-check: a client may have connected during the wait
                    if idle_client_count.load(Ordering::SeqCst) == 0 {
                        info!("idle timeout reached, shutting down");
                        let _ = idle_shutdown_tx.send(true);
                        break;
                    }
                }
            }
        });

        // SIGTERM handler
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");

        // Accept loop
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let count = client_count.fetch_add(1, Ordering::SeqCst) + 1;
                            info!("client connected (total: {count})");

                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let shutdown_rx = shutdown_rx.clone();

                            tokio::spawn(async move {
                                handle_client(stream, daemon, shutdown_rx).await;
                                let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
                                info!("client disconnected (total: {count})");
                            });
                        }
                        Err(e) => {
                            error!("failed to accept connection: {e}");
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("shutdown signal received");
                        break;
                    }
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

        // Clean up socket file on shutdown
        if let Err(e) = std::fs::remove_file(&socket_path) {
            warn!("failed to remove socket file on shutdown: {e}");
        }

        info!("daemon server stopped");
        Ok(())
    }
}

/// Write a JSON message followed by a newline to the writer.
async fn write_message(
    writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    msg: &Message,
) -> Result<(), ()> {
    let mut w = writer.lock().await;
    let json = serde_json::to_string(msg).map_err(|_| ())?;
    w.write_all(json.as_bytes()).await.map_err(|_| ())?;
    w.write_all(b"\n").await.map_err(|_| ())?;
    w.flush().await.map_err(|_| ())?;
    Ok(())
}

/// Handle a single client connection.
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write_half)));

    // Spawn event forwarder task
    let event_writer = Arc::clone(&writer);
    let mut event_rx = daemon.subscribe();
    let event_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let msg = Message::Event {
                        event: Box::new(event),
                    };
                    if write_message(&event_writer, &msg).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("event subscriber lagged, skipped {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Read request lines and dispatch
    let mut lines = reader.lines();
    loop {
        tokio::select! {
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        let msg: Message = match serde_json::from_str(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("failed to parse message: {e}");
                                continue;
                            }
                        };

                        match msg {
                            Message::Request { id, method, params } => {
                                let response = dispatch_request(&daemon, id, &method, params).await;
                                if write_message(&writer, &response).await.is_err() {
                                    break;
                                }
                            }
                            other => {
                                warn!("unexpected message type from client: {:?}", other);
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — client disconnected
                        break;
                    }
                    Err(e) => {
                        error!("error reading from client: {e}");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Abort the event forwarder task
    event_task.abort();
}

/// Dispatch a request to the appropriate `DaemonHandle` method.
async fn dispatch_request(
    daemon: &Arc<InProcessDaemon>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Message {
    match method {
        "list_repos" => match daemon.list_repos().await {
            Ok(repos) => Message::ok_response(id, &repos),
            Err(e) => Message::error_response(id, e),
        },

        "get_state" => {
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.get_state(&repo).await {
                Ok(snapshot) => Message::ok_response(id, &snapshot),
                Err(e) => Message::error_response(id, e),
            }
        }

        "execute" => {
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            let command: Command = match params
                .get("command")
                .cloned()
                .ok_or_else(|| "missing 'command' field".to_string())
                .and_then(|v| {
                    serde_json::from_value(v).map_err(|e| format!("invalid command: {e}"))
                }) {
                Ok(cmd) => cmd,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.execute(&repo, command).await {
                Ok(result) => Message::ok_response(id, &result),
                Err(e) => Message::error_response(id, e),
            }
        }

        "refresh" => {
            let repo = match extract_repo_path(&params) {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.refresh(&repo).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "add_repo" => {
            let path = match extract_path_param(&params, "path") {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.add_repo(&path).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "remove_repo" => {
            let path = match extract_path_param(&params, "path") {
                Ok(p) => p,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.remove_repo(&path).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }

        unknown => Message::error_response(id, format!("unknown method: {unknown}")),
    }
}

/// Extract the "repo" field from params as a PathBuf.
fn extract_repo_path(params: &serde_json::Value) -> Result<PathBuf, String> {
    extract_path_param(params, "repo")
}

/// Extract a named path field from params as a PathBuf.
fn extract_path_param(params: &serde_json::Value, field: &str) -> Result<PathBuf, String> {
    params
        .get(field)
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing '{field}' parameter"))
}
