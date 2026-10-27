//! #3792: vessel agents on a host flotilla reaches over SSH and runs no daemon
//! on (an SSH host). Flotilla owns the host. Each vessel gets its own
//! supervised drydock agent in a private directory, bootstrapped through the
//! host's existing command runner and reached through drydock's SSH transport.
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use drydock_agent::Client;
use drydock_protocol::Platform;
use drydock_transport::{
    ssh::{GuestStreams, SshConfig},
    Limits, Transport,
};
use flotilla_provider_runtime::{execution_context::BaseLayer, ChannelLabel, CommandRunner};
use tokio::sync::OnceCell;

use super::{connect_current, AgentConnector};

/// The agent's control socket inside its vessel directory.
pub const AGENT_SOCKET: &str = "agent.sock";
/// The agent executable inside its vessel directory. Upgrades replace it there.
pub const AGENT_EXECUTABLE: &str = "drydock-agent";

/// The bootstrap script's exit status when the vessel has no executable yet.
const MISSING_EXECUTABLE: i32 = 64;

/// Start the vessel's supervisor unless it already runs. The supervisor's lock
/// makes this idempotent, so every reconnect may run it. The agent runs with
/// the login's identity and the prepared base layer, passed as `NAME=value`
/// words after the directory: never the SSH session's own environment.
const BOOTSTRAP: &str = r#"dir=$1
shift
umask 077
mkdir -p -- "$dir" && chmod -- 700 "$dir" || exit 1
[ -x "$dir/drydock-agent" ] || exit 64
exec env -i HOME="${HOME:-/}" USER="${USER:-}" LOGNAME="${LOGNAME:-}" SHELL="${SHELL:-/bin/sh}" \
  XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" "$@" \
  "$dir/drydock-agent" --supervise --socket "$dir/agent.sock""#;

/// Publish the copied executable only once it is complete and executable.
const INSTALL: &str = r#"chmod -- 700 "$1" && mv -f -- "$1" "$2""#;

/// Kill the supervisor and agent serving one socket, then remove the vessel's
/// directories. Both processes end their argv with `--socket <socket>`; the
/// supervisor is killed in the same signal, so it cannot restart the agent.
/// `ps` rather than `/proc` keeps this portable to macOS hosts.
const RETIRE: &str = r#"dir=$1
shift
suffix=" --socket $dir/agent.sock"
for round in 1 2 3; do
  pids=$(ps -axww -o pid= -o command= | awk -v s="$suffix" 'length($0) >= length(s) && substr($0, length($0) - length(s) + 1) == s { print $1 }')
  [ -n "$pids" ] || break
  kill -KILL $pids 2>/dev/null
  sleep 1
done
rm -rf -- "$dir" "$@""#;

/// The explicit recipe for reaching a host's agents. Drydock's SSH transport
/// reads no ambient SSH configuration or agent: the key file is named and the
/// host key pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshAgentReach {
    pub address: String,
    pub port: u16,
    pub user: String,
    /// The host's raw OpenSSH public key.
    pub host_key: String,
    pub identity_file: PathBuf,
}

/// Opens one transport to an agent socket on the host. Called for every
/// connection attempt, including the reconnects after an upgrade.
#[async_trait]
pub trait AgentTransport: Send + Sync {
    async fn open(&self, socket: &Path) -> io::Result<Transport>;
}

/// Drydock's SSH transport over a local forward to the agent socket. No sshd
/// configuration changes: hosts without root cannot permit remote Unix-socket
/// forwards, and guest-opened streams share the forward's yamux link instead.
pub struct SshAgentTransport {
    reach: SshAgentReach,
    connect_timeout: Duration,
}

impl SshAgentTransport {
    pub fn new(reach: SshAgentReach, connect_timeout: Duration) -> Self {
        Self { reach, connect_timeout }
    }
}

#[async_trait]
impl AgentTransport for SshAgentTransport {
    async fn open(&self, socket: &Path) -> io::Result<Transport> {
        let config = SshConfig {
            connect_timeout: self.connect_timeout,
            host: self.reach.address.clone(),
            port: self.reach.port,
            user: self.reach.user.clone(),
            host_key: self.reach.host_key.clone(),
            identity_file: self.reach.identity_file.clone(),
            agent_socket: socket.into(),
            guest_streams: GuestStreams::OverLocalForward,
        };
        drydock_transport::ssh::connect(config, Limits::default()).await
    }
}

/// Connects to one vessel's agent, bootstrapping it through the host runner
/// when it does not answer: after a host reboot, or once its supervisor died.
pub struct HostAgentConnector {
    host: Arc<dyn CommandRunner>,
    transport: Arc<dyn AgentTransport>,
    directory: PathBuf,
    platform: Platform,
    executable: PathBuf,
    executable_bytes: Arc<OnceCell<Vec<u8>>>,
    timeout: Duration,
    base: Vec<String>,
}

impl HostAgentConnector {
    /// `directory` is the vessel's private directory on the host; `executable`
    /// is the controller's agent build for the host's `platform`; `base` is
    /// the vessel's prepared base layer, which the agent process starts with.
    pub fn new(
        host: Arc<dyn CommandRunner>,
        transport: Arc<dyn AgentTransport>,
        directory: PathBuf,
        platform: Platform,
        executable: PathBuf,
        timeout: Duration,
        base: &BaseLayer,
    ) -> Self {
        let base = base.environment().into_iter().map(|(name, value)| format!("{name}={value}")).collect();
        Self { host, transport, directory, platform, executable, executable_bytes: Arc::new(OnceCell::new()), timeout, base }
    }

    /// Share one read of the agent build between a host's vessels.
    pub fn with_executable_cache(mut self, cache: Arc<OnceCell<Vec<u8>>>) -> Self {
        self.executable_bytes = cache;
        self
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.join(AGENT_SOCKET)
    }

    async fn executable(&self) -> Result<&[u8], String> {
        self.executable_bytes
            .get_or_try_init(|| async {
                let bytes = tokio::fs::read(&self.executable)
                    .await
                    .map_err(|e| format!("read drydock agent build {}: {e}", self.executable.display()))?;
                if bytes.is_empty() {
                    return Err(format!("drydock agent build {} is empty", self.executable.display()));
                }
                Ok::<_, String>(bytes)
            })
            .await
            .map(Vec::as_slice)
    }

    /// Start the supervisor, copying the executable in first if it is absent.
    pub async fn bootstrap(&self) -> Result<(), String> {
        if self.start().await? {
            return Ok(());
        }
        self.install().await?;
        if self.start().await? {
            return Ok(());
        }
        Err(format!("drydock agent in {} is still missing after install", self.directory.display()))
    }

    async fn start(&self) -> Result<bool, String> {
        let directory = self.directory.to_string_lossy();
        let mut args = vec!["-c", BOOTSTRAP, "flotilla-vessel-agent", &directory];
        args.extend(self.base.iter().map(String::as_str));
        let output = self
            .host
            .run_output("sh", &args, Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("start vessel agent in {directory}: {error}"))?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(MISSING_EXECUTABLE) => Ok(false),
            code => Err(format!("start vessel agent in {directory} exited {code:?}: {}", output.stderr.trim())),
        }
    }

    async fn install(&self) -> Result<(), String> {
        let staged = self.directory.join(format!(".{AGENT_EXECUTABLE}-{}", uuid::Uuid::new_v4()));
        let installed = self.directory.join(AGENT_EXECUTABLE);
        let published = async {
            self.host.write_file_from(&self.executable, &staged).await.map_err(|error| format!("copy: {error}"))?;
            self.host
                .run(
                    "sh",
                    &["-c", INSTALL, "flotilla-vessel-agent-install", &staged.to_string_lossy(), &installed.to_string_lossy()],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await
                .map_err(|error| format!("publish: {error}"))
        }
        .await;
        if let Err(error) = published {
            let _ = self.host.run("rm", &["-f", "--", &staged.to_string_lossy()], Path::new("/"), &ChannelLabel::Default).await;
            return Err(format!("install drydock agent in {}: {error}", self.directory.display()));
        }
        tracing::info!(directory = %self.directory.display(), source = %self.executable.display(), "installed vessel agent");
        Ok(())
    }
}

#[async_trait]
impl AgentConnector for HostAgentConnector {
    async fn connect(&self) -> Result<Client, String> {
        let executable = self.executable().await?;
        let socket = self.socket();
        // An agent from an earlier connect normally still runs under its
        // supervisor: try it once before touching the host.
        match Client::connect_current(|| self.transport.open(&socket), &self.platform, executable, self.timeout).await {
            Ok(client) => return Ok(client),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => return Err(error.to_string()),
            Err(error) => tracing::debug!(socket = %socket.display(), %error, "vessel agent did not answer; bootstrapping"),
        }
        self.bootstrap().await?;
        connect_current(|| self.transport.open(&socket), &self.platform, executable, self.timeout)
            .await
            .map_err(|error| format!("vessel agent at {}: {error}", socket.display()))
    }
}

/// Stop the agent serving `directory` and its supervisor, then remove
/// `directory` and every path in `also`. Stopped processes and missing paths
/// are not errors.
pub async fn retire_host_agent(host: &dyn CommandRunner, directory: &Path, also: &[PathBuf]) -> Result<(), String> {
    let directory = directory.to_string_lossy().into_owned();
    let also = also.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let mut args = vec!["-c", RETIRE, "flotilla-vessel-agent-retire", directory.as_str()];
    args.extend(also.iter().map(String::as_str));
    host.run("sh", &args, Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("retire vessel agent in {directory}: {error}"))
}

/// Drydock's name for a platform flotilla probed with `uname -s -m`.
pub fn platform_from_uname(system: &str, machine: &str) -> Platform {
    let os = match system.trim() {
        "Linux" => "linux".to_string(),
        "Darwin" => "darwin".to_string(),
        other => other.to_ascii_lowercase(),
    };
    let architecture = match machine.trim() {
        "amd64" | "x86_64" => "amd64".to_string(),
        "arm64" | "aarch64" => "arm64".to_string(),
        other => other.to_ascii_lowercase(),
    };
    Platform::new(os, architecture)
}

#[cfg(test)]
mod tests;
