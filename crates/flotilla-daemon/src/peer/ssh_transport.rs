use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::config::RemoteHostConfig;
use flotilla_protocol::{ConfigLabel, GoodbyeReason, Message, NodeId, PeerWireMessage, PROTOCOL_VERSION};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

use super::transport::{PeerConnectionStatus, PeerSender, PeerTransport};

/// Maximum backoff delay between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Initial backoff delay between reconnection attempts.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// How long to wait for the forwarded socket to appear after spawning SSH.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between polls when waiting for the socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Channel buffer size for inbound and outbound peer data messages.
const CHANNEL_BUFFER: usize = 256;

struct ChannelPeerSender {
    tx: tokio::sync::Mutex<Option<mpsc::Sender<PeerWireMessage>>>,
}

#[async_trait]
impl PeerSender for ChannelPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        let tx = self.tx.lock().await.as_ref().cloned().ok_or_else(|| "outbound channel closed".to_string())?;
        tx.send(msg).await.map_err(|_| "outbound channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        let tx = self.tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(PeerWireMessage::Goodbye { reason }).await.map_err(|_| "outbound channel closed".to_string())?;
        }
        Ok(())
    }
}

/// SSH-based transport that forwards a remote daemon's Unix socket locally
/// and exchanges peer wire messages over it.
///
/// The transport spawns `ssh -N -L <local>:<remote> [user@]host` as a child
/// process, then connects to the locally-forwarded socket to read/write
/// JSON-line `Message` values. Only `Message::Peer` payloads are forwarded;
/// other message types on the wire are silently discarded.
pub struct SshTransport {
    local_node_id: NodeId,
    config: RemoteHostConfig,
    config_label: ConfigLabel,
    expected_node_id: NodeId,
    local_socket_path: PathBuf,
    ssh_process: Option<tokio::process::Child>,
    status: PeerConnectionStatus,
    /// Receiver for inbound peer data, produced by `connect_socket()` and
    /// returned once via `subscribe()`.
    inbound_rx: Option<mpsc::Receiver<PeerWireMessage>>,
    outbound_tx: Option<mpsc::Sender<PeerWireMessage>>,
    /// Holds JoinHandles for the reader and writer background tasks so we can
    /// abort them on disconnect.
    task_handles: Vec<tokio::task::JoinHandle<()>>,
    /// Local daemon's session ID, included in outbound hello messages.
    local_session_id: uuid::Uuid,
    /// Session ID received from the remote peer during handshake.
    remote_session_id: Option<uuid::Uuid>,
}

impl SshTransport {
    /// Create a new SSH transport for the given remote host.
    ///
    /// The local forwarded socket will be placed at
    /// `~/.config/flotilla/peers/<host-name>.sock`.
    pub fn new(
        local_node_id: NodeId,
        config_label: ConfigLabel,
        config: RemoteHostConfig,
        local_session_id: uuid::Uuid,
        state_dir: &Path,
    ) -> Result<Self, String> {
        // Sanitise: reject host names containing path separators to prevent
        // path traversal (e.g. `../` in hosts.toml).
        let name_str = config_label.0.as_str();
        if name_str.contains('/') || name_str.contains('\\') || name_str.contains('\0') {
            return Err(format!("peer host name must not contain path separators: {name_str:?}"));
        }
        let local_socket_path = state_dir.join("peers").join(format!("{}.sock", config_label.0));
        let expected_node_id = NodeId::new(&config.expected_host_name);

        Ok(Self {
            local_node_id,
            config,
            config_label,
            expected_node_id,
            local_socket_path,
            ssh_process: None,
            status: PeerConnectionStatus::Disconnected,
            inbound_rx: None,
            outbound_tx: None,
            task_handles: Vec::new(),
            local_session_id,
            remote_session_id: None,
        })
    }

    /// Spawn the SSH child process that forwards the remote socket locally.
    async fn spawn_ssh(&mut self) -> Result<(), String> {
        // Clean up any stale local socket before spawning
        self.cleanup_socket();

        // Ensure peers directory exists
        if let Some(parent) = self.local_socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create peers directory: {e}"))?;
        }

        let forward_spec = format!("{}:{}", self.local_socket_path.display(), self.config.daemon_socket);

        let destination = match &self.config.user {
            Some(user) => format!("{user}@{}", self.config.hostname),
            None => self.config.hostname.clone(),
        };

        info!(
            peer = %self.expected_node_id,
            label = %self.config_label.0,
            %destination,
            forward = %forward_spec,
            "spawning SSH tunnel"
        );

        let child = tokio::process::Command::new("ssh")
            .arg("-N") // no remote command
            .arg("-L")
            .arg(&forward_spec)
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg(&destination)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn ssh: {e}"))?;

        self.ssh_process = Some(child);
        Ok(())
    }

    /// Wait for the forwarded local socket file to appear on disk.
    ///
    /// Also checks whether the SSH child has exited early (bad key,
    /// unreachable host, etc.) to fail fast instead of waiting the
    /// full timeout.
    async fn wait_for_socket(&mut self) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + SOCKET_POLL_TIMEOUT;

        loop {
            if self.local_socket_path.exists() {
                debug!(
                    path = %self.local_socket_path.display(),
                    "forwarded socket appeared"
                );
                return Ok(());
            }

            // Detect early SSH exit (auth failure, unreachable host, etc.)
            if let Some(ref mut child) = self.ssh_process {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("ssh exited prematurely with {status}"));
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(format!("timed out waiting for forwarded socket at {}", self.local_socket_path.display()));
            }

            tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
        }
    }

    /// Connect to the local forwarded socket, complete the hello handshake,
    /// then spawn reader/writer tasks.
    async fn connect_socket(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        let mut stream = UnixStream::connect(&self.local_socket_path)
            .await
            .map_err(|e| format!("failed to connect to forwarded socket {}: {e}", self.local_socket_path.display()))?;

        flotilla_protocol::framing::write_message_line(&mut stream, &Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: self.local_node_id.clone(),
            display_name: self.local_node_id.to_string(),
            session_id: self.local_session_id,
            connection_role: None,
        })
        .await?;

        let (read_half, write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let line = lines
            .next_line()
            .await
            .map_err(|e| format!("failed to read peer hello: {e}"))?
            .ok_or_else(|| "peer closed before sending hello".to_string())?;
        let hello = serde_json::from_str(&line).map_err(|e| format!("failed to parse peer hello: {e}"))?;
        let remote_session_id = Self::validate_remote_hello(&self.expected_node_id, hello)?;
        self.remote_session_id = Some(remote_session_id);

        // Inbound: reader task → inbound channel → subscriber
        let (inbound_tx, inbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);

        // Outbound: send() → outbound channel → writer task
        let (outbound_tx, outbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);
        self.outbound_tx = Some(outbound_tx);

        // Spawn reader task
        let node_id = self.expected_node_id.clone();
        let reader_handle = tokio::spawn(async move {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let msg: Message = match serde_json::from_str(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                debug!(
                                    node = %node_id,
                                    err = %e,
                                    "skipping unparseable message from peer"
                                );
                                continue;
                            }
                        };

                        match msg {
                            Message::Peer(peer_msg) => {
                                if inbound_tx.send(*peer_msg).await.is_err() {
                                    debug!(
                                        node = %node_id,
                                        "inbound channel closed, stopping reader"
                                    );
                                    break;
                                }
                            }
                            _ => {
                                // Silently ignore non-peer messages after handshake.
                                debug!(
                                    node = %node_id,
                                    "ignoring non-peer message from peer"
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        info!(node = %node_id, "peer socket EOF");
                        break;
                    }
                    Err(e) => {
                        error!(node = %node_id, err = %e, "error reading from peer socket");
                        break;
                    }
                }
            }
        });

        // Spawn writer task
        let node_id_w = self.expected_node_id.clone();
        let writer_handle = tokio::spawn(async move {
            let mut outbound_rx = outbound_rx;
            let mut writer = write_half;

            while let Some(peer_msg) = outbound_rx.recv().await {
                let msg = Message::Peer(Box::new(peer_msg));
                if let Err(e) = flotilla_protocol::framing::write_message_line(&mut writer, &msg).await {
                    error!(node = %node_id_w, err = %e, "failed to write to peer socket");
                    break;
                }
            }
        });

        self.task_handles.push(reader_handle);
        self.task_handles.push(writer_handle);

        Ok(inbound_rx)
    }

    /// Remove the local forwarded socket file if it exists.
    fn cleanup_socket(&self) {
        if self.local_socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.local_socket_path) {
                warn!(
                    path = %self.local_socket_path.display(),
                    err = %e,
                    "failed to remove stale forwarded socket"
                );
            }
        }
    }

    /// Kill the SSH child process if running.
    async fn kill_ssh(&mut self) {
        if let Some(ref mut child) = self.ssh_process {
            debug!(peer = %self.expected_node_id, "killing SSH process");
            // kill_on_drop is set, but explicitly kill for clean shutdown
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.ssh_process = None;
    }

    /// Abort reader/writer background tasks.
    fn abort_tasks(&mut self) {
        for handle in self.task_handles.drain(..) {
            handle.abort();
        }
    }

    /// Compute the backoff delay for a given reconnection attempt.
    ///
    /// Uses capped exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s, ...
    pub fn backoff_delay(attempt: u32) -> Duration {
        let delay = INITIAL_BACKOFF.checked_mul(2u32.saturating_pow(attempt.saturating_sub(1))).unwrap_or(MAX_BACKOFF);
        std::cmp::min(delay, MAX_BACKOFF)
    }

    fn validate_remote_hello(expected_node_id: &NodeId, hello: Message) -> Result<uuid::Uuid, String> {
        match hello {
            Message::Hello { protocol_version, node_id, session_id, .. } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(format!("peer protocol version mismatch: expected {}, got {}", PROTOCOL_VERSION, protocol_version));
                }
                if node_id != *expected_node_id {
                    return Err(format!("peer node mismatch: expected {}, got {}", expected_node_id, node_id));
                }
                Ok(session_id)
            }
            other => Err(format!("expected peer hello, got {:?}", other)),
        }
    }
}

#[async_trait]
impl PeerTransport for SshTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Connecting;

        self.spawn_ssh().await?;
        self.wait_for_socket().await.inspect_err(|_| {
            self.cleanup_socket();
        })?;
        let rx = self.connect_socket().await.inspect_err(|_| {
            self.cleanup_socket();
        })?;

        // Store the inbound receiver for subscribe() to return
        self.inbound_rx = Some(rx);

        self.status = PeerConnectionStatus::Connected;
        info!(peer = %self.expected_node_id, "peer connection established");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        info!(peer = %self.expected_node_id, "disconnecting peer transport");

        self.abort_tasks();

        // Drop channels
        self.inbound_rx = None;
        self.outbound_tx = None;
        self.remote_session_id = None;

        self.kill_ssh().await;
        self.cleanup_socket();

        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        if self.status != PeerConnectionStatus::Connected {
            return Err("not connected".to_string());
        }

        // Return the receiver from connect(). This is a one-shot call —
        // the receiver is produced during connect() and consumed here.
        self.inbound_rx.take().ok_or_else(|| "already subscribed (receiver already taken)".to_string())
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.outbound_tx
            .as_ref()
            .map(|tx| Arc::new(ChannelPeerSender { tx: tokio::sync::Mutex::new(Some(tx.clone())) }) as Arc<dyn PeerSender>)
    }

    fn remote_session_id(&self) -> Option<uuid::Uuid> {
        self.remote_session_id
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        // Abort background tasks synchronously — handles are cancel-safe
        self.abort_tasks();

        // Clean up the local socket file
        self.cleanup_socket();

        // ssh_process has kill_on_drop(true), so the SSH child is killed
        // automatically when the Child is dropped.
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::PeerDataMessage;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[test]
    fn backoff_delay_exponential_with_cap() {
        assert_eq!(SshTransport::backoff_delay(1), Duration::from_secs(1));
        assert_eq!(SshTransport::backoff_delay(2), Duration::from_secs(2));
        assert_eq!(SshTransport::backoff_delay(3), Duration::from_secs(4));
        assert_eq!(SshTransport::backoff_delay(4), Duration::from_secs(8));
        assert_eq!(SshTransport::backoff_delay(5), Duration::from_secs(16));
        assert_eq!(SshTransport::backoff_delay(6), Duration::from_secs(32));
        assert_eq!(SshTransport::backoff_delay(7), Duration::from_secs(60)); // capped
        assert_eq!(SshTransport::backoff_delay(8), Duration::from_secs(60)); // capped
        assert_eq!(SshTransport::backoff_delay(100), Duration::from_secs(60)); // capped
    }

    #[test]
    fn local_socket_path_uses_host_name() {
        let config = RemoteHostConfig {
            hostname: "10.0.0.5".to_string(),
            expected_host_name: "my-server".to_string(),
            user: Some("dev".to_string()),
            daemon_socket: "/run/user/1000/flotilla.sock".to_string(),
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("my-server".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");
        assert!(transport.local_socket_path.to_string_lossy().ends_with("peers/my-server.sock"));
    }

    #[test]
    fn rejects_host_name_with_path_separator() {
        let config = RemoteHostConfig {
            hostname: "10.0.0.5".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        match SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("../evil".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        ) {
            Err(e) => assert!(e.contains("path separators"), "unexpected error: {e}"),
            Ok(_) => panic!("should reject host name with path separators"),
        }
    }

    #[test]
    fn initial_status_is_disconnected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("remote".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn validate_remote_hello_accepts_matching_protocol_and_host() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("remote"),
            display_name: "remote".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
        };

        SshTransport::validate_remote_hello(&NodeId::new("remote"), hello).expect("matching hello should be accepted");
    }

    #[test]
    fn validate_remote_hello_rejects_wrong_protocol_version() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION + 1,
            node_id: NodeId::new("remote"),
            display_name: "remote".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
        };

        let err =
            SshTransport::validate_remote_hello(&NodeId::new("remote"), hello).expect_err("unexpected protocol version should be rejected");
        assert!(err.contains("protocol"));
    }

    #[test]
    fn validate_remote_hello_rejects_unexpected_node_id() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("someone-else"),
            display_name: "someone-else".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
        };

        let err = SshTransport::validate_remote_hello(&NodeId::new("remote"), hello).expect_err("unexpected node id should be rejected");
        assert!(err.contains("node"));
    }

    #[tokio::test]
    async fn send_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("remote".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");
        assert!(transport.sender().is_none(), "disconnected transport should not expose a sender");
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("remote".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");

        let result = transport.subscribe().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
    }

    #[tokio::test]
    #[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
    async fn connect_socket_preserves_peer_message_buffered_after_hello() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind listener");

        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local"),
            ConfigLabel("remote".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");
        transport.local_socket_path = socket_path.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut line = String::new();
            let mut reader = BufReader::new(&mut stream);
            reader.read_line(&mut line).await.expect("read hello");
            let hello = serde_json::to_string(&Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                node_id: NodeId::new("remote"),
                display_name: "remote".into(),
                session_id: uuid::Uuid::nil(),
                connection_role: None,
            })
            .expect("serialize hello");
            let peer = serde_json::to_string(&Message::Peer(Box::new(PeerWireMessage::Data(PeerDataMessage {
                origin_node_id: NodeId::new("remote"),
                repo_identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
                host_repo_root: Some(PathBuf::from("/home/remote/repo")),
                clock: flotilla_protocol::VectorClock::default(),
                kind: flotilla_protocol::PeerDataKind::Snapshot { data: Box::new(flotilla_protocol::ProviderData::default()), seq: 1 },
            }))))
            .expect("serialize peer");
            stream.write_all(format!("{hello}\n{peer}\n").as_bytes()).await.expect("write hello and peer");
        });

        let mut inbound = transport.connect_socket().await.expect("connect socket");
        let msg = inbound.recv().await.expect("first peer message");
        match msg {
            PeerWireMessage::Data(PeerDataMessage {
                origin_node_id,
                host_repo_root,
                kind: flotilla_protocol::PeerDataKind::Snapshot { seq, .. },
                ..
            }) => {
                assert_eq!(origin_node_id, NodeId::new("remote"));
                assert_eq!(host_repo_root, Some(PathBuf::from("/home/remote/repo")));
                assert_eq!(seq, 1);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        transport.disconnect().await.expect("disconnect cleanly");
        server.await.expect("server task");
    }

    /// Integration test that requires a real SSH setup and running daemon.
    /// Run manually with: `cargo test -p flotilla-daemon ssh_transport_connects -- --ignored`
    #[tokio::test]
    #[ignore] // requires SSH setup and a running remote daemon
    async fn ssh_transport_connects() {
        let config = RemoteHostConfig {
            hostname: "localhost".to_string(),
            expected_host_name: "localhost-test".to_string(),
            user: None,
            daemon_socket: "/tmp/flotilla-test-daemon.sock".to_string(),
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local-test"),
            ConfigLabel("localhost-test".to_string()),
            config,
            uuid::Uuid::nil(),
            Path::new("/tmp/flotilla-test"),
        )
        .expect("valid host name");

        transport.connect().await.expect("should connect to localhost daemon");
        assert_eq!(transport.status(), PeerConnectionStatus::Connected);

        transport.disconnect().await.expect("should disconnect cleanly");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }
}
