use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use flotilla_core::config::{flotilla_config_dir, RemoteHostConfig};
use flotilla_protocol::{HostName, Message, PeerDataMessage};

use super::transport::{PeerConnectionStatus, PeerTransport};

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

/// SSH-based transport that forwards a remote daemon's Unix socket locally
/// and exchanges `PeerData` messages over it.
///
/// The transport spawns `ssh -N -L <local>:<remote> [user@]host` as a child
/// process, then connects to the locally-forwarded socket to read/write
/// JSON-line `Message` values. Only `Message::PeerData` messages are
/// forwarded; other message types on the wire are silently discarded.
pub struct SshTransport {
    config: RemoteHostConfig,
    host_name: HostName,
    local_socket_path: PathBuf,
    ssh_process: Option<tokio::process::Child>,
    status: PeerConnectionStatus,
    inbound_tx: Option<mpsc::Sender<PeerDataMessage>>,
    outbound_tx: Option<mpsc::Sender<PeerDataMessage>>,
    /// Holds JoinHandles for the reader and writer background tasks so we can
    /// abort them on disconnect.
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SshTransport {
    /// Create a new SSH transport for the given remote host.
    ///
    /// The local forwarded socket will be placed at
    /// `~/.config/flotilla/peers/<host-name>.sock`.
    pub fn new(host_name: HostName, config: RemoteHostConfig) -> Self {
        let local_socket_path = peers_dir().join(format!("{}.sock", host_name));

        Self {
            config,
            host_name,
            local_socket_path,
            ssh_process: None,
            status: PeerConnectionStatus::Disconnected,
            inbound_tx: None,
            outbound_tx: None,
            task_handles: Vec::new(),
        }
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
            host = %self.host_name,
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
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn ssh: {e}"))?;

        self.ssh_process = Some(child);
        Ok(())
    }

    /// Wait for the forwarded local socket file to appear on disk.
    async fn wait_for_socket(&self) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + SOCKET_POLL_TIMEOUT;

        loop {
            if self.local_socket_path.exists() {
                debug!(
                    path = %self.local_socket_path.display(),
                    "forwarded socket appeared"
                );
                return Ok(());
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

    /// Connect to the local forwarded socket and spawn reader/writer tasks.
    async fn connect_socket(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String> {
        let stream = UnixStream::connect(&self.local_socket_path)
            .await
            .map_err(|e| {
                format!(
                    "failed to connect to forwarded socket {}: {e}",
                    self.local_socket_path.display()
                )
            })?;

        let (read_half, write_half) = stream.into_split();

        // Inbound: reader task → inbound channel → subscriber
        let (inbound_tx, inbound_rx) = mpsc::channel::<PeerDataMessage>(CHANNEL_BUFFER);
        self.inbound_tx = Some(inbound_tx.clone());

        // Outbound: send() → outbound channel → writer task
        let (outbound_tx, outbound_rx) = mpsc::channel::<PeerDataMessage>(CHANNEL_BUFFER);
        self.outbound_tx = Some(outbound_tx);

        // Spawn reader task
        let host_name = self.host_name.clone();
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
                            Message::PeerData(peer_msg) => {
                                if inbound_tx.send(*peer_msg).await.is_err() {
                                    debug!(
                                        host = %host_name,
                                        "inbound channel closed, stopping reader"
                                    );
                                    break;
                                }
                            }
                            _ => {
                                // Silently ignore non-PeerData messages
                                debug!(
                                    host = %host_name,
                                    "ignoring non-PeerData message from peer"
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
        let host_name_w = self.host_name.clone();
        let writer_handle = tokio::spawn(async move {
            let mut outbound_rx = outbound_rx;
            let mut writer = write_half;

            while let Some(peer_msg) = outbound_rx.recv().await {
                let msg = Message::PeerData(Box::new(peer_msg));
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
            debug!(host = %self.host_name, "killing SSH process");
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
        self.connect_socket().await.inspect_err(|_| {
            self.cleanup_socket();
        })?;

        self.status = PeerConnectionStatus::Connected;
        info!(host = %self.host_name, "peer connection established");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        info!(host = %self.host_name, "disconnecting peer transport");

        self.abort_tasks();

        // Drop channel senders
        self.inbound_tx = None;
        self.outbound_tx = None;

        self.kill_ssh().await;
        self.cleanup_socket();

        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String> {
        // If we're already connected and have an inbound_tx, create a new
        // receiver by replacing the channel. For simplicity, we reconnect
        // the socket to get a fresh inbound channel.
        if self.status != PeerConnectionStatus::Connected {
            return Err("not connected".to_string());
        }

        // Re-establish the socket connection to get a fresh inbound channel.
        // First abort existing tasks.
        self.abort_tasks();

        let rx = self.connect_socket().await?;
        Ok(rx)
    }

    async fn send(&self, msg: PeerDataMessage) -> Result<(), String> {
        let tx = self
            .outbound_tx
            .as_ref()
            .ok_or_else(|| "not connected".to_string())?;

        tx.send(msg)
            .await
            .map_err(|_| "outbound channel closed".to_string())
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
            user: Some("dev".to_string()),
            daemon_socket: "/run/user/1000/flotilla.sock".to_string(),
        };
        let transport = SshTransport::new(HostName::new("my-server"), config);
        assert!(transport
            .local_socket_path
            .to_string_lossy()
            .ends_with("peers/my-server.sock"));
    }

    #[test]
    fn initial_status_is_disconnected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        let transport = SshTransport::new(HostName::new("remote"), config);
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }

    #[tokio::test]
    async fn send_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        let transport = SshTransport::new(HostName::new("remote"), config);

        let msg = PeerDataMessage {
            origin_host: HostName::new("local"),
            repo_identity: flotilla_protocol::RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/tmp/repo"),
            kind: flotilla_protocol::PeerDataKind::RequestResync { since_seq: 0 },
        };

        let result = transport.send(msg).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            user: None,
            daemon_socket: "/tmp/daemon.sock".to_string(),
        };
        let mut transport = SshTransport::new(HostName::new("remote"), config);

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
            user: None,
            daemon_socket: "/tmp/flotilla-test-daemon.sock".to_string(),
        };
        let mut transport = SshTransport::new(HostName::new("localhost-test"), config);

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
