use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::RemoteHostConfig,
    providers::{ChannelLabel, CommandOutput, CommandProcess, CommandRunner},
};
use flotilla_protocol::{ConfigLabel, GoodbyeReason, HostName, Message, NodeId, NodeInfo, PeerWireMessage, PROTOCOL_VERSION};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

use super::transport::{PeerConnectionStatus, PeerSender, PeerTransport};
use crate::DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH;

/// Maximum backoff delay between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Initial backoff delay between reconnection attempts.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// How long to wait for the forwarded socket to appear after spawning SSH.
const FORWARDED_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for an SSH discovery, cleanup, or diagnostic command.
const REMOTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between polls when waiting for the socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);

const REMOTE_SOCKET_PATH_PREFIX: &str = "FLOTILLA_DAEMON_SOCKET_PATH=";

const PRE_HELLO_CLOSE_ERROR: &str = "peer closed before sending hello";

const REMOTE_DAEMON_NOT_LISTENING: &str = "FLOTILLA_DAEMON_NOT_LISTENING";

/// Channel buffer size for inbound and outbound peer wire messages.
const CHANNEL_BUFFER: usize = 256;

pub(crate) fn peer_resource_socket_path(peer_resource_socket_dir: &Path, config_label: &ConfigLabel) -> Result<PathBuf, String> {
    // Sanitise: reject labels containing path separators to prevent path
    // traversal (e.g. `../` in hosts.toml).
    let name_str = config_label.0.as_str();
    if name_str.contains('/') || name_str.contains('\\') || name_str.contains('\0') {
        return Err(format!("peer host name must not contain path separators: {name_str:?}"));
    }
    Ok(peer_resource_socket_dir.join(format!("{}.sock", config_label.0)))
}

/// Path bound on the accepting peer by the reverse half of an SSH resource
/// tunnel. Both ends derive it from the accepting daemon socket and the
/// dialing node identity, so no follower-side peer configuration is needed.
pub(crate) fn reverse_peer_resource_socket_path(daemon_socket: &Path, dialing_node: &NodeId) -> Result<PathBuf, String> {
    let parent = daemon_socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("daemon socket has no parent directory: {}", daemon_socket.display()))?;
    let node_hash = dialing_node
        .as_str()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3));
    Ok(parent.join(format!(".peer-{node_hash:016x}")))
}

#[derive(Debug, Clone, Copy)]
pub struct SshTransportPaths<'a> {
    pub state_dir: &'a Path,
    pub daemon_socket: &'a Path,
}

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

/// SSH-based transport that forwards both daemons' Unix sockets over one SSH
/// connection and exchanges peer wire messages over the local forward.
///
/// The transport spawns
/// `ssh -N -L <local-resource>:<remote-daemon>
///          -R <remote-resource>:<local-daemon> [user@]host`
/// as a child process, then connects to the locally-forwarded socket to
/// read/write JSON-line `Message` values. Only `Message::Peer` payloads are
/// forwarded; other message types on the wire are silently discarded.
pub struct SshTransport {
    local_node_id: NodeId,
    local_display_name: String,
    config: RemoteHostConfig,
    config_label: ConfigLabel,
    expected_host_name: HostName,
    expected_node_id: Option<NodeId>,
    local_socket_path: PathBuf,
    local_daemon_socket_path: PathBuf,
    remote_daemon_socket_path: Option<PathBuf>,
    remote_resource_socket_path: Option<PathBuf>,
    ssh_binary: PathBuf,
    command_runner: Arc<dyn CommandRunner>,
    ssh_process: Option<Box<dyn CommandProcess>>,
    status: PeerConnectionStatus,
    /// Receiver for inbound peer wire messages, produced by `connect_socket()` and
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
    /// Remote node identity learned from the last successful hello handshake.
    remote_node_info: Option<NodeInfo>,
}

impl SshTransport {
    /// Create a new SSH transport for the given remote host.
    ///
    /// The local forwarded socket will be placed at
    /// `~/.config/flotilla/peers/<host-name>.sock`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local_node_id: NodeId,
        local_display_name: String,
        config_label: ConfigLabel,
        config: RemoteHostConfig,
        expected_node_id: Option<NodeId>,
        local_session_id: uuid::Uuid,
        command_runner: Arc<dyn CommandRunner>,
        paths: SshTransportPaths<'_>,
    ) -> Result<Self, String> {
        let local_socket_path = peer_resource_socket_path(&paths.state_dir.join("peers"), &config_label)?;
        let expected_host_name = HostName::new(&config.expected_host_name);

        Ok(Self {
            local_node_id,
            local_display_name,
            config,
            config_label,
            expected_host_name,
            expected_node_id,
            local_socket_path,
            local_daemon_socket_path: paths.daemon_socket.to_path_buf(),
            remote_daemon_socket_path: None,
            remote_resource_socket_path: None,
            ssh_binary: PathBuf::from("ssh"),
            command_runner,
            ssh_process: None,
            status: PeerConnectionStatus::Disconnected,
            inbound_rx: None,
            outbound_tx: None,
            task_handles: Vec::new(),
            local_session_id,
            remote_session_id: None,
            remote_node_info: None,
        })
    }

    /// Spawn the SSH child process that forwards the remote socket locally.
    async fn spawn_ssh(&mut self) -> Result<(), String> {
        // Clean up any stale local socket before spawning
        self.cleanup_socket();

        let remote_daemon_socket = self.resolve_remote_daemon_socket().await?;
        self.remote_resource_socket_path = Some(reverse_peer_resource_socket_path(&remote_daemon_socket, &self.local_node_id)?);
        self.remote_daemon_socket_path = Some(remote_daemon_socket);

        // OpenSSH's client-side StreamLocalBindUnlink option only applies to
        // the local (-L) socket. The reverse (-R) socket is bound by sshd and
        // survives a dead tunnel unless the server is configured to unlink
        // it. Remove our deterministic reverse socket explicitly so a stale
        // file cannot make every subsequent tunnel attempt fail.
        self.cleanup_remote_socket().await?;

        // Ensure peers directory exists
        if let Some(parent) = self.local_socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create peers directory: {e}"))?;
        }

        let (forward_spec, reverse_forward_spec) = self.resource_forward_specs();

        let destination = self.destination();

        info!(
            expected_host = %self.expected_host_name,
            label = %self.config_label.0,
            %destination,
            local_forward = %forward_spec,
            reverse_forward = %reverse_forward_spec,
            "spawning SSH tunnel"
        );

        let args = vec![
            "-N".to_string(),
            "-L".to_string(),
            forward_spec,
            "-R".to_string(),
            reverse_forward_spec,
            "-o".to_string(),
            "ExitOnForwardFailure=yes".to_string(),
            "-o".to_string(),
            "StreamLocalBindUnlink=yes".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            destination,
        ];
        let binary = self.ssh_binary.to_string_lossy();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let child = self
            .command_runner
            .spawn_long_lived(&binary, &arg_refs, Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("failed to spawn ssh: {error}"))?;

        self.ssh_process = Some(child);
        Ok(())
    }

    fn destination(&self) -> String {
        match &self.config.user {
            Some(user) => format!("{user}@{}", self.config.hostname),
            None => self.config.hostname.clone(),
        }
    }

    async fn resolve_remote_daemon_socket(&self) -> Result<PathBuf, String> {
        let destination = self.destination();
        let command = remote_socket_path_command();
        let output = tokio::time::timeout(REMOTE_COMMAND_TIMEOUT, self.run_ssh_output(&destination, command))
            .await
            .map_err(|_| format!("timed out resolving remote daemon socket path on {destination}"))?
            .map_err(|error| format!("failed to resolve remote daemon socket path on {destination}: {error}"))?;

        if !output.success {
            return Err(format!(
                "failed to resolve remote daemon socket path on {destination}: ssh exited unsuccessfully{}",
                if output.stderr.trim().is_empty() { String::new() } else { format!(": {}", output.stderr.trim()) }
            ));
        }

        parse_remote_socket_path(output.stdout.as_bytes())
            .map_err(|error| format!("invalid remote daemon socket path from {destination}: {error}"))
    }

    async fn cleanup_remote_socket(&self) -> Result<(), String> {
        let destination = self.destination();
        let path = self.remote_resource_socket_path()?.to_string_lossy();
        let command = self.remote_cleanup_command();
        debug!(%destination, remote_socket = %path, "removing stale reverse peer socket before SSH tunnel dial");

        let output = tokio::time::timeout(REMOTE_COMMAND_TIMEOUT, self.run_ssh_output(&destination, command))
            .await
            .map_err(|_| format!("timed out removing stale reverse peer socket at {path} on {destination}"))?
            .map_err(|e| format!("failed to run remote stale peer socket cleanup at {path} on {destination}: {e}"))?;

        if output.success {
            return Ok(());
        }

        Err(format!(
            "failed to remove stale reverse peer socket at {path} on {destination}: ssh exited unsuccessfully{}",
            if output.stderr.trim().is_empty() { String::new() } else { format!(": {}", output.stderr.trim()) }
        ))
    }

    async fn run_ssh_output(&self, destination: &str, command: String) -> Result<CommandOutput, String> {
        let binary = self.ssh_binary.to_string_lossy();
        // ssh hands the command line to the remote user's login shell, which
        // may be zsh or fish rather than POSIX sh. Pin the interpreter so the
        // login shell only forwards one quoted argument.
        let command = format!("sh -c {}", shell_quote(&command));
        self.command_runner
            .run_output(&binary, &["-T", "-o", "BatchMode=yes", destination, &command], Path::new("/"), &ChannelLabel::Default)
            .await
    }

    fn remote_cleanup_command(&self) -> String {
        self.remote_resource_socket_path()
            .map(|path| format!("rm -f -- {}", shell_quote(&path.to_string_lossy())))
            .expect("remote resource socket path is resolved before cleanup")
    }

    fn resource_forward_specs(&self) -> (String, String) {
        let remote_daemon_socket = self.remote_daemon_socket_path().expect("remote daemon socket path is resolved before forwarding");
        let remote_resource_socket = self.remote_resource_socket_path().expect("remote resource socket path is resolved before forwarding");
        (
            format!("{}:{}", self.local_socket_path.display(), remote_daemon_socket.display()),
            format!("{}:{}", remote_resource_socket.display(), self.local_daemon_socket_path.display()),
        )
    }

    fn remote_daemon_socket_path(&self) -> Result<&Path, String> {
        self.remote_daemon_socket_path.as_deref().ok_or_else(|| "remote daemon socket path has not been resolved".to_string())
    }

    fn remote_resource_socket_path(&self) -> Result<&Path, String> {
        self.remote_resource_socket_path.as_deref().ok_or_else(|| "remote resource socket path has not been resolved".to_string())
    }

    /// Wait for the forwarded local socket file to appear on disk.
    ///
    /// Also checks whether the SSH child has exited early (bad key,
    /// unreachable host, etc.) to fail fast instead of waiting the
    /// full timeout.
    async fn wait_for_socket(&mut self) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + FORWARDED_SOCKET_TIMEOUT;

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
            display_name: self.local_display_name.clone(),
            session_id: self.local_session_id,
            connection_role: None,
            surface: None,
        })
        .await?;

        let (read_half, write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let line = lines
            .next_line()
            .await
            .map_err(|e| format!("failed to read peer hello: {e}"))?
            .ok_or_else(|| PRE_HELLO_CLOSE_ERROR.to_string())?;
        let hello = serde_json::from_str(&line).map_err(|e| format!("failed to parse peer hello: {e}"))?;
        let (remote_node_info, remote_session_id) =
            Self::validate_remote_hello(&self.expected_host_name, self.expected_node_id.as_ref(), hello)?;
        self.remote_node_info = Some(remote_node_info.clone());
        self.remote_session_id = Some(remote_session_id);

        // Inbound: reader task → inbound channel → subscriber
        let (inbound_tx, inbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);

        // Outbound: send() → outbound channel → writer task
        let (outbound_tx, outbound_rx) = mpsc::channel::<PeerWireMessage>(CHANNEL_BUFFER);
        self.outbound_tx = Some(outbound_tx);

        // Spawn reader task
        let node_id = remote_node_info.node_id.clone();
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
        let node_id_w = remote_node_info.node_id.clone();
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

    async fn diagnose_pre_hello_close(&self) -> Option<String> {
        let remote_socket = self.remote_daemon_socket_path().ok()?;
        let destination = self.destination();
        let quoted_path = shell_quote(&remote_socket.to_string_lossy());
        let command = format!(
            "test -S {quoted_path} && {{ ! command -v ss >/dev/null 2>&1 || ss -xlH | grep -F -- {quoted_path} >/dev/null; }} || printf '%s\\n' {REMOTE_DAEMON_NOT_LISTENING}"
        );
        let output = tokio::time::timeout(REMOTE_COMMAND_TIMEOUT, self.run_ssh_output(&destination, command)).await.ok()?.ok()?;

        if output.success && output.stdout.lines().any(|line| line.trim() == REMOTE_DAEMON_NOT_LISTENING) {
            Some(format!(
                "remote daemon not listening at derived path {} on {destination} (peer closed before sending hello)",
                remote_socket.display()
            ))
        } else {
            None
        }
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
            debug!(expected_host = %self.expected_host_name, "killing SSH process");
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

    fn validate_remote_hello(
        expected_host_name: &HostName,
        expected_node_id: Option<&NodeId>,
        hello: Message,
    ) -> Result<(NodeInfo, uuid::Uuid), String> {
        match hello {
            Message::Hello { protocol_version, node_id, display_name, session_id, .. } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(format!("peer protocol version mismatch: expected {}, got {}", PROTOCOL_VERSION, protocol_version));
                }
                match expected_node_id {
                    Some(expected_node_id) if &node_id != expected_node_id => {
                        return Err(format!("peer node id mismatch: expected {expected_node_id}, got {node_id}"));
                    }
                    _ => {}
                }
                if display_name != expected_host_name.as_str() {
                    debug!(
                        expected_host = %expected_host_name,
                        remote_node = %node_id,
                        remote_display = %display_name,
                        "configured host name did not match remote hello display name"
                    );
                }
                Ok((NodeInfo::new(node_id, display_name), session_id))
            }
            other => Err(format!("expected peer hello, got {:?}", other)),
        }
    }
}

fn parse_remote_socket_path(stdout: &[u8]) -> Result<PathBuf, String> {
    let stdout = std::str::from_utf8(stdout).map_err(|error| format!("not UTF-8: {error}"))?;
    let mut paths = stdout.lines().filter_map(|line| line.strip_prefix(REMOTE_SOCKET_PATH_PREFIX));
    let path = paths.next().ok_or_else(|| "discovery command returned no tagged path".to_string())?.trim();
    if path.is_empty() {
        return Err("discovery command returned an empty tagged path".to_string());
    }
    if paths.next().is_some() {
        return Err("discovery command returned more than one tagged path".to_string());
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!("path is not absolute: {}", path.display()));
    }
    Ok(path)
}

fn remote_socket_path_command() -> String {
    // The remote login shell may be zsh, where the lowercase `path` variable is
    // an array tied to `PATH` — assigning it destroys command lookup for the
    // rest of the line. Keep every variable here outside the set of zsh tied
    // parameters (path, fpath, cdpath, manpath, ...).
    format!(
        "discovery=\"$HOME/.config/flotilla/{DAEMON_SOCKET_DISCOVERY_RELATIVE_PATH}\"; sock=$(cat \"$discovery\") || exit; case \"$sock\" in /*) ;; *) sock=\"$(dirname \"$discovery\")/$sock\" ;; esac; printf '{REMOTE_SOCKET_PATH_PREFIX}%s\\n' \"$sock\""
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[async_trait]
impl PeerTransport for SshTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Connecting;

        self.spawn_ssh().await?;
        self.wait_for_socket().await.inspect_err(|_| {
            self.cleanup_socket();
        })?;
        let rx = match self.connect_socket().await {
            Ok(rx) => rx,
            Err(error) => {
                self.cleanup_socket();
                if error == PRE_HELLO_CLOSE_ERROR {
                    return Err(self.diagnose_pre_hello_close().await.unwrap_or(error));
                }
                return Err(error);
            }
        };

        // Store the inbound receiver for subscribe() to return
        self.inbound_rx = Some(rx);

        self.status = PeerConnectionStatus::Connected;
        let peer =
            self.remote_node_info.as_ref().map(|node| node.node_id.clone()).unwrap_or_else(|| NodeId::new(self.config_label.0.clone()));
        info!(%peer, expected_host = %self.expected_host_name, "peer connection established");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        let peer = self.remote_node_info.as_ref().map(|node| node.node_id.to_string()).unwrap_or_else(|| self.config_label.0.clone());
        info!(peer, expected_host = %self.expected_host_name, "disconnecting peer transport");

        self.abort_tasks();

        // Drop channels
        self.inbound_rx = None;
        self.outbound_tx = None;
        self.remote_session_id = None;
        self.remote_node_info = None;

        self.kill_ssh().await;
        self.cleanup_socket();

        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    fn connection_address(&self) -> String {
        match &self.config.user {
            Some(user) => format!("ssh://{user}@{}", self.config.hostname),
            None => format!("ssh://{}", self.config.hostname),
        }
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

    fn remote_node_info(&self) -> Option<NodeInfo> {
        self.remote_node_info.clone()
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
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use tokio::io::AsyncWriteExt;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedCommand {
        binary: String,
        args: Vec<String>,
    }

    struct FakeCommandRunner {
        outputs: StdMutex<VecDeque<Result<CommandOutput, String>>>,
        commands: StdMutex<Vec<RecordedCommand>>,
        spawns: StdMutex<Vec<RecordedCommand>>,
        events: Arc<StdMutex<Vec<&'static str>>>,
    }

    impl FakeCommandRunner {
        fn new(outputs: Vec<Result<CommandOutput, String>>, events: Arc<StdMutex<Vec<&'static str>>>) -> Self {
            Self { outputs: StdMutex::new(outputs.into()), commands: StdMutex::new(Vec::new()), spawns: StdMutex::new(Vec::new()), events }
        }

        fn commands(&self) -> Vec<RecordedCommand> {
            self.commands.lock().expect("command lock").clone()
        }

        fn spawns(&self) -> Vec<RecordedCommand> {
            self.spawns.lock().expect("spawn lock").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeCommandRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            panic!("ssh transport only uses run_output")
        }

        async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.events.lock().expect("event lock").push("command");
            self.commands
                .lock()
                .expect("command lock")
                .push(RecordedCommand { binary: cmd.to_string(), args: args.iter().map(|arg| (*arg).to_string()).collect() });
            self.outputs.lock().expect("output lock").pop_front().expect("queued command output")
        }

        async fn spawn_long_lived(
            &self,
            cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
        ) -> Result<Box<dyn CommandProcess>, String> {
            self.events.lock().expect("event lock").push("spawn");
            self.spawns
                .lock()
                .expect("spawn lock")
                .push(RecordedCommand { binary: cmd.to_string(), args: args.iter().map(|arg| (*arg).to_string()).collect() });
            let local_forward =
                args.windows(2).find_map(|pair| (pair[0] == "-L").then_some(pair[1])).expect("tunnel spawn has a local forward");
            let (local_socket, _) = local_forward.split_once(':').expect("local forward spec");
            std::fs::write(local_socket, []).expect("simulate forwarded socket");
            Ok(Box::new(FakeCommandProcess))
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct FakeCommandProcess;

    #[async_trait]
    impl CommandProcess for FakeCommandProcess {
        fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
            Ok(None)
        }

        async fn kill(&mut self) -> Result<(), String> {
            Ok(())
        }

        async fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
            use std::os::unix::process::ExitStatusExt;

            Ok(std::process::ExitStatus::from_raw(0))
        }
    }

    fn successful_output(stdout: impl Into<String>) -> Result<CommandOutput, String> {
        Ok(CommandOutput { stdout: stdout.into(), stderr: String::new(), success: true })
    }

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
    fn remote_socket_path_parser_requires_one_absolute_path() {
        assert_eq!(
            parse_remote_socket_path(b"login banner\nFLOTILLA_DAEMON_SOCKET_PATH=/tmp/flotilla.sock\nmotd\n").expect("absolute path"),
            PathBuf::from("/tmp/flotilla.sock")
        );
        assert!(parse_remote_socket_path(b"relative.sock\n").expect_err("untagged path").contains("no tagged path"));
        assert!(parse_remote_socket_path(b"FLOTILLA_DAEMON_SOCKET_PATH=relative.sock\n")
            .expect_err("relative path")
            .contains("not absolute"));
        assert!(parse_remote_socket_path(b"FLOTILLA_DAEMON_SOCKET_PATH=\n").expect_err("empty path").contains("empty"));
        assert!(parse_remote_socket_path(b"FLOTILLA_DAEMON_SOCKET_PATH=/one.sock\nFLOTILLA_DAEMON_SOCKET_PATH=/two.sock\n")
            .expect_err("multiple paths")
            .contains("more than one"));
    }

    #[test]
    fn remote_socket_path_command_uses_shared_discovery_contract() {
        let command = remote_socket_path_command();
        assert!(command.contains("$HOME/.config/flotilla/run/socket-path"));
        assert!(command.contains("dirname"), "relative advertisements must be resolved beside the discovery file: {command}");
        assert!(command.contains(REMOTE_SOCKET_PATH_PREFIX));
    }

    #[test]
    fn remote_socket_path_command_avoids_zsh_tied_parameters() {
        // zsh ties the lowercase `path`, `fpath`, `cdpath`, and `manpath`
        // arrays to their uppercase environment counterparts; assigning any of
        // them clobbers the environment mid-command. On a zsh remote this made
        // `dirname` unresolvable and derived `/flotilla.sock` (2026-08-16,
        // kiwi→feta peering outage).
        let command = remote_socket_path_command();
        for tied in ["path=", "fpath=", "cdpath=", "manpath="] {
            assert!(
                !command.contains(&format!("; {tied}")) && !command.starts_with(tied),
                "discovery command must not assign the zsh tied parameter `{tied}`: {command}"
            );
        }
        for tied in ["$path", "$fpath", "$cdpath", "$manpath"] {
            assert!(!command.contains(tied), "discovery command must not read the zsh tied parameter `{tied}`: {command}");
        }
    }

    #[tokio::test]
    async fn remote_socket_path_is_resolved_again_after_it_moves() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let runner = Arc::new(FakeCommandRunner::new(
            vec![
                successful_output(format!("login banner\n{REMOTE_SOCKET_PATH_PREFIX}/run/first.sock\nmotd\n")),
                successful_output(format!("login banner\n{REMOTE_SOCKET_PATH_PREFIX}/run/moved.sock\nmotd\n")),
            ],
            events,
        ));
        let transport = test_transport(runner.clone());
        assert_eq!(transport.resolve_remote_daemon_socket().await.expect("first resolution"), PathBuf::from("/run/first.sock"));
        assert_eq!(transport.resolve_remote_daemon_socket().await.expect("second resolution"), PathBuf::from("/run/moved.sock"));

        let commands = runner.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], commands[1]);
        assert_eq!(commands[0].binary, "ssh");
        assert_eq!(&commands[0].args[..4], ["-T", "-o", "BatchMode=yes", "remote.example.invalid"]);
        assert_eq!(commands[0].args[4], format!("sh -c {}", shell_quote(&remote_socket_path_command())));
    }

    #[tokio::test]
    async fn pre_hello_close_names_remote_daemon_not_listening() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let runner = Arc::new(FakeCommandRunner::new(vec![successful_output(format!("{REMOTE_DAEMON_NOT_LISTENING}\n"))], events));
        let mut transport = test_transport(runner.clone());
        transport.remote_daemon_socket_path = Some(PathBuf::from("/remote/run/flotilla.sock"));

        let diagnosis = transport.diagnose_pre_hello_close().await.expect("probable cause");
        assert!(diagnosis.contains("remote daemon not listening at derived path /remote/run/flotilla.sock"));
        assert!(diagnosis.contains("peer closed before sending hello"));
        let command = runner.commands().pop().expect("diagnostic command");
        assert_eq!(&command.args[..4], ["-T", "-o", "BatchMode=yes", "remote.example.invalid"]);
        assert!(command.args[4].starts_with("sh -c "), "remote commands must pin the POSIX interpreter: {}", command.args[4]);
        assert!(command.args[4].contains("test -S "));
        assert!(command.args[4].contains("/remote/run/flotilla.sock"));
    }

    fn test_transport(command_runner: Arc<dyn CommandRunner>) -> SshTransport {
        SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("remote".into()),
            RemoteHostConfig {
                hostname: "remote.example.invalid".into(),
                expected_host_name: "remote".into(),
                expected_node_id: None,
                user: None,
                ssh_multiplex: None,
            },
            None,
            uuid::Uuid::nil(),
            command_runner,
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("test transport")
    }

    #[test]
    fn local_socket_path_uses_host_name() {
        let config = RemoteHostConfig {
            hostname: "10.0.0.5".to_string(),
            expected_host_name: "my-server".to_string(),
            expected_node_id: None,
            user: Some("dev".to_string()),
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("my-server".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid host name");
        assert!(transport.local_socket_path.to_string_lossy().ends_with("peers/my-server.sock"));
    }

    #[test]
    fn resource_socket_forward_specs_are_bidirectional() {
        let config = RemoteHostConfig {
            hostname: "peer-a.example.invalid".to_string(),
            expected_host_name: "peer-a".to_string(),
            expected_node_id: None,
            user: Some("test-user".to_string()),
            ssh_multiplex: None,
        };
        let local_node = NodeId::new("local-node");
        let mut transport = SshTransport::new(
            local_node.clone(),
            "local-host".into(),
            ConfigLabel("peer-a".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths {
                state_dir: Path::new("/home/test-local/.local/state/flotilla"),
                daemon_socket: Path::new("/home/test-local/.config/flotilla/flotilla.sock"),
            },
        )
        .expect("valid transport");
        transport.remote_daemon_socket_path = Some(PathBuf::from("/home/test-remote/.config/flotilla/run/flotilla.sock"));
        transport.remote_resource_socket_path = Some(
            reverse_peer_resource_socket_path(transport.remote_daemon_socket_path.as_deref().expect("remote daemon socket"), &local_node)
                .expect("reverse path"),
        );

        let (local_forward, reverse_forward) = transport.resource_forward_specs();

        assert_eq!(
            local_forward,
            "/home/test-local/.local/state/flotilla/peers/peer-a.sock:/home/test-remote/.config/flotilla/run/flotilla.sock"
        );
        assert_eq!(
            reverse_forward,
            format!(
                "{}:/home/test-local/.config/flotilla/flotilla.sock",
                reverse_peer_resource_socket_path(Path::new("/home/test-remote/.config/flotilla/run/flotilla.sock"), &local_node)
                    .expect("reverse path")
                    .display()
            )
        );
    }

    #[test]
    fn remote_socket_cleanup_command_quotes_the_derived_path() {
        let config = RemoteHostConfig {
            hostname: "peer-a.example.invalid".to_string(),
            expected_host_name: "peer-a".to_string(),
            expected_node_id: None,
            user: Some("test-user".to_string()),
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local-node"),
            "local-host".into(),
            ConfigLabel("peer-a".into()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid transport");
        transport.remote_resource_socket_path = Some(
            reverse_peer_resource_socket_path(Path::new("/home/O'Brien/.config/flotilla/run/flotilla.sock"), &NodeId::new("local-node"))
                .expect("reverse path"),
        );

        assert_eq!(
            transport.remote_cleanup_command(),
            format!(
                "rm -f -- '/home/O'\"'\"'Brien/.config/flotilla/run/{}'",
                transport
                    .remote_resource_socket_path()
                    .expect("remote resource socket")
                    .file_name()
                    .expect("socket file name")
                    .to_string_lossy()
            )
        );
    }

    #[tokio::test]
    async fn spawning_tunnel_removes_stale_reverse_socket_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let daemon_socket = tmp.path().join("flotilla.sock");
        let config = RemoteHostConfig {
            hostname: "peer-a.example.invalid".to_string(),
            expected_host_name: "peer-a".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let events = Arc::new(StdMutex::new(Vec::new()));
        let runner = Arc::new(FakeCommandRunner::new(
            vec![successful_output(format!("{REMOTE_SOCKET_PATH_PREFIX}{}\n", daemon_socket.display())), successful_output("")],
            events.clone(),
        ));
        let mut transport = SshTransport::new(
            NodeId::new("local-node"),
            "local-host".into(),
            ConfigLabel("peer-a".into()),
            config,
            None,
            uuid::Uuid::nil(),
            runner.clone(),
            SshTransportPaths { state_dir: tmp.path(), daemon_socket: &daemon_socket },
        )
        .expect("valid transport");

        transport.spawn_ssh().await.expect("stale cleanup and tunnel spawn should succeed");
        transport.wait_for_socket().await.expect("fake tunnel creates forwarded socket");

        assert_eq!(*events.lock().expect("event lock"), ["command", "command", "spawn"]);
        let commands = runner.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].args[4], format!("sh -c {}", shell_quote(&transport.remote_cleanup_command())));
        let tunnel = runner.spawns().pop().expect("tunnel invocation");
        let (forward, reverse) = transport.resource_forward_specs();
        assert_eq!(tunnel, RecordedCommand {
            binary: "ssh".into(),
            args: vec![
                "-N".into(),
                "-L".into(),
                forward,
                "-R".into(),
                reverse,
                "-o".into(),
                "ExitOnForwardFailure=yes".into(),
                "-o".into(),
                "StreamLocalBindUnlink=yes".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                "-o".into(),
                "ServerAliveCountMax=3".into(),
                "peer-a.example.invalid".into(),
            ],
        });
    }

    #[tokio::test]
    async fn remote_socket_cleanup_failure_names_the_stale_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let daemon_socket = tmp.path().join("flotilla.sock");
        let config = RemoteHostConfig {
            hostname: "peer-a.example.invalid".to_string(),
            expected_host_name: "peer-a".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let events = Arc::new(StdMutex::new(Vec::new()));
        let runner = Arc::new(FakeCommandRunner::new(
            vec![
                successful_output(format!("{REMOTE_SOCKET_PATH_PREFIX}{}\n", daemon_socket.display())),
                Ok(CommandOutput { stdout: String::new(), stderr: "cleanup-denied".into(), success: false }),
            ],
            events.clone(),
        ));
        let mut transport = SshTransport::new(
            NodeId::new("local-node"),
            "local-host".into(),
            ConfigLabel("peer-a".into()),
            config,
            None,
            uuid::Uuid::nil(),
            runner.clone(),
            SshTransportPaths { state_dir: tmp.path(), daemon_socket: &daemon_socket },
        )
        .expect("valid transport");

        let error = transport.spawn_ssh().await.expect_err("cleanup failure must stop the tunnel dial");

        assert!(error.contains("failed to remove stale reverse peer socket"), "unexpected error: {error}");
        assert!(error.contains(&transport.remote_resource_socket_path().expect("resolved reverse socket").to_string_lossy().into_owned()));
        assert!(error.contains("cleanup-denied"), "unexpected error: {error}");
        assert!(transport.ssh_process.is_none(), "tunnel must not spawn after cleanup failure");
        assert!(runner.spawns().is_empty(), "tunnel must not be invoked after cleanup failure");
        assert_eq!(*events.lock().expect("event lock"), ["command", "command"]);
    }

    #[test]
    fn rejects_host_name_with_path_separator() {
        let config = RemoteHostConfig {
            hostname: "10.0.0.5".to_string(),
            expected_host_name: "remote".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        match SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("../evil".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
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
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("remote".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid host name");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn validate_remote_hello_returns_remote_identity_from_hello() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("remote-node-1"),
            display_name: "remote-host".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
            surface: None,
        };

        let (node, session_id) =
            SshTransport::validate_remote_hello(&HostName::new("expected-host"), None, hello).expect("hello should be accepted");
        assert_eq!(node, NodeInfo::new(NodeId::new("remote-node-1"), "remote-host"));
        assert_eq!(session_id, uuid::Uuid::nil());
    }

    #[test]
    fn validate_remote_hello_rejects_wrong_protocol_version() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION + 1,
            node_id: NodeId::new("remote"),
            display_name: "remote".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
            surface: None,
        };

        let err = SshTransport::validate_remote_hello(&HostName::new("expected-host"), None, hello)
            .expect_err("unexpected protocol version should be rejected");
        assert!(err.contains("protocol"));
    }

    #[test]
    fn validate_remote_hello_accepts_matching_expected_node_id() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("expected-node"),
            display_name: "different-display".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
            surface: None,
        };

        let (node, _) = SshTransport::validate_remote_hello(&HostName::new("expected-host"), Some(&NodeId::new("expected-node")), hello)
            .expect("hello should be accepted");
        assert_eq!(node, NodeInfo::new(NodeId::new("expected-node"), "different-display"));
    }

    #[test]
    fn validate_remote_hello_rejects_mismatched_expected_node_id() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("actual-node"),
            display_name: "different-display".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
            surface: None,
        };

        let err = SshTransport::validate_remote_hello(&HostName::new("expected-host"), Some(&NodeId::new("expected-node")), hello)
            .expect_err("mismatched expected node id should be rejected");
        assert!(err.contains("node id"));
    }

    #[test]
    fn validate_remote_hello_allows_absent_expected_node_id() {
        let hello = Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: NodeId::new("someone-else"),
            display_name: "different-display".into(),
            session_id: uuid::Uuid::nil(),
            connection_role: None,
            surface: None,
        };

        let (node, _) =
            SshTransport::validate_remote_hello(&HostName::new("expected-host"), None, hello).expect("hello should be accepted");
        assert_eq!(node, NodeInfo::new(NodeId::new("someone-else"), "different-display"));
    }

    #[tokio::test]
    async fn send_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let transport = SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("remote".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid host name");
        assert!(transport.sender().is_none(), "disconnected transport should not expose a sender");
    }

    #[tokio::test]
    async fn subscribe_fails_when_not_connected() {
        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("remote".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid host name");

        let result = transport.subscribe().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
    }

    #[tokio::test]
    #[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
    async fn connect_socket_preserves_peer_message_buffered_after_hello() {
        let dir = flotilla_test_support::TestSocketDir::new();
        let socket_path = dir.socket_path("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind listener");

        let config = RemoteHostConfig {
            hostname: "example.com".to_string(),
            expected_host_name: "remote".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local"),
            "local-display".into(),
            ConfigLabel("remote".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
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
                surface: None,
            })
            .expect("serialize hello");
            let peer = serde_json::to_string(&Message::Peer(Box::new(PeerWireMessage::RouteAdvertisement {
                origin_node_id: NodeId::new("remote"),
                origin_display_name: "remote".into(),
                remaining_hops: 8,
                visited: vec![NodeId::new("remote")],
            })))
            .expect("serialize peer");
            stream.write_all(format!("{hello}\n{peer}\n").as_bytes()).await.expect("write hello and peer");
        });

        let mut inbound = transport.connect_socket().await.expect("connect socket");
        let msg = inbound.recv().await.expect("first peer message");
        match msg {
            PeerWireMessage::RouteAdvertisement { origin_node_id, remaining_hops, .. } => {
                assert_eq!(origin_node_id, NodeId::new("remote"));
                assert_eq!(remaining_hops, 8);
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
            expected_node_id: None,
            user: None,
            ssh_multiplex: None,
        };
        let mut transport = SshTransport::new(
            NodeId::new("local-test"),
            "local-display".into(),
            ConfigLabel("localhost-test".to_string()),
            config,
            None,
            uuid::Uuid::nil(),
            Arc::new(flotilla_core::providers::ProcessCommandRunner),
            SshTransportPaths { state_dir: Path::new("/tmp/flotilla-test"), daemon_socket: Path::new("/tmp/flotilla.sock") },
        )
        .expect("valid host name");

        transport.connect().await.expect("should connect to localhost daemon");
        assert_eq!(transport.status(), PeerConnectionStatus::Connected);

        transport.disconnect().await.expect("should disconnect cleanly");
        assert_eq!(transport.status(), PeerConnectionStatus::Disconnected);
    }
}
