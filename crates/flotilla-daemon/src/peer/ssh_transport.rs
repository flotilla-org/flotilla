use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use flotilla_core::config::{flotilla_config_dir, RemoteHostConfig};
use flotilla_protocol::{ConfigLabel, HostName, Message, PeerWireMessage, PROTOCOL_VERSION};

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
    tx: mpsc::Sender<PeerWireMessage>,
}

#[async_trait]
impl PeerSender for ChannelPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| "outbound channel closed".to_string())
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
    local_host: HostName,
    config: RemoteHostConfig,
    config_label: ConfigLabel,
    expected_host_name: HostName,
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
}

impl SshTransport {
    /// Create a new SSH transport for the given remote host.
    ///
    /// The local forwarded socket will be placed at
    /// `~/.config/flotilla/peers/<host-name>.sock`.
    pub fn new(
        local_host: HostName,
        config_label: ConfigLabel,
        config: RemoteHostConfig,
    ) -> Result<Self, String> {
        // Sanitise: reject host names containing path separators to prevent
        // path traversal (e.g. `../` in hosts.toml).
        let name_str = config_label.0.as_str();
        if name_str.contains('/') || name_str.contains('\\') || name_str.contains('\0') {
            return Err(format!(
                "peer host name must not contain path separators: {name_str:?}"
            ));
        }
        let local_socket_path = peers_dir().join(format!("{}.sock", config_label.0));
        let expected_host_name = HostName::new(&config.expected_host_name);

        Ok(Self {
            local_host,
            config,
            config_label,
            expected_host_name,
            local_socket_path,
            ssh_process: None,
            status: PeerConnectionStatus::Disconnected,
            inbound_rx: None,
            outbound_tx: None,
            task_handles: Vec::new(),
        })
    }

    /// Spawn the SSH child process that forwards the remote socket locally.
    async fn spawn_ssh(&mut self) -> Result<(), String> {
        // Clean up any stale local socket before spawning
        self.cleanup_socket();

        // Ensure peers directory exists
        let peers = peers_dir();
        std::fs::create_dir_all(&peers)
            .map_err(|e| format!("failed to create peers directory: {e}"))?;

        let forward_spec = format!(
            "{}:{}",
            self.local_socket_path.display(),
            self.config.daemon_socket
        );

        let destination = match &self.config.user {
            Some(user) => format!("{user}@{}", self.config.hostname),
            None => self.config.hostname.clone(),
        };

        info!(
            peer = %self.expected_host_name,
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
                return Err(format!(
                    "timed out waiting for forwarded socket at {}",
                    self.local_socket_path.display()
                ));
            }

            tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
        }
    }

    /// Connect to the local forwarded socket, complete the hello handshake,
    /// then spawn reader/writer tasks.
    async fn connect_socket(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        let mut stream = UnixStream::connect(&self.local_socket_path)
            .await
            .map_err(|e| {
                format!(
                    "failed to connect to forwarded socket {}: {e}",
                    self.local_socket_path.display()
                )
            })?;

        Self::write_message_line(
            &mut stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                host_name: self.local_host.clone(),
            },
        )
        .await?;

        let mut hello_reader = BufReader::new(stream);
        let mut line = String::new();
        let bytes = hello_reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("failed to read peer hello: {e}"))?;
        if bytes == 0 {
            return Err("peer closed before sending hello".to_string());
        }
        let hello = serde_json::from_str(line.trim_end())
            .map_err(|e| format!("failed to parse peer hello: {e}"))?;
        Self::validate_remote_hello(&self.expected_host_name, hello)?;

        let stream = hello_reader.into_inner();
        let (read_half, write_half) = stream.into_split();

        // Inbound: reader task → inbound channel → subscriber
        let (inbound_tx, inbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);

        // Outbound: send() → outbound channel → writer task
        let (outbound_tx, outbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);
        self.outbound_tx = Some(outbound_tx);

        // Spawn reader task
        let host_name = self.expected_host_name.clone();
        let reader_handle = tokio::spawn(async move {
            let reader = BufReader::new(read_half);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let msg: Message = match serde_json::from_str(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                debug!(
                                    host = %host_name,
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
                                        host = %host_name,
                                        "inbound channel closed, stopping reader"
                                    );
                                    break;
                                }
                            }
                            _ => {
                                // Silently ignore non-peer messages after handshake.
                                debug!(
                                    host = %host_name,
                                    "ignoring non-peer message from peer"
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        info!(host = %host_name, "peer socket EOF");
                        break;
                    }
                    Err(e) => {
                        error!(host = %host_name, err = %e, "error reading from peer socket");
                        break;
                    }
                }
            }
        });

        // Spawn writer task
        let host_name_w = self.expected_host_name.clone();
        let writer_handle = tokio::spawn(async move {
            let mut outbound_rx = outbound_rx;
            let mut writer = write_half;

            while let Some(peer_msg) = outbound_rx.recv().await {
                let msg = Message::Peer(Box::new(peer_msg));
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        error!(
                            host = %host_name_w,
                            err = %e,
                            "failed to serialize outbound message"
                        );
                        continue;
                    }
                };

                let mut buf = json.into_bytes();
                buf.push(b'\n');

                if let Err(e) = writer.write_all(&buf).await {
                    error!(host = %host_name_w, err = %e, "failed to write to peer socket");
                    break;
                }

                if let Err(e) = writer.flush().await {
                    error!(host = %host_name_w, err = %e, "failed to flush peer socket");
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
            debug!(peer = %self.expected_host_name, "killing SSH process");
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
        let delay = INITIAL_BACKOFF
            .checked_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
            .unwrap_or(MAX_BACKOFF);
        std::cmp::min(delay, MAX_BACKOFF)
    }

    fn validate_remote_hello(expected_host_name: &HostName, hello: Message) -> Result<(), String> {
        match hello {
            Message::Hello {
                protocol_version,
                host_name,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(format!(
                        "peer protocol version mismatch: expected {}, got {}",
                        PROTOCOL_VERSION, protocol_version
                    ));
                }
                if host_name != *expected_host_name {
                    return Err(format!(
                        "peer host mismatch: expected {}, got {}",
                        expected_host_name, host_name
                    ));
                }
                Ok(())
            }
            other => Err(format!("expected peer hello, got {:?}", other)),
        }
    }

    async fn write_message_line(stream: &mut UnixStream, msg: &Message) -> Result<(), String> {
        let json = serde_json::to_string(msg)
            .map_err(|e| format!("failed to serialize peer message: {e}"))?;
        stream
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("failed to write peer message: {e}"))?;
        stream
            .write_all(b"\n")
            .await
            .map_err(|e| format!("failed to terminate peer message: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("failed to flush peer message: {e}"))?;
        Ok(())
    }
}

/// Returns the `~/.config/flotilla/peers/` directory path.
fn peers_dir() -> PathBuf {
    flotilla_config_dir().join("peers")
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
        info!(peer = %self.expected_host_name, "peer connection established");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        info!(peer = %self.expected_host_name, "disconnecting peer transport");

        self.abort_tasks();

        // Drop channels
        self.inbound_rx = None;
        self.outbound_tx = None;

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
        self.inbound_rx
            .take()
            .ok_or_else(|| "already subscribed (receiver already taken)".to_string())
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.outbound_tx
            .as_ref()
            .map(|tx| Arc::new(ChannelPeerSender { tx: tx.clone() }) as Arc<dyn PeerSender>)
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
        };
        let transport = SshTransport::new(
            HostName::new("local"),
            ConfigLabel("my-server".to_string()),
            config,
        )
        .expect("valid host name");
        assert!(transport
            .local_socket_path
            .to_string_lossy()
            .ends_with("peers/my-server.sock"));
    }

    #[test]
    fn rejects_host_name_with_path_separator() {
        let config = RemoteHostConfig {
            hostname: "10.0.0.5".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        match SshTransport::new(
            HostName::new("local"),
            ConfigLabel("../evil".to_string()),
            config,
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
        };
        let transport = SshTransport::new(
            HostName::new("local"),
            ConfigLabel("remote".to_string()),
            config,
        )
        .expect("valid host name");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn validate_remote_hello_accepts_matching_protocol_and_host() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            host_name: HostName::new("remote"),
        };

        SshTransport::validate_remote_hello(&HostName::new("remote"), hello)
            .expect("matching hello should be accepted");
    }

    #[test]
    fn validate_remote_hello_rejects_wrong_protocol_version() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION + 1,
            host_name: HostName::new("remote"),
        };

        let err = SshTransport::validate_remote_hello(&HostName::new("remote"), hello)
            .expect_err("unexpected protocol version should be rejected");
        assert!(err.contains("protocol"));
    }

    #[test]
    fn validate_remote_hello_rejects_unexpected_host_name() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            host_name: HostName::new("someone-else"),
        };

        let err = SshTransport::validate_remote_hello(&HostName::new("remote"), hello)
            .expect_err("unexpected host name should be rejected");
        assert!(err.contains("host"));
    }

    #[tokio::test]
    async fn send_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        let transport = SshTransport::new(
            HostName::new("local"),
            ConfigLabel("remote".to_string()),
            config,
        )
        .expect("valid host name");
        assert!(
            transport.sender().is_none(),
            "disconnected transport should not expose a sender"
        );
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        let mut transport = SshTransport::new(
            HostName::new("local"),
            ConfigLabel("remote".to_string()),
            config,
        )
        .expect("valid host name");

        let result = transport.subscribe().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
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
        };
        let mut transport = SshTransport::new(
            HostName::new("local-test"),
            ConfigLabel("localhost-test".to_string()),
            config,
        )
        .expect("valid host name");

        transport
            .connect()
            .await
            .expect("should connect to localhost daemon");
        assert_eq!(transport.status(), PeerConnectionStatus::Connected);

        transport
            .disconnect()
            .await
            .expect("should disconnect cleanly");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }
}
