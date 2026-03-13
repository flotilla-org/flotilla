use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{Command, DaemonEvent, GoodbyeReason, HostName, Message, PeerConnectionState, PeerWireMessage, PROTOCOL_VERSION};
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
};
use tracing::{error, info, warn};

use crate::{
    peer::{ActivationResult, ConnectionDirection, ConnectionMeta, InboundPeerEnvelope, PeerManager, PeerSender},
    peer_networking::{disconnect_peer_and_rebuild, PeerConnectedNotice},
};

struct SocketPeerSender {
    tx: tokio::sync::Mutex<Option<mpsc::Sender<Message>>>,
}

#[async_trait]
impl PeerSender for SocketPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        let tx = self.tx.lock().await.as_ref().cloned().ok_or_else(|| "socket peer outbound channel closed".to_string())?;
        tx.send(Message::Peer(Box::new(msg))).await.map_err(|_| "socket peer outbound channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        let tx = self.tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(Message::Peer(Box::new(PeerWireMessage::Goodbye { reason })))
                .await
                .map_err(|_| "socket peer outbound channel closed".to_string())?;
        }
        Ok(())
    }
}

/// The daemon server that listens on a Unix socket and dispatches requests
/// to an `InProcessDaemon`.
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    follower: bool,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Channel for inbound peer wire messages tagged with connection authority.
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    /// Manages connections to remote peer hosts and stores their provider data.
    peer_manager: Arc<Mutex<PeerManager>>,
    /// Peer networking task, consumed by `run()`.
    peer_networking: Option<crate::peer_networking::PeerNetworkingTask>,
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
    ) -> Result<Self, String> {
        let daemon_config = config.load_daemon_config();
        let host_name = daemon_config.host_name.map(HostName::new).unwrap_or_else(HostName::local);

        let daemon = InProcessDaemon::new_with_options(repo_paths, config.clone(), daemon_config.follower, host_name).await;

        let (peer_networking, peer_manager, peer_data_tx) = crate::peer_networking::PeerNetworkingTask::new(Arc::clone(&daemon), &config)?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(Self {
            daemon,
            socket_path,
            idle_timeout,
            follower: daemon_config.follower,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_tx,
            shutdown_rx,
            peer_data_tx,
            peer_manager,
            peer_networking: Some(peer_networking),
        })
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
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| format!("failed to bind socket: {e}"))?;

        info!(path = %self.socket_path.display(), "daemon listening");

        let daemon = self.daemon;
        let client_count = self.client_count;
        let shutdown_tx = self.shutdown_tx;
        let mut shutdown_rx = self.shutdown_rx;
        let idle_timeout = self.idle_timeout;
        let socket_path = self.socket_path.clone();
        let client_notify = self.client_notify;
        let peer_data_tx = self.peer_data_tx;
        let peer_manager = self.peer_manager;

        // Spawn peer networking task — returns a peer_connected_tx that
        // handle_client uses to notify the outbound broadcaster when
        // socket peers connect.
        let peer_networking = self.peer_networking.take().expect("run() called twice");
        let (_peer_handle, peer_connected_tx) = peer_networking.spawn();

        // Spawn idle timeout watcher (disabled for follower-mode daemons
        // which serve peer connections and should stay up indefinitely)
        if !self.follower {
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

                    info!(timeout_secs = idle_timeout.as_secs(), "no clients connected, waiting before shutdown");

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
        } else {
            info!("follower mode: idle timeout disabled");
        }

        // SIGTERM handler
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to register SIGTERM handler");

        // Accept loop
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();
                            let peer_data_tx = peer_data_tx.clone();
                            let peer_manager = Arc::clone(&peer_manager);
                            let peer_connected_tx = peer_connected_tx.clone();

                            tokio::spawn(async move {
                                handle_client(
                                    stream,
                                    daemon,
                                    shutdown_rx,
                                    peer_data_tx,
                                    peer_manager,
                                    client_count,
                                    client_notify,
                                    peer_connected_tx,
                                )
                                .await;
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
async fn write_message(writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>, msg: &Message) -> Result<(), ()> {
    let mut w = writer.lock().await;
    flotilla_protocol::framing::write_message_line(&mut *w, msg).await.map_err(|_| ())
}

/// Handle a single client connection.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
    mut shutdown_rx: watch::Receiver<bool>,
    peer_data_tx: mpsc::Sender<InboundPeerEnvelope>,
    peer_manager: Arc<Mutex<PeerManager>>,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    peer_connected_tx: mpsc::UnboundedSender<PeerConnectedNotice>,
) {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(write_half)));
    let mut lines = reader.lines();
    let first_msg = tokio::select! {
        line_result = lines.next_line() => {
            match line_result {
                Ok(Some(line)) => match serde_json::from_str::<Message>(&line) {
                    Ok(msg) => Some(msg),
                    Err(e) => {
                        warn!(err = %e, "failed to parse first message");
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    error!(err = %e, "error reading first message from client");
                    None
                }
            }
        }
        _ = shutdown_rx.changed() => None,
    };

    let Some(first_msg) = first_msg else {
        return;
    };

    match first_msg {
        Message::Request { id, method, params } => {
            let count = client_count.fetch_add(1, Ordering::SeqCst) + 1;
            info!(%count, "client connected");
            client_notify.notify_one();

            let event_writer = Arc::clone(&writer);
            let mut event_rx = daemon.subscribe();
            let event_task = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            let msg = Message::Event { event: Box::new(event) };
                            if write_message(&event_writer, &msg).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "event subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let first_response = dispatch_request(&daemon, id, &method, params).await;
            if write_message(&writer, &first_response).await.is_ok() {
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
                                        other => {
                                            warn!(msg = ?other, "unexpected message type from client");
                                            break;
                                        }
                                    }
                                }
                                Ok(None) => break,
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
            }

            event_task.abort();
            let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
            info!(%count, "client disconnected");
            client_notify.notify_one();
        }
        Message::Hello { protocol_version, host_name } => {
            if protocol_version != PROTOCOL_VERSION {
                warn!(
                    peer = %host_name,
                    expected = PROTOCOL_VERSION,
                    got = protocol_version,
                    "peer protocol version mismatch"
                );
                return;
            }

            if write_message(&writer, &Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: daemon.host_name().clone() })
                .await
                .is_err()
            {
                return;
            }

            let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);
            let relay_writer = Arc::clone(&writer);
            let relay_task = tokio::spawn(async move {
                while let Some(msg) = outbound_rx.recv().await {
                    if write_message(&relay_writer, &msg).await.is_err() {
                        break;
                    }
                }
            });

            let (generation, displaced_generation) = {
                let mut pm = peer_manager.lock().await;
                match pm.activate_connection(
                    host_name.clone(),
                    Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(Some(outbound_tx.clone())) }),
                    ConnectionMeta {
                        direction: ConnectionDirection::Inbound,
                        config_label: None,
                        expected_peer: None,
                        config_backed: false,
                    },
                ) {
                    ActivationResult::Accepted { generation, displaced } => (generation, displaced),
                    ActivationResult::Rejected { reason } => {
                        let _ = write_message(&writer, &Message::Peer(Box::new(PeerWireMessage::Goodbye { reason }))).await;
                        relay_task.abort();
                        return;
                    }
                }
            };
            if let Some(displaced_generation) = displaced_generation {
                let displaced = {
                    let mut pm = peer_manager.lock().await;
                    pm.take_displaced_sender(&host_name, displaced_generation)
                };
                if let Some(displaced) = displaced {
                    let _ = displaced.retire(GoodbyeReason::Superseded).await;
                }
            }
            daemon.send_event(DaemonEvent::PeerStatusChanged { host: host_name.clone(), status: PeerConnectionState::Connected });
            let _ = peer_connected_tx.send(PeerConnectedNotice { peer: host_name.clone(), generation });

            loop {
                tokio::select! {
                    line_result = lines.next_line() => {
                        match line_result {
                            Ok(Some(line)) => {
                                let msg: Message = match serde_json::from_str(&line) {
                                    Ok(m) => m,
                                    Err(e) => {
                                        warn!(peer = %host_name, err = %e, "failed to parse peer message");
                                        break;
                                    }
                                };
                                match msg {
                                    Message::Peer(peer_msg) => {
                                        if let Err(e) = peer_data_tx.send(InboundPeerEnvelope {
                                            msg: *peer_msg,
                                            connection_generation: generation,
                                            connection_peer: host_name.clone(),
                                        }).await {
                                            warn!(peer = %host_name, err = %e, "failed to forward inbound peer message");
                                            break;
                                        }
                                    }
                                    other => {
                                        warn!(peer = %host_name, msg = ?other, "unexpected message type from peer");
                                        break;
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                error!(peer = %host_name, err = %e, "error reading from peer");
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

            let plan = disconnect_peer_and_rebuild(&peer_manager, &daemon, &host_name, generation).await;
            if plan.was_active {
                daemon.send_event(DaemonEvent::PeerStatusChanged { host: host_name, status: PeerConnectionState::Disconnected });
            }
            relay_task.abort();
        }
        other => {
            warn!(msg = ?other, "unexpected first message type from client");
        }
    }
}

/// Dispatch a request to the appropriate `DaemonHandle` method.
async fn dispatch_request(daemon: &Arc<InProcessDaemon>, id: u64, method: &str, params: serde_json::Value) -> Message {
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
                .and_then(|v| serde_json::from_value(v).map_err(|e| format!("invalid command: {e}")))
            {
                Ok(cmd) => cmd,
                Err(e) => return Message::error_response(id, e),
            };
            match daemon.execute(&repo, command).await {
                Ok(command_id) => Message::ok_response(id, &command_id),
                Err(e) => Message::error_response(id, e),
            }
        }

        "cancel" => {
            let command_id: u64 = match params.get("command_id").and_then(|v| v.as_u64()) {
                Some(id) => id,
                None => return Message::error_response(id, "missing or invalid 'command_id'".to_string()),
            };
            match daemon.cancel(command_id).await {
                Ok(()) => Message::empty_ok_response(id),
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
            let last_seen: std::collections::HashMap<std::path::PathBuf, u64> =
                params.get("last_seen").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_else(|| {
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
    params.get(field).and_then(|v| v.as_str()).map(PathBuf::from).ok_or_else(|| format!("missing '{field}' parameter"))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use flotilla_protocol::{
        Checkout, DaemonEvent, HostName, HostPath, PeerDataKind, PeerDataMessage, PeerWireMessage, ProviderData, RepoIdentity, RepoInfo,
        VectorClock,
    };
    use indexmap::IndexMap;

    use super::*;
    use crate::peer::test_support::{ensure_test_connection_generation, handle_test_peer_data};

    fn assert_ok_empty_response(msg: Message, expected_id: u64) {
        match msg {
            Message::Response { id, ok, data, error } => {
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

    fn checkout(branch: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn peer_snapshot(host: &str, repo_identity: &RepoIdentity, repo_path: &Path, checkout_path: &str, branch: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: repo_identity.clone(),
            repo_path: repo_path.to_path_buf(),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new(host), checkout_path), checkout(branch))]),
                    ..Default::default()
                }),
                seq: 1,
            },
        }
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
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 1);
                assert!(!ok);
                assert!(data.is_none());
                assert!(error.unwrap_or_default().contains("unknown method"), "unexpected error payload");
            }
            other => panic!("expected response, got {other:?}"),
        }

        let missing_repo = dispatch_request(&daemon, 2, "get_state", serde_json::json!({})).await;
        match missing_repo {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 2);
                assert!(!ok);
                assert!(data.is_none());
                assert!(error.unwrap_or_default().contains("missing 'repo' parameter"), "unexpected error payload");
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_add_list_remove_repo_round_trip() {
        let (tmp, daemon) = empty_daemon().await;
        let repo_path = tmp.path().join("repo-a");
        std::fs::create_dir_all(&repo_path).unwrap();

        let add = dispatch_request(&daemon, 10, "add_repo", serde_json::json!({ "path": repo_path })).await;
        assert_ok_empty_response(add, 10);

        let list = dispatch_request(&daemon, 11, "list_repos", serde_json::json!({})).await;
        let listed: Vec<RepoInfo> = match list {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 11);
                assert!(ok, "list_repos should be ok: {error:?}");
                serde_json::from_value(data.expect("list data")).expect("parse repo list")
            }
            other => panic!("expected response, got {other:?}"),
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, repo_path);

        let remove = dispatch_request(&daemon, 12, "remove_repo", serde_json::json!({ "path": listed[0].path })).await;
        assert_ok_empty_response(remove, 12);
    }

    #[tokio::test]
    async fn dispatch_replay_since_with_bad_payload_degrades_to_empty_last_seen() {
        let (_tmp, daemon) = empty_daemon().await;

        let replay = dispatch_request(&daemon, 30, "replay_since", serde_json::json!({ "last_seen": "invalid-shape" })).await;
        match replay {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 30);
                assert!(ok, "replay_since should still succeed: {error:?}");
                let events: Vec<DaemonEvent> = serde_json::from_value(data.expect("replay events data")).expect("events");
                assert!(events.is_empty());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn daemon_server_replays_configured_hosts_as_disconnected() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.udder]\nhostname = \"udder\"\ndaemon_socket = \"/tmp/udder.sock\"\n\n[hosts.feta]\nhostname = \"feta\"\ndaemon_socket = \"/tmp/feta.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        let events = server.daemon.replay_since(&HashMap::new()).await.unwrap();
        let mut statuses: Vec<(HostName, PeerConnectionState)> = events
            .into_iter()
            .filter_map(|event| match event {
                DaemonEvent::PeerStatusChanged { host, status } => Some((host, status)),
                _ => None,
            })
            .collect();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(statuses, vec![
            (HostName::new("feta"), PeerConnectionState::Disconnected),
            (HostName::new("udder"), PeerConnectionState::Disconnected),
        ]);
    }

    fn test_peer_msg(host: &str) -> PeerDataMessage {
        PeerDataMessage {
            origin_host: HostName::new(host),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/tmp/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 0 },
        }
    }

    #[tokio::test]
    async fn handle_client_forwards_peer_data_and_registers_peer() {
        let (_tmp, daemon) = empty_daemon().await;
        let expected_local_host = daemon.host_name().clone();
        let daemon_events = daemon.subscribe();
        let (peer_data_tx, mut peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pm = Arc::clone(&peer_manager);
        let count_ref = Arc::clone(&client_count);
        let notify_ref = Arc::clone(&client_notify);
        let daemon_for_task = Arc::clone(&daemon);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon_for_task, shutdown_rx, peer_data_tx, pm, count_ref, notify_ref, peer_connected_tx).await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut reader = BufReader::new(read_half).lines();
        let mut writer = BufWriter::new(write_half);

        let hello = Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("remote-host") };
        flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

        let line = reader.next_line().await.expect("read hello response").expect("hello line");
        let hello_back: Message = serde_json::from_str(&line).expect("parse hello");
        match hello_back {
            Message::Hello { protocol_version, host_name } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(host_name, expected_local_host);
            }
            other => panic!("expected hello response, got {other:?}"),
        }

        let mut daemon_events = daemon_events;
        let connected_event = tokio::time::timeout(Duration::from_secs(2), daemon_events.recv())
            .await
            .expect("timeout waiting for peer status")
            .expect("peer status event");
        match connected_event {
            DaemonEvent::PeerStatusChanged { host, status } => {
                assert_eq!(host, HostName::new("remote-host"));
                assert_eq!(status, PeerConnectionState::Connected);
            }
            other => panic!("expected peer status event, got {other:?}"),
        }

        // Send a peer message from the client side
        let peer_msg = test_peer_msg("remote-host");
        let wire_msg = Message::Peer(Box::new(PeerWireMessage::Data(peer_msg.clone())));
        flotilla_protocol::framing::write_message_line(&mut writer, &wire_msg).await.expect("write peer message");

        // The server should forward the peer envelope
        let received = tokio::time::timeout(Duration::from_secs(2), peer_data_rx.recv())
            .await
            .expect("timeout waiting for peer data")
            .expect("channel closed");
        assert_eq!(received.connection_peer, HostName::new("remote-host"));
        assert_eq!(received.connection_generation, 1);
        match received.msg {
            PeerWireMessage::Data(msg) => {
                assert_eq!(msg.origin_host, HostName::new("remote-host"));
            }
            other => panic!("expected data message, got {other:?}"),
        }

        {
            let pm = peer_manager.lock().await;
            assert_eq!(pm.current_generation(&HostName::new("remote-host")), Some(1));
        }
        assert_eq!(client_count.load(Ordering::SeqCst), 0);

        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let disconnected_event = tokio::time::timeout(Duration::from_secs(2), daemon_events.recv())
            .await
            .expect("timeout waiting for peer disconnect")
            .expect("peer disconnect event");
        match disconnected_event {
            DaemonEvent::PeerStatusChanged { host, status } => {
                assert_eq!(host, HostName::new("remote-host"));
                assert_eq!(status, PeerConnectionState::Disconnected);
            }
            other => panic!("expected peer disconnect event, got {other:?}"),
        }

        let pm = peer_manager.lock().await;
        assert!(pm.current_generation(&HostName::new("remote-host")).is_none(), "peer should be disconnected after socket close");
    }

    #[tokio::test]
    async fn peer_manager_initialized_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();

        // Write daemon config with a custom host name
        std::fs::write(base.join("daemon.toml"), "host_name = \"test-host\"\n").unwrap();

        // Write hosts config with one peer
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = \"remote\"\ndaemon_socket = \"/tmp/daemon.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        // PeerManager should be initialized and accessible
        let pm = server.peer_manager.lock().await;
        // peer_data is empty since no data has been received yet
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn peer_manager_default_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
        let server = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await.unwrap();

        // Should still have a PeerManager with no peers
        let pm = server.peer_manager.lock().await;
        assert!(pm.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn daemon_server_new_returns_error_for_invalid_hosts_config() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.remote]\nhostname = \"10.0.0.5\"\nexpected_host_name = [\ndaemon_socket = \"/tmp/daemon.sock\"\n",
        )
        .unwrap();

        let config = Arc::new(ConfigStore::with_base(&base));
        let result = DaemonServer::new(vec![], config, tmp.path().join("test.sock"), Duration::from_secs(60)).await;

        match result {
            Ok(_) => panic!("invalid hosts config should return startup error"),
            Err(err) => assert!(err.contains("failed to parse")),
        }
    }

    #[tokio::test]
    async fn handle_client_relays_outbound_peer_messages() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().expect("pair");

        // Spawn handle_client on the server side
        let pm = Arc::clone(&peer_manager);
        let count_ref = Arc::clone(&client_count);
        let notify_ref = Arc::clone(&client_notify);
        let handle = tokio::spawn(async move {
            handle_client(server_stream, daemon, shutdown_rx, peer_data_tx, pm, count_ref, notify_ref, peer_connected_tx).await;
        });

        let (read_half, write_half) = client_stream.into_split();
        let mut reader = BufReader::new(read_half).lines();
        let mut writer = BufWriter::new(write_half);

        let hello = Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("relay-target") };
        flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");
        let _ = reader.next_line().await.expect("read hello").expect("line");

        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let pm = peer_manager.lock().await;
            pm.send_to(&HostName::new("relay-target"), PeerWireMessage::Data(test_peer_msg("other-host"))).await.expect("send relay");
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut found_relay = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(1), reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let msg: Message = serde_json::from_str(&line).expect("parse");
                    if let Message::Peer(peer_msg) = msg {
                        if let PeerWireMessage::Data(peer_msg) = *peer_msg {
                            assert_eq!(peer_msg.origin_host, HostName::new("other-host"));
                            found_relay = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_relay, "should have received relayed peer message");

        // Clean up
        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn duplicate_inbound_peer_receives_goodbye_on_rejection() {
        let (_tmp, daemon) = empty_daemon().await;
        let (peer_data_tx, _peer_data_rx) = mpsc::channel(16);
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let client_count = Arc::new(AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();

        let (client_stream_a, server_stream_a) = tokio::net::UnixStream::pair().expect("pair a");
        let (client_stream_b, server_stream_b) = tokio::net::UnixStream::pair().expect("pair b");

        let expected_server_host = daemon.host_name().clone();
        let daemon_a = Arc::clone(&daemon);
        let daemon_b = Arc::clone(&daemon);
        let pm_a = Arc::clone(&peer_manager);
        let pm_b = Arc::clone(&peer_manager);
        let tx_a = peer_data_tx.clone();
        let tx_b = peer_data_tx.clone();
        let count_a = Arc::clone(&client_count);
        let count_b = Arc::clone(&client_count);
        let notify_a = Arc::clone(&client_notify);
        let notify_b = Arc::clone(&client_notify);
        let shutdown_rx_a = shutdown_rx.clone();
        let shutdown_rx_b = shutdown_rx.clone();
        let peer_connected_tx_a = peer_connected_tx.clone();
        let peer_connected_tx_b = peer_connected_tx.clone();

        let handle_a = tokio::spawn(async move {
            handle_client(server_stream_a, daemon_a, shutdown_rx_a, tx_a, pm_a, count_a, notify_a, peer_connected_tx_a).await;
        });
        let handle_b = tokio::spawn(async move {
            handle_client(server_stream_b, daemon_b, shutdown_rx_b, tx_b, pm_b, count_b, notify_b, peer_connected_tx_b).await;
        });

        async fn send_peer_hello(
            stream: tokio::net::UnixStream,
            expected_server_host: &HostName,
        ) -> (tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>, tokio::io::BufWriter<tokio::net::unix::OwnedWriteHalf>)
        {
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half).lines();
            let mut writer = BufWriter::new(write_half);
            let hello = Message::Hello { protocol_version: PROTOCOL_VERSION, host_name: HostName::new("peer") };
            flotilla_protocol::framing::write_message_line(&mut writer, &hello).await.expect("write hello");

            let line = reader.next_line().await.expect("read hello response").expect("hello response line");
            let msg: Message = serde_json::from_str(&line).expect("parse hello response");
            match msg {
                Message::Hello { host_name, .. } => {
                    assert_eq!(host_name, expected_server_host.clone())
                }
                other => panic!("expected hello response, got {other:?}"),
            }

            (reader, writer)
        }

        let (_reader_a, writer_a) = send_peer_hello(client_stream_a, &expected_server_host).await;
        let (mut reader_b, writer_b) = send_peer_hello(client_stream_b, &expected_server_host).await;

        let goodbye = tokio::time::timeout(Duration::from_secs(2), reader_b.next_line())
            .await
            .expect("timeout waiting for goodbye")
            .expect("read goodbye line")
            .expect("goodbye line");
        let goodbye_msg: Message = serde_json::from_str(&goodbye).expect("parse goodbye");
        match goodbye_msg {
            Message::Peer(inner) => match *inner {
                PeerWireMessage::Goodbye { reason: flotilla_protocol::GoodbyeReason::Superseded } => {}
                other => panic!("expected superseded goodbye, got {other:?}"),
            },
            other => panic!("expected peer goodbye, got {other:?}"),
        }

        drop(writer_a);
        drop(writer_b);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle_a).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), handle_b).await;
    }

    #[tokio::test]
    async fn clear_peer_data_rebuilds_remote_only_repo_without_stale_first_event() {
        let (_tmp, daemon) = empty_daemon().await;
        let peer_manager = Arc::new(Mutex::new(PeerManager::new(HostName::new("local"))));
        let repo_identity = RepoIdentity { authority: "github.com".into(), path: "owner/remote-only".into() };
        let repo_path = PathBuf::from("/srv/remote-only");

        {
            let mut pm = peer_manager.lock().await;
            assert_eq!(
                handle_test_peer_data(
                    &mut pm,
                    peer_snapshot("peer-a", &repo_identity, &repo_path, "/srv/peer-a/remote-only", "feature-a",),
                    || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
                )
                .await,
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
            assert_eq!(
                handle_test_peer_data(
                    &mut pm,
                    peer_snapshot("peer-b", &repo_identity, &repo_path, "/srv/peer-b/remote-only", "feature-b",),
                    || { Arc::new(SocketPeerSender { tx: tokio::sync::Mutex::new(None) }) as Arc<dyn PeerSender> },
                )
                .await,
                crate::peer::HandleResult::Updated(repo_identity.clone())
            );
        }

        let synthetic = crate::peer::synthetic_repo_path(&HostName::new("peer-a"), &repo_path);
        let merged = {
            let pm = peer_manager.lock().await;
            let peers: Vec<(HostName, ProviderData)> = pm
                .get_peer_data()
                .iter()
                .filter_map(|(host, repos)| repos.get(&repo_identity).map(|state| (host.clone(), state.provider_data.clone())))
                .collect();
            crate::peer::merge_provider_data(
                &ProviderData::default(),
                daemon.host_name(),
                &peers.iter().map(|(h, d)| (h.clone(), d)).collect::<Vec<_>>(),
            )
        };
        daemon.add_virtual_repo(synthetic.clone(), merged).await.expect("add virtual repo");
        daemon
            .set_peer_providers(&synthetic, vec![
                (HostName::new("peer-a"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only"), checkout("feature-a"))]),
                    ..Default::default()
                }),
                (HostName::new("peer-b"), ProviderData {
                    checkouts: IndexMap::from([(HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only"), checkout("feature-b"))]),
                    ..Default::default()
                }),
            ])
            .await;
        {
            let mut pm = peer_manager.lock().await;
            pm.register_remote_repo(repo_identity.clone(), synthetic.clone());
        }

        let mut rx = daemon.subscribe();
        let gen_a = {
            let mut pm = peer_manager.lock().await;
            ensure_test_connection_generation(&mut pm, &HostName::new("peer-a"), || {
                Arc::new(super::SocketPeerSender { tx: tokio::sync::Mutex::new(Some(mpsc::channel(1).0)) }) as Arc<dyn PeerSender>
            })
        };

        disconnect_peer_and_rebuild(&peer_manager, &daemon, &HostName::new("peer-a"), gen_a).await;

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for first event")
            .expect("broadcast channel should stay open");

        let stale_key = HostPath::new(HostName::new("peer-a"), "/srv/peer-a/remote-only");
        let remaining_key = HostPath::new(HostName::new("peer-b"), "/srv/peer-b/remote-only");
        match event {
            DaemonEvent::SnapshotFull(snapshot) => {
                assert_eq!(snapshot.repo, synthetic);
                assert!(
                    !snapshot.providers.checkouts.contains_key(&stale_key),
                    "first snapshot after disconnect should not include stale peer-a checkout"
                );
                assert_eq!(snapshot.providers.checkouts[&remaining_key].branch, "feature-b");
            }
            DaemonEvent::SnapshotDelta(delta) => {
                assert_eq!(delta.repo, synthetic);
                assert!(
                    delta.changes.iter().any(|change| matches!(
                        change,
                        flotilla_protocol::Change::Checkout {
                            key,
                            op: flotilla_protocol::EntryOp::Removed
                        } if key == &stale_key
                    )),
                    "first delta after disconnect should remove stale peer-a checkout"
                );
            }
            other => panic!("expected snapshot event, got {other:?}"),
        }
    }
}
