//! #3618: crews on an agentless SSH host reach their controlling daemon as a
//! Docker vessel does, through a per-environment socket. A dedicated `ssh -R`
//! per host carries that socket to an owner-only path on the host, and the
//! controller installs its own generation's `flotilla` CLI beside it.
//!
//! The forward follows the peer transport's StreamLocal pattern (stale remote
//! socket removal, `ExitOnForwardFailure`) through the neutral
//! `tender::ssh` helper; it never forwards the main daemon socket.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_discovery_api::EnvironmentBag;
use flotilla_protocol::{DaemonHostPath, EnvironmentId};
use flotilla_provider_runtime::{ChannelLabel, CommandProcess, CommandRunner};
use tokio::{
    sync::{watch, Mutex},
    task::JoinHandle,
};
use tracing::{info, warn};

use flotilla_environment::environment::{CONTAINED_DAEMON_REQUIRED_ENV, DAEMON_SOCKET_ENV};

use super::discovery::AgentlessSshProfile;
use crate::server::environment_sockets::{directory_name, EnvironmentSocketService};

/// Darwin's sun_path bound, as for local environment sockets.
const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Health of one host's forward. Sessions bake the remote path in at launch,
/// so a re-established forward serves them again without a relaunch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForwardHealth {
    Starting,
    Up,
    Down(String),
}

impl ForwardHealth {
    /// The reason a vessel shows while its crews cannot reach the daemon.
    pub(crate) fn unreachable_reason(&self) -> Option<String> {
        match self {
            Self::Down(reason) => Some(format!("daemon forward down: {reason}")),
            Self::Starting | Self::Up => None,
        }
    }
}

/// Operating system and architecture, normalised as Rust names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostPlatform {
    pub(crate) os: String,
    pub(crate) arch: String,
}

impl HostPlatform {
    pub(crate) fn controller() -> Self {
        Self { os: std::env::consts::OS.to_string(), arch: std::env::consts::ARCH.to_string() }
    }

    /// From `uname -s` and `uname -m`.
    pub(crate) fn from_uname(system: &str, machine: &str) -> Self {
        let os = match system.trim() {
            "Linux" => "linux".to_string(),
            "Darwin" => "macos".to_string(),
            other => other.to_ascii_lowercase(),
        };
        let arch = match machine.trim() {
            "amd64" | "x86_64" => "x86_64".to_string(),
            "arm64" | "aarch64" => "aarch64".to_string(),
            other => other.to_ascii_lowercase(),
        };
        Self { os, arch }
    }

    pub(crate) async fn probe(runner: &dyn CommandRunner) -> Result<Self, String> {
        let output =
            tokio::time::timeout(Duration::from_secs(15), runner.run("uname", &["-s", "-m"], Path::new("/"), &ChannelLabel::Default))
                .await
                .map_err(|_| "timed out probing host platform".to_string())?
                .map_err(|error| format!("probe host platform: {error}"))?;
        let mut words = output.split_whitespace();
        match (words.next(), words.next()) {
            (Some(system), Some(machine)) => Ok(Self::from_uname(system, machine)),
            _ => Err(format!("unexpected `uname -s -m` output: {:?}", output.trim())),
        }
    }
}

impl std::fmt::Display for HostPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.os, self.arch)
    }
}

/// Where the controller finds the `flotilla` CLI for a host's platform.
#[derive(Clone, Debug)]
pub(crate) struct CliSource {
    /// The CLI beside the running daemon, from the controller's own generation.
    own_binary: Result<PathBuf, String>,
    /// The generation every delivered CLI must match.
    generation: String,
}

impl CliSource {
    pub(crate) fn new(own_binary: Result<PathBuf, String>, generation: impl Into<String>) -> Self {
        Self { own_binary, generation: generation.into() }
    }

    pub(crate) fn for_controller() -> Self {
        let own_binary = std::env::current_exe()
            .map_err(|error| format!("resolve running daemon binary: {error}"))
            .and_then(|daemon| crate::environment_tools::adjacent_flotilla_binary(&daemon))
            .map(DaemonHostPath::into_path_buf);
        Self::new(own_binary, flotilla_daemon_api::build_info::build_id())
    }

    /// A release ships other platforms' CLIs at `platforms/<os>-<arch>/bin/flotilla`
    /// beside its own `bin/`, so a match is from this generation by construction.
    /// The controller's own CLI serves only a host on the controller's platform.
    pub(crate) fn for_platform(&self, platform: &HostPlatform) -> Result<PathBuf, String> {
        let own = self.own_binary.as_ref().map_err(|error| format!("no flotilla CLI for {platform}: {error}"))?;
        let platform_build =
            own.parent().and_then(Path::parent).map(|release| release.join("platforms").join(platform.to_string()).join("bin/flotilla"));
        if let Some(candidate) = platform_build.as_ref().filter(|candidate| candidate.is_file()) {
            return Ok(candidate.clone());
        }
        if *platform == HostPlatform::controller() {
            return Ok(own.clone());
        }
        Err(format!(
            "generation {} has no flotilla CLI build for {platform} (looked for {}; the controller runs {})",
            self.generation,
            platform_build.map_or_else(|| "a platforms directory".to_string(), |path| path.display().to_string()),
            HostPlatform::controller(),
        ))
    }

    fn generation_directory(&self) -> String {
        let name: String =
            self.generation.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
        if name.is_empty() {
            "unknown".into()
        } else {
            name
        }
    }
}

/// One agentless host's Environment, as its controller reaches it.
#[derive(Clone)]
pub(crate) struct ReachTarget {
    pub(crate) environment: EnvironmentId,
    pub(crate) destination: String,
    /// Runs commands on the host (the agentless SSH runner).
    pub(crate) remote: Arc<dyn CommandRunner>,
    pub(crate) env_bag: EnvironmentBag,
}

impl From<&AgentlessSshProfile> for ReachTarget {
    fn from(profile: &AgentlessSshProfile) -> Self {
        Self {
            environment: profile.environment_id.clone(),
            destination: profile.destination.clone(),
            remote: Arc::clone(&profile.runner),
            env_bag: profile.env_bag.clone(),
        }
    }
}

/// Point a session at the forwarded socket and the delivered CLI. The CLI
/// never spawns a daemon of its own there: a missing socket means the forward
/// is down, which the controller reports and repairs.
pub(crate) fn apply_reach_environment(env: &mut Vec<(String, String)>, environment: &str, paths: &ReachPaths, host_path: Option<&str>) {
    let base_path = env
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| host_path.map(str::to_string))
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
    let cli = paths.cli_directory.to_string_lossy();
    let path = if base_path.split(':').next() == Some(cli.as_ref()) { base_path } else { format!("{cli}:{base_path}") };
    env.retain(|(name, _)| {
        !matches!(name.as_str(), "PATH" | "FLOTILLA_ENVIRONMENT_ID" | DAEMON_SOCKET_ENV | CONTAINED_DAEMON_REQUIRED_ENV)
    });
    env.extend([
        (DAEMON_SOCKET_ENV.to_string(), paths.remote_socket.to_string_lossy().into_owned()),
        ("FLOTILLA_ENVIRONMENT_ID".to_string(), environment.to_string()),
        (CONTAINED_DAEMON_REQUIRED_ENV.to_string(), "1".to_string()),
        ("PATH".to_string(), path),
    ]);
}

/// Remote locations a session on the host is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReachPaths {
    pub(crate) remote_socket: PathBuf,
    pub(crate) cli_directory: PathBuf,
}

impl ReachTarget {
    /// Deterministic, so sessions launched before a controller restart find
    /// the re-established forward at the path they were given.
    pub(crate) fn paths(&self, cli: &CliSource) -> Result<ReachPaths, String> {
        let home = self.env_bag.find_env_var("HOME").ok_or_else(|| format!("agentless host {} has no HOME", self.destination))?;
        let state = self.env_bag.find_env_var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| Path::new(home).join(".local/state"));
        // The login's runtime directory is owner-only; the state fallback is
        // made owner-only when the forward prepares it.
        let runtime = self
            .env_bag
            .find_env_var("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join("flotilla"))
            .unwrap_or_else(|| state.join("flotilla/run"));
        let remote_socket = runtime.join("env").join(directory_name(&self.environment)).join("flotilla.sock");
        let length = remote_socket.as_os_str().len();
        if length > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "remote environment socket path {} is {length} bytes; SUN_LEN permits at most {MAX_SOCKET_PATH_BYTES}",
                remote_socket.display()
            ));
        }
        Ok(ReachPaths { remote_socket, cli_directory: state.join("flotilla/agentless-cli").join(cli.generation_directory()) })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ForwardTiming {
    pub(crate) open_timeout: Duration,
    pub(crate) probe_interval: Duration,
    pub(crate) liveness_interval: Duration,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
}

impl Default for ForwardTiming {
    fn default() -> Self {
        Self {
            open_timeout: Duration::from_secs(20),
            probe_interval: Duration::from_millis(250),
            liveness_interval: Duration::from_secs(1),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

struct Forward {
    health: watch::Receiver<ForwardHealth>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Supervises one reverse forward per agentless Environment.
pub(crate) struct AgentlessReach {
    /// Runs the controller-side `ssh` processes.
    local: Arc<dyn CommandRunner>,
    sockets: EnvironmentSocketService,
    cli: CliSource,
    timing: ForwardTiming,
    forwards: Mutex<HashMap<EnvironmentId, Forward>>,
    /// Tombstones for torn-down Environments: a late observation of one of
    /// their sessions must not resurrect the forward or its socket. Provisioning
    /// the Environment again admits it ([`AgentlessReach::admit`]); the set is
    /// bounded by the configured agentless hosts, whose IDs are per host.
    retired: Mutex<HashSet<EnvironmentId>>,
    /// Serialises CLI installation, so concurrent launches never race one path.
    installing: Mutex<()>,
    /// Last successfully probed platform per Environment. A host's platform
    /// does not change under a running controller, so a transient probe
    /// failure keeps the known value instead of flapping readiness.
    platforms: Mutex<HashMap<EnvironmentId, HostPlatform>>,
}

impl Drop for AgentlessReach {
    fn drop(&mut self) {
        // Dropped at daemon shutdown, where nothing can await retirement: the
        // aborted supervisors drop their ssh clients, which the process runner
        // spawns kill-on-drop. The remote socket is removed before the next dial.
        for forward in self.forwards.get_mut().values() {
            let _ = forward.stop.send(true);
            forward.task.abort();
        }
    }
}

impl AgentlessReach {
    pub(crate) fn new(local: Arc<dyn CommandRunner>, sockets: EnvironmentSocketService, cli: CliSource, timing: ForwardTiming) -> Self {
        Self {
            local,
            sockets,
            cli,
            timing,
            forwards: Mutex::new(HashMap::new()),
            retired: Mutex::new(HashSet::new()),
            installing: Mutex::new(()),
            platforms: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn cli(&self) -> &CliSource {
        &self.cli
    }

    /// The host's platform, probed once and then remembered.
    pub(crate) async fn platform(&self, target: &ReachTarget) -> Result<HostPlatform, String> {
        match HostPlatform::probe(target.remote.as_ref()).await {
            Ok(platform) => {
                self.platforms.lock().await.insert(target.environment.clone(), platform.clone());
                Ok(platform)
            }
            Err(error) => self.platforms.lock().await.get(&target.environment).cloned().ok_or(error),
        }
    }

    /// Prepare a session launch: install the CLI, then start the forward and
    /// wait for its first outcome, so a host that cannot be reached fails the
    /// launch with the reason instead of starting a crew without hooks.
    pub(crate) async fn prepare_session(&self, target: &ReachTarget) -> Result<ReachPaths, String> {
        let paths = target.paths(&self.cli)?;
        self.install_cli(target, &paths).await?;
        let mut health = self.ensure_forward(target).await?;
        // Outlast the supervisor's own attempt budget, so its specific reason
        // (rather than a bare timeout) reaches the launch error.
        let budget = self.timing.open_timeout * 2;
        let outcome = tokio::time::timeout(budget, health.wait_for(|health| *health != ForwardHealth::Starting))
            .await
            .map_err(|_| format!("daemon forward to {} did not open within {budget:?}", target.destination))?
            .map_err(|_| "daemon forward supervisor stopped".to_string())?
            .clone();
        match outcome {
            ForwardHealth::Down(reason) => Err(format!("daemon forward to {} is down: {reason}", target.destination)),
            ForwardHealth::Starting | ForwardHealth::Up => Ok(paths),
        }
    }

    /// Start the forward unless its supervisor is running, without waiting.
    /// Observation calls this too, so a restarted controller re-establishes
    /// the forward for sessions that outlived it.
    pub(crate) async fn ensure_forward(&self, target: &ReachTarget) -> Result<watch::Receiver<ForwardHealth>, String> {
        let mut forwards = self.forwards.lock().await;
        if self.retired.lock().await.contains(&target.environment) {
            return Err(format!("environment {} has been torn down", target.environment));
        }
        if let Some(forward) = forwards.get(&target.environment) {
            if !forward.task.is_finished() {
                return Ok(forward.health.clone());
            }
        }
        let paths = target.paths(&self.cli)?;
        let local_socket = self.sockets.prepare_forwarded(target.environment.clone()).await?;
        let (health_tx, health) = watch::channel(ForwardHealth::Starting);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            Arc::clone(&self.local),
            target.clone(),
            paths.remote_socket,
            local_socket.into_path_buf(),
            self.timing,
            health_tx,
            stop_rx,
        ));
        forwards.insert(target.environment.clone(), Forward { health: health.clone(), stop, task });
        Ok(health)
    }

    pub(crate) async fn health(&self, environment: &EnvironmentId) -> Option<ForwardHealth> {
        self.forwards.lock().await.get(environment).map(|forward| forward.health.borrow().clone())
    }

    /// Environment (re-)provisioning: a torn-down Environment of the same name
    /// is a new incarnation, so its forward may start again.
    pub(crate) async fn admit(&self, environment: &EnvironmentId) {
        self.retired.lock().await.remove(environment);
    }

    /// Environment teardown: stop the forward, which removes its remote
    /// socket, and retire the local environment socket.
    pub(crate) async fn retire(&self, environment: &EnvironmentId) -> Result<(), String> {
        let forward = {
            let mut forwards = self.forwards.lock().await;
            self.retired.lock().await.insert(environment.clone());
            forwards.remove(environment)
        };
        if let Some(forward) = forward {
            let _ = forward.stop.send(true);
            if tokio::time::timeout(self.timing.open_timeout, forward.task).await.is_err() {
                warn!(%environment, "daemon forward did not stop in time");
            }
        }
        self.sockets.remove(environment).await
    }

    async fn install_cli(&self, target: &ReachTarget, paths: &ReachPaths) -> Result<(), String> {
        let _installing = self.installing.lock().await;
        let remote = target.remote.as_ref();
        let installed = paths.cli_directory.join("flotilla");
        let installed_text = installed.to_string_lossy();
        if remote.run("test", &["-x", &installed_text], Path::new("/"), &ChannelLabel::Default).await.is_ok() {
            return Ok(());
        }
        let platform = self.platform(target).await?;
        let source = self.cli.for_platform(&platform)?;
        remote
            .run("mkdir", &["-p", &paths.cli_directory.to_string_lossy()], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("create flotilla CLI directory on {}: {error}", target.destination))?;
        // Running crews exec this path: publish a complete, executable file by
        // rename, never a partly written or not-yet-executable one.
        let staged = paths.cli_directory.join(format!(".flotilla-{}", uuid::Uuid::new_v4()));
        let staged_text = staged.to_string_lossy();
        let published = async {
            remote.write_file_from(&source, &staged).await.map_err(|error| format!("copy: {error}"))?;
            remote
                .run("chmod", &["755", &staged_text], Path::new("/"), &ChannelLabel::Default)
                .await
                .map_err(|error| format!("chmod: {error}"))?;
            remote
                .run("mv", &["-f", "--", &staged_text, &installed_text], Path::new("/"), &ChannelLabel::Default)
                .await
                .map_err(|error| format!("publish: {error}"))
        }
        .await;
        if let Err(error) = published {
            let _ = remote.run("rm", &["-f", "--", &staged_text], Path::new("/"), &ChannelLabel::Default).await;
            return Err(format!("install flotilla CLI on {}: {error}", target.destination));
        }
        info!(destination = %target.destination, %platform, source = %source.display(), "installed flotilla CLI on agentless host");
        Ok(())
    }
}

/// Owner-only socket directory, with any stale socket removed: sshd binds the
/// reverse listener and would otherwise refuse while the dead file remains.
const PREPARE_REMOTE_SOCKET: &str = "umask 077 && mkdir -p \"$1\" && chmod 700 \"$1\" && rm -f -- \"$2\"";

async fn prepare_remote_socket(remote: &dyn CommandRunner, socket: &Path) -> Result<(), String> {
    let directory = socket.parent().ok_or("remote socket has no directory")?;
    remote
        .run(
            "sh",
            &["-c", PREPARE_REMOTE_SOCKET, "sh", &directory.to_string_lossy(), &socket.to_string_lossy()],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("prepare remote socket {}: {error}", socket.display()))
}

async fn wait_for_remote_socket(
    remote: &dyn CommandRunner,
    socket: &Path,
    child: &mut Box<dyn CommandProcess>,
    timing: ForwardTiming,
) -> Result<(), String> {
    let socket_text = socket.to_string_lossy();
    let deadline = tokio::time::Instant::now() + timing.open_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("ssh forward exited before listening ({status})"));
        }
        if remote.run("test", &["-S", &socket_text], Path::new("/"), &ChannelLabel::Default).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("remote socket {} did not appear within {:?}", socket.display(), timing.open_timeout));
        }
        tokio::time::sleep(timing.probe_interval).await;
    }
}

/// Resolve once retirement is requested (or its sender is gone), holding no
/// watch guard across the caller's await.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Stop a forward on retirement: kill the ssh client explicitly (rather than
/// relying on the runner's kill-on-drop) and remove the remote socket it bound.
async fn retire_child(child: &mut Box<dyn CommandProcess>, target: &ReachTarget, remote_socket: &Path) {
    let _ = child.kill().await;
    let socket = remote_socket.to_string_lossy();
    let _ = target.remote.run("rm", &["-f", "--", &socket], Path::new("/"), &ChannelLabel::Default).await;
    info!(environment = %target.environment, "daemon forward retired");
}

async fn supervise(
    local: Arc<dyn CommandRunner>,
    target: ReachTarget,
    remote_socket: PathBuf,
    local_socket: PathBuf,
    timing: ForwardTiming,
    health: watch::Sender<ForwardHealth>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = timing.initial_backoff;
    let environment = &target.environment;
    let args = tender::ssh::reverse_forwarding_arguments(&target.destination, &remote_socket, &local_socket);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    loop {
        // Only steps that own no process are cancelled by retirement; once
        // ssh is spawned, this loop holds it and stops it explicitly.
        let prepared = tokio::select! {
            prepared = prepare_remote_socket(target.remote.as_ref(), &remote_socket) => prepared,
            _ = stopped(&mut stop) => return,
        };
        let down = match prepared {
            Err(error) => error,
            Ok(()) => match local.spawn_long_lived("ssh", &args, Path::new("/"), &ChannelLabel::Default).await {
                Err(error) => format!("spawn ssh forward: {error}"),
                Ok(mut child) => {
                    let listening = tokio::select! {
                        listening = wait_for_remote_socket(target.remote.as_ref(), &remote_socket, &mut child, timing) => listening,
                        _ = stopped(&mut stop) => {
                            retire_child(&mut child, &target, &remote_socket).await;
                            return;
                        }
                    };
                    match listening {
                        Err(error) => {
                            let _ = child.kill().await;
                            error
                        }
                        Ok(()) => {
                            info!(%environment, destination = %target.destination, "daemon forward up");
                            health.send_replace(ForwardHealth::Up);
                            backoff = timing.initial_backoff;
                            loop {
                                tokio::select! {
                                    _ = stopped(&mut stop) => {
                                        retire_child(&mut child, &target, &remote_socket).await;
                                        return;
                                    }
                                    _ = tokio::time::sleep(timing.liveness_interval) => match child.try_wait() {
                                        Ok(None) => {}
                                        Ok(Some(status)) => break format!("ssh forward exited ({status})"),
                                        Err(error) => {
                                            let _ = child.kill().await;
                                            break format!("ssh forward unobservable: {error}");
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
            },
        };
        warn!(%environment, destination = %target.destination, reason = %down, ?backoff, "daemon forward down; re-establishing");
        health.send_replace(ForwardHealth::Down(down));
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stopped(&mut stop) => return,
        }
        backoff = (backoff * 2).min(timing.max_backoff);
    }
}

#[cfg(test)]
#[path = "agentless_reach/tests.rs"]
mod tests;
