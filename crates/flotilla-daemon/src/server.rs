use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tracing::{debug, error, info, warn};

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{Command, HostName, Message, PeerDataMessage};

/// The daemon server that listens on a Unix socket and dispatches requests
/// to an `InProcessDaemon`.
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Channel for inbound peer data messages forwarded from connected peer clients.
    peer_data_tx: mpsc::Sender<PeerDataMessage>,
    peer_data_rx: Option<mpsc::Receiver<PeerDataMessage>>,
    /// Map of connected peer clients, keyed by their host name.
    /// Each entry holds a sender that can push messages back to that peer's socket.
    peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `repo_paths` — initial repos to track.
    /// `socket_path` — path to the Unix domain socket.
    /// `idle_timeout` — how long to wait after the last client disconnects before shutting down.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Self {
        let daemon = InProcessDaemon::new(repo_paths, config).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_data_tx, peer_data_rx) = mpsc::channel(256);

        Self {
            daemon,
            socket_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_tx,
            shutdown_rx,
            peer_data_tx,
            peer_data_rx: Some(peer_data_rx),
            peer_clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Take the receiver for inbound peer data messages.
    ///
    /// Returns `Some` on the first call, `None` thereafter. The PeerManager
    /// consumes this to process data arriving from peer daemons.
    pub fn take_peer_data_rx(&mut self) -> Option<mpsc::Receiver<PeerDataMessage>> {
        self.peer_data_rx.take()
    }

    /// Get a handle to the peer clients map.
    ///
    /// The PeerManager uses this to send `Message::PeerData` back to specific
    /// connected peer daemons.
    pub fn peer_clients(&self) -> Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> {
        Arc::clone(&self.peer_clients)
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

        info!(path = %self.socket_path.display(), "daemon listening");

        let daemon = self.daemon;
        let client_count = self.client_count;
        let shutdown_tx = self.shutdown_tx;
        let mut shutdown_rx = self.shutdown_rx;
        let idle_timeout = self.idle_timeout;
        let socket_path = self.socket_path.clone();
        let client_notify = self.client_notify;
        let peer_data_tx = self.peer_data_tx;
        let peer_clients = self.peer_clients;

        // Spawn idle timeout watcher
        let idle_client_count = Arc::clone(&client_count);
        let idle_shutdown_tx = shutdown_tx.clone();
        let idle_notify = Arc::clone(&client_notify);
        tokio::spawn(async move {
            loop {
                // Wait until zero clients
                loop {
                    if idle_client_count.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    idle_notify.notified().await;
                }

                info!(
                    timeout_secs = idle_timeout.as_secs(),
                    "no clients connected, waiting before shutdown"
                );

                // Race: timeout vs client count change
                tokio::select! {
                    () = tokio::time::sleep(idle_timeout) => {
                        if idle_client_count.load(Ordering::SeqCst) == 0 {
                            info!("idle timeout reached, shutting down");
                            let _ = idle_shutdown_tx.send(true);
                            return;
                        }
                        // Client connected during the sleep — loop back
                    }
                    () = idle_notify.notified() => {
                        // Client count changed — loop back to re-check
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
                            info!(%count, "client connected");
                            client_notify.notify_one();

                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();
                            let peer_data_tx = peer_data_tx.clone();
                            let peer_clients = Arc::clone(&peer_clients);

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_clients,
                                )
                                .await;
                                let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
                                info!(%count, "client disconnected");
                                client_notify.notify_one();
                            });
                        }
                        Err(e) => {
                            error!(err = %e, "failed to accept connection");
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
            warn!(err = %e, "failed to remove socket file on shutdown");
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
    peer_data_tx: mpsc::Sender<PeerDataMessage>,
    peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>>,
) {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write_half)));

    // Channel for outbound messages to this specific client (used for peer relay).
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);

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
                    warn!(skipped = n, "event subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Spawn outbound relay task — writes messages from outbound_rx to the socket.
    let relay_writer = Arc::clone(&writer);
    let relay_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if write_message(&relay_writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Track whether this client has registered as a peer, and under what name.
    let mut peer_host_name: Option<HostName> = None;

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
                                warn!(err = %e, "failed to parse message");
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
                            Message::PeerData(peer_msg) => {
                                let origin = peer_msg.origin_host.clone();

                                // Register this client as a peer on first PeerData message.
                                if peer_host_name.is_none() {
                                    debug!(host = %origin, "registering peer client");
                                    peer_host_name = Some(origin.clone());
                                    peer_clients
                                        .lock()
                                        .await
                                        .insert(origin, outbound_tx.clone());
                                }

                                if let Err(e) = peer_data_tx.send(*peer_msg).await {
                                    warn!(err = %e, "failed to forward peer data");
                                }
                            }
                            other => {
                                warn!(msg = ?other, "unexpected message type from client");
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — client disconnected
                        break;
                    }
                    Err(e) => {
                        error!(err = %e, "error reading from client");
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

    // Unregister peer client on disconnect.
    if let Some(host) = peer_host_name {
        debug!(%host, "unregistering peer client");
        peer_clients.lock().await.remove(&host);
    }

    // Abort the event forwarder and relay tasks
    event_task.abort();
    relay_task.abort();
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
                Ok(command_id) => Message::ok_response(id, &command_id),
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

        "replay_since" => {
            let last_seen: std::collections::HashMap<std::path::PathBuf, u64> = params
                .get("last_seen")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_else(|| {
                    warn!("replay_since: failed to parse last_seen, returning full snapshots");
                    std::collections::HashMap::new()
                });
            match daemon.replay_since(&last_seen).await {
                Ok(events) => Message::ok_response(id, &events),
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

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::{DaemonEvent, PeerDataKind, RepoIdentity, RepoInfo};

    fn assert_ok_empty_response(msg: Message, expected_id: u64) {
        match msg {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, expected_id);
                assert!(ok);
                assert!(data.is_none());
                assert!(error.is_none());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    async fn empty_daemon() -> (tempfile::TempDir, Arc<InProcessDaemon>) {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let daemon = InProcessDaemon::new(vec![], config).await;
        (tmp, daemon)
    }

    #[tokio::test]
    async fn write_message_writes_json_line() {
        let (a, b) = tokio::net::UnixStream::pair().expect("pair");
        let (_read_half, write_half) = a.into_split();
        let writer = tokio::sync::Mutex::new(BufWriter::new(write_half));

        let msg = Message::empty_ok_response(9);
        write_message(&writer, &msg).await.expect("write_message");

        let mut lines = BufReader::new(b).lines();
        let line = lines.next_line().await.expect("read line").expect("line");
        let parsed: Message = serde_json::from_str(&line).expect("parse line as message");
        match parsed {
            Message::Response { id, ok, .. } => {
                assert_eq!(id, 9);
                assert!(ok);
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn extract_path_param_requires_string_field() {
        let params = serde_json::json!({});
        let err = extract_path_param(&params, "repo").expect_err("missing field should error");
        assert!(err.contains("missing 'repo' parameter"));

        let params = serde_json::json!({ "repo": 42 });
        let err = extract_path_param(&params, "repo").expect_err("non-string field should error");
        assert!(err.contains("missing 'repo' parameter"));

        let params = serde_json::json!({ "repo": "/tmp/project" });
        let path = extract_path_param(&params, "repo").expect("valid path string");
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[tokio::test]
    async fn dispatch_request_handles_unknown_and_missing_params() {
        let (_tmp, daemon) = empty_daemon().await;

        let unknown = dispatch_request(&daemon, 1, "not_a_method", serde_json::json!({})).await;
        match unknown {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 1);
                assert!(!ok);
                assert!(data.is_none());
                assert!(
                    error.unwrap_or_default().contains("unknown method"),
                    "unexpected error payload"
                );
            }
            other => panic!("expected response, got {other:?}"),
        }

        let missing_repo = dispatch_request(&daemon, 2, "get_state", serde_json::json!({})).await;
        match missing_repo {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 2);
                assert!(!ok);
                assert!(data.is_none());
                assert!(
                    error
                        .unwrap_or_default()
                        .contains("missing 'repo' parameter"),
                    "unexpected error payload"
                );
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_add_list_remove_repo_round_trip() {
        let (tmp, daemon) = empty_daemon().await;
        let repo_path = tmp.path().join("repo-a");
        std::fs::create_dir_all(&repo_path).unwrap();

        let add = dispatch_request(
            &daemon,
            10,
            "add_repo",
            serde_json::json!({ "path": repo_path }),
        )
        .await;
        assert_ok_empty_response(add, 10);

        let list = dispatch_request(&daemon, 11, "list_repos", serde_json::json!({})).await;
        let listed: Vec<RepoInfo> = match list {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 11);
                assert!(ok, "list_repos should be ok: {error:?}");
                serde_json::from_value(data.expect("list data")).expect("parse repo list")
            }
            other => panic!("expected response, got {other:?}"),
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, repo_path);

        let remove = dispatch_request(
            &daemon,
            12,
            "remove_repo",
            serde_json::json!({ "path": listed[0].path }),
        )
        .await;
        assert_ok_empty_response(remove, 12);
    }

    #[tokio::test]
    async fn dispatch_replay_since_with_bad_payload_degrades_to_empty_last_seen() {
        let (_tmp, daemon) = empty_daemon().await;

        let replay = dispatch_request(
            &daemon,
            30,
            "replay_since",
            serde_json::json!({ "last_seen": "invalid-shape" }),
        )
        .await;
        match replay {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 30);
                assert!(ok, "replay_since should still succeed: {error:?}");
                let events: Vec<DaemonEvent> =
                    serde_json::from_value(data.expect("replay events data")).expect("events");
                assert!(events.is_empty());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn take_peer_data_rx_returns_some_once() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let mut server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        assert!(
            server.take_peer_data_rx().is_some(),
            "first call should return Some"
        );
        assert!(
            server.take_peer_data_rx().is_none(),
            "second call should return None"
        );
    }

    #[tokio::test]
    async fn peer_clients_accessor_returns_shared_map() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let server = DaemonServer::new(
            vec![],
            config,
            tmp.path().join("test.sock"),
            Duration::from_secs(60),
        )
        .await;

        let map = server.peer_clients();
        assert!(map.lock().await.is_empty());

        // Inserting via one handle is visible via another
        let map2 = server.peer_clients();
        let (tx, _rx) = mpsc::channel(1);
        map.lock().await.insert(HostName::new("laptop"), tx);
        assert_eq!(map2.lock().await.len(), 1);
    }

    fn test_peer_msg(host: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/tmp/repo"),
            kind: PeerDataKind::RequestResync { since_seq: 0 },
        }
    }

    #[tokio::test]
    async fn handle_client_forwards_peer_data_and_registers_peer() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
        let peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pc = Arc::clone(&peer_clients);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon, shutdown_rx, peer_data_tx, pc).await;
        });

        // Send a PeerData message from the client side
        let peer_msg = test_peer_msg("remote-host");
        let wire_msg = Message::PeerData(Box::new(peer_msg.clone()));
        let json = serde_json::to_string(&wire_msg).expect("serialize");

        let (read_half, write_half) = client_stream.into_split();
        let mut writer = BufWriter::new(write_half);
        writer.write_all(json.as_bytes()).await.expect("write");
        writer.write_all(b"\n").await.expect("newline");
        writer.flush().await.expect("flush");

        // The server should forward the peer data
        let received = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
            .await
            .expect("timeout waiting for peer data")
            .expect("channel closed");
        assert_eq!(received.origin_host, HostName::new("remote-host"));

        // The peer should now be registered in peer_clients
        // Give a brief moment for the lock to be released
        tokio::time::sleep(Duration::from_millis(50)).await;
        let map = peer_clients.lock().await;
        assert!(
            map.contains_key(&HostName::new("remote-host")),
            "peer should be registered after sending PeerData"
        );
        drop(map);

        // Drop the writer to close the connection, triggering cleanup
        drop(writer);
        drop(read_half);

        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // After disconnect, the peer should be unregistered
        let map = peer_clients.lock().await;
        assert!(
            !map.contains_key(&HostName::new("remote-host")),
            "peer should be unregistered after disconnect"
        );
    }

    #[tokio::test]
    async fn handle_client_relays_outbound_peer_messages() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_clients: Arc<Mutex<HashMap<HostName, mpsc::Sender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pc = Arc::clone(&peer_clients);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon, shutdown_rx, peer_data_tx, pc).await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut writer = BufWriter::new(write_half);

        // Send a PeerData message to register as a peer
        let peer_msg = test_peer_msg("relay-target");
        let wire_msg = Message::PeerData(Box::new(peer_msg));
        let json = serde_json::to_string(&wire_msg).expect("serialize");
        writer.write_all(json.as_bytes()).await.expect("write");
        writer.write_all(b"\n").await.expect("newline");
        writer.flush().await.expect("flush");

        // Wait for registration
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now push a message via peer_clients to relay back to this client
        let relay_msg = Message::PeerData(Box::new(test_peer_msg("other-host")));
        {
            let map = peer_clients.lock().await;
            let sender = map
                .get(&HostName::new("relay-target"))
                .expect("peer should be registered");
            sender.send(relay_msg).await.expect("send relay");
        }

        // Read from the client side — should receive the relayed message
        let reader = BufReader::new(read_half);
        let mut lines = reader.lines();

        // We may receive event messages (snapshots) before our peer data relay,
        // so loop until we find the PeerData message.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut found_relay = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let msg: Message = serde_json::from_str(&line).expect("parse");
                    if let Message::PeerData(peer_msg) = msg {
                        assert_eq!(peer_msg.origin_host, HostName::new("other-host"));
                        found_relay = true;
                        break;
                    }
                    // Skip non-PeerData messages (events, etc.)
                }
                _ => break,
            }
        }
        assert!(found_relay, "should have received relayed PeerData message");

        // Clean up
        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
