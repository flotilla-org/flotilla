//! #3792: vessel agents on a host flotilla reaches over SSH and runs no daemon
//! on (an SSH host). Flotilla owns the host. Each vessel gets its own
//! supervised drydock agent in a private directory, bootstrapped through the
//! host's existing command runner and reached through drydock's SSH transport.
//!
//! #4087: the host user's vessels share one content store on durable disk
//! ([`SharedStore`]). Each agent names its vessel's state directory as its
//! holder, so retiring a vessel releases its uses, and the retirement's
//! collection removes what no live vessel uses.
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use drydock_agent::Client;
use drydock_transport::{ssh::SshConfig, ByteStream};
use flotilla_environment::environment::agent::{
    client::{AgentClient, Platform},
    AgentConnector,
};
use flotilla_provider_runtime::{execution_context::BaseLayer, ChannelLabel, CommandRunner};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use crate::agent::{connect_current, platform_of, DrydockClient};

/// The agent's control socket inside its vessel directory.
pub const AGENT_SOCKET: &str = "agent.sock";
/// The agent executable inside its vessel directory. Upgrades replace it there.
pub const AGENT_EXECUTABLE: &str = "drydock-agent";

/// The bootstrap script's exit status when the vessel has no executable yet.
const MISSING_EXECUTABLE: i32 = 64;

/// How long a store entry no vessel links outlives its last fetch: longer
/// than any vessel waits between fetching an artifact and linking it.
pub const STORE_GRACE: Duration = Duration::from_secs(3600);

/// Start the vessel's supervisor unless it already runs. The supervisor's lock
/// makes this idempotent, so every reconnect may run it. The agent runs with
/// the login's identity and the prepared base layer, passed as `NAME=value`
/// words after the directory, the store and the holder: never the SSH
/// session's own environment. Without a store the agent keeps its own beside
/// its executable.
const BOOTSTRAP: &str = r#"dir=$1
store=$2
holder=$3
shift 3
umask 077
mkdir -p -- "$dir" && chmod -- 700 "$dir" || exit 1
[ -x "$dir/drydock-agent" ] || exit 64
exec env -i HOME="${HOME:-/}" USER="${USER:-}" LOGNAME="${LOGNAME:-}" SHELL="${SHELL:-/bin/sh}" \
  XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" "$@" \
  "$dir/drydock-agent" --supervise ${store:+--store "$store" --holder "$holder"} --socket "$dir/agent.sock""#;

/// Publish the copied executable only once it is complete and executable.
const INSTALL: &str = r#"chmod -- 700 "$1" && mv -f -- "$1" "$2""#;

/// Kill the supervisor and agent serving one socket, then remove the vessel's
/// other directories (`also`), keeping its own for the collection that
/// follows. Both processes end their argv with `--socket <socket>`; the
/// supervisor is killed in the same signal, so it cannot restart the agent.
/// Drydock stores content-cache entries read-only, so the owner gets write
/// permission on the trees before they are removed.
/// `ps` rather than `/proc` keeps this portable to macOS hosts.
const STOP: &str = r#"dir=$1
shift
suffix=" --socket $dir/agent.sock"
for round in 1 2 3; do
  pids=$(ps -axww -o pid= -o command= | awk -v s="$suffix" 'length($0) >= length(s) && substr($0, length($0) - length(s) + 1) == s { print $1 }')
  [ -n "$pids" ] || break
  kill -KILL $pids 2>/dev/null
  sleep 1
done
[ "$#" -eq 0 ] && exit 0
chmod -R u+w -- "$@" 2>/dev/null
rm -rf -- "$@""#;

/// Remove the stopped vessel's own directory, with any private store in it.
const REMOVE: &str = r#"chmod -R u+w -- "$1" 2>/dev/null
rm -rf -- "$1""#;

/// The content store a host user's vessels share (#4087), on durable disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedStore {
    pub path: PathBuf,
    /// How long an entry nobody links survives a collection after its last fetch.
    pub grace: Duration,
}

impl SharedStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, grace: STORE_GRACE }
    }
}

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
    async fn open(&self, socket: &Path) -> io::Result<ByteStream>;
}

/// Drydock's SSH transport over a local forward to the agent socket. No sshd
/// configuration changes: hosts without root cannot permit remote Unix-socket
/// forwards; the agent never dials out, drydockd claims outward connections.
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
    async fn open(&self, socket: &Path) -> io::Result<ByteStream> {
        let config = SshConfig {
            connect_timeout: self.connect_timeout,
            host: self.reach.address.clone(),
            port: self.reach.port,
            user: self.reach.user.clone(),
            host_key: self.reach.host_key.clone(),
            identity_file: self.reach.identity_file.clone(),
            agent_socket: socket.into(),
        };
        drydock_transport::ssh::connect(config).await
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
    /// The vessel this agent serves, as its abandoned upgrades are logged.
    vessel: String,
    /// The shared store, and the directory whose existence keeps this
    /// vessel's links in use there.
    store: Option<(PathBuf, PathBuf)>,
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
        let vessel = directory.display().to_string();
        Self {
            host,
            transport,
            directory,
            platform,
            executable,
            executable_bytes: Arc::new(OnceCell::new()),
            timeout,
            base,
            vessel,
            store: None,
        }
    }

    /// Keep content in `store`, shared with the host's other vessels, with
    /// this vessel's links in use there while `holder` exists.
    pub fn with_store(mut self, store: PathBuf, holder: PathBuf) -> Self {
        self.store = Some((store, holder));
        self
    }

    /// Name the vessel in this connector's logs; its directory otherwise.
    pub fn with_vessel(mut self, vessel: impl Into<String>) -> Self {
        self.vessel = vessel.into();
        self
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
    /// An executable from an earlier generation may refuse this one's store
    /// options; it is replaced with this generation's build and started again.
    pub async fn bootstrap(&self) -> Result<(), String> {
        match self.start().await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => tracing::info!(vessel = %self.vessel, %error, "vessel agent did not start; installing this generation's build"),
        }
        self.install().await?;
        if self.start().await? {
            return Ok(());
        }
        Err(format!("drydock agent in {} is still missing after install", self.directory.display()))
    }

    async fn start(&self) -> Result<bool, String> {
        let directory = self.directory.to_string_lossy();
        let (store, holder) = match &self.store {
            Some((store, holder)) => (store.to_string_lossy(), holder.to_string_lossy()),
            None => (Default::default(), Default::default()),
        };
        let mut args = vec!["-c", BOOTSTRAP, "flotilla-vessel-agent", &directory, &store, &holder];
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
    async fn connect(&self) -> Result<Arc<dyn AgentClient>, String> {
        let executable = self.executable().await?;
        let watch = UpgradeWatch { connector: self, reached: AtomicBool::new(false), armed: true };
        let connected = self.connect_with(executable, &watch.reached).await;
        watch.settle(connected.as_ref().err());
        connected.map(|client| Arc::new(DrydockClient(client)) as Arc<dyn AgentClient>)
    }
}

/// How long the report of an abandoned upgrade waits for the agent's answer.
const ABANDONED_UPGRADE_PROBE: Duration = Duration::from_secs(10);

/// One connect, which may upload an agent upgrade. A connect that reached the
/// agent and then fails, or is dropped before it finishes, asks the agent how
/// much of this build's upload it holds, and logs an upgrade it left partway
/// (#4054). A connect that never reached the agent uploaded nothing.
struct UpgradeWatch<'a> {
    connector: &'a HostAgentConnector,
    /// Whether any transport to the agent opened during this connect.
    reached: AtomicBool,
    armed: bool,
}

impl UpgradeWatch<'_> {
    fn settle(mut self, error: Option<&String>) {
        self.armed = false;
        if let Some(error) = error {
            self.report(error.clone());
        }
    }

    fn report(&self, reason: String) {
        if !self.reached.load(Ordering::SeqCst) {
            return;
        }
        // A connect dropped as the runtime shuts down has nowhere to report from.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let (transport, socket, vessel) = (self.connector.transport.clone(), self.connector.socket(), self.connector.vessel.clone());
        let build = self.connector.executable_bytes.clone();
        runtime.spawn(async move {
            // Hashed here, off the failing caller's path: a connect that
            // succeeds pays nothing.
            let Ok(Some((digest, length))) =
                tokio::task::spawn_blocking(move || build.get().map(|bytes| (format!("{:x}", Sha256::digest(bytes)), bytes.len() as u64)))
                    .await
            else {
                return;
            };
            let progress = tokio::time::timeout(ABANDONED_UPGRADE_PROBE, async {
                let client = Client::connect(transport.open(&socket).await?, digest.clone()).await?;
                client.upload_offset(&digest).await
            })
            .await;
            match progress {
                Ok(Ok((sent, false))) if sent > 0 => tracing::warn!(
                    %vessel,
                    %digest,
                    bytes_sent = sent,
                    bytes_total = length,
                    %reason,
                    "vessel agent upgrade abandoned partway; its partial upload remains"
                ),
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%vessel, %digest, %reason, %error, "could not ask the vessel agent for its upgrade upload")
                }
                Err(_) => tracing::debug!(%vessel, %digest, %reason, "the vessel agent did not report its upgrade upload in time"),
            }
        });
    }
}

impl Drop for UpgradeWatch<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.report("the connect was dropped before it finished".into());
        }
    }
}

impl HostAgentConnector {
    async fn connect_with(&self, executable: &[u8], reached: &AtomicBool) -> Result<Client, String> {
        let socket = self.socket();
        let open = || async {
            let transport = self.transport.open(&socket).await;
            if transport.is_ok() {
                reached.store(true, Ordering::SeqCst);
            }
            transport
        };
        let platform = platform_of(&self.platform);
        // An agent from an earlier connect normally still runs under its
        // supervisor: try it once before touching the host.
        match Client::connect_current(open, &platform, executable, self.timeout).await {
            Ok(client) => return Ok(client),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => return Err(error.to_string()),
            Err(error) => tracing::debug!(socket = %socket.display(), %error, "vessel agent did not answer; bootstrapping"),
        }
        self.bootstrap().await?;
        connect_current(open, &platform, executable, self.timeout)
            .await
            .map_err(|error| format!("vessel agent at {}: {error}", socket.display()))
    }
}

/// Stop the agent serving `directory` and its supervisor, then remove
/// `directory` and every path in `also`. Stopped processes and missing paths
/// are not errors.
///
/// With a shared `store`, `also` holds the vessel's holder: once it is gone,
/// the vessel's own executable collects the store before its directory goes,
/// removing what no live vessel uses. A collection that cannot run, as after
/// a reboot cleared the executable, is logged and left to the next
/// retirement on the host.
pub async fn retire_host_agent(
    host: &dyn CommandRunner,
    directory: &Path,
    also: &[PathBuf],
    store: Option<&SharedStore>,
) -> Result<(), String> {
    let shown = directory.display();
    let directory_arg = directory.to_string_lossy();
    let also = also.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let mut args = vec!["-c", STOP, "flotilla-vessel-agent-retire", &directory_arg];
    args.extend(also.iter().map(String::as_str));
    host.run("sh", &args, Path::new("/"), &ChannelLabel::Default)
        .await
        .map_err(|error| format!("retire vessel agent in {shown}: {error}"))?;
    if let Some(store) = store {
        match collect_store(host, &directory.join(AGENT_EXECUTABLE), store).await {
            Ok(report) => tracing::info!(store = %store.path.display(), report = %report.trim(), "collected the shared content store"),
            Err(error) => tracing::warn!(store = %store.path.display(), %error, "could not collect the shared content store"),
        }
    }
    host.run("sh", &["-c", REMOVE, "flotilla-vessel-agent-remove", &directory_arg], Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("remove vessel agent directory {shown}: {error}"))
}

/// Remove what no live vessel uses from `store`, with the drydock agent at `executable`.
async fn collect_store(host: &dyn CommandRunner, executable: &Path, store: &SharedStore) -> Result<String, String> {
    let (store_arg, grace) = (store.path.to_string_lossy(), store.grace.as_secs().to_string());
    host.run(
        &executable.to_string_lossy(),
        &["--collect", "--store", &store_arg, "--grace", &grace],
        Path::new("/"),
        &ChannelLabel::Default,
    )
    .await
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
