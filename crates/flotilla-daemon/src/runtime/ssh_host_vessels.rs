//! #3792: SSH hosts run each vessel through drydock's vessel agent,
//! as Docker vessels do since #3756. Flotilla keeps owning the host: its
//! lifecycle, capacity and placement. Only the way commands, files, forwards
//! and tool layers reach the host changes.
//!
//! Each vessel gets its own supervised agent in a private directory under the
//! remote user's `$XDG_RUNTIME_DIR`, bootstrapped through the host's SSH runner
//! and reached through drydock's SSH transport. Its tool layer and cleat state
//! live under the user's state directory, and its content-cache entries (pinned
//! harnesses, skill trees) in one store there that the host's vessels share
//! (#4087). The agent's pushed environment, its
//! forwards and its claimed endpoints belong to that vessel alone, however many
//! vessels share the host and its one Environment.
//!
//! Crews reach the daemon through a guest forward the agent serves at the
//! vessel's `flotilla.sock`, carried over the controller's own link to a
//! per-vessel socket serving the host's Environment. With the controller
//! gone, nothing forwards: the agent has no link to open streams on.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_config::ConfigStore;
use flotilla_discovery::{registry::ProviderRegistry, DiscoveryRuntime};
use flotilla_discovery_api::EnvironmentBag;
use flotilla_drydock::host::{platform_from_uname, retire_host_agent, HostAgentConnector, SharedStore, SshAgentTransport};
use flotilla_environment::environment::agent::{AgentRunner, WritableBases};
use flotilla_environment::environment::{VesselAgent, CONTAINED_DAEMON_REQUIRED_ENV, DAEMON_SOCKET_ENV};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::EnvironmentId;
use flotilla_provider_runtime::{
    execution_context::{BaseLayer, BoundLayer},
    ChannelLabel, CommandOutput, CommandProcess, CommandRunner,
};
use flotilla_resources::{TerminalSession, TerminalSessionTarget, Vessel};
use flotilla_store::ResourceBackend;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OnceCell};
use tracing::{error, info, warn};

use super::discovery::SshHostProfile;
use crate::environment_tools::{EnvironmentToolContext, EnvironmentToolProvisioner, SSH_HOST_PROVIDER_KIND};
use crate::server::environment_sockets::EnvironmentSocketService;

/// The guest forward that carries a vessel's crews to the daemon.
pub(super) const DAEMON_FORWARD: &str = "flotilla-daemon";

/// Darwin's sun_path bound, as for local environment sockets.
const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Room cleat needs below its runtime directory for `<daemon>/socket`.
const CLEAT_SOCKET_SUFFIX: usize = "/default-daemon-name/socket".len();

/// Which vessel a marker on the host belongs to, so a controller that
/// restarts can retire the agents of vessels removed while it was down.
const MARKER: &str = "vessel.json";

/// The login identity a vessel's processes run as, kept in its environment
/// when the host's discovery observed it. Locale comes with the base layer.
const IDENTITY_VARIABLES: &[&str] =
    &["HOME", "USER", "LOGNAME", "SHELL", "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"];

/// One vessel on one SSH host's Environment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct VesselKey {
    pub(super) environment: String,
    pub(super) vessel: String,
}

impl VesselKey {
    pub(super) fn new(environment: &EnvironmentId, vessel: &str) -> Self {
        Self { environment: environment.to_string(), vessel: vessel.to_string() }
    }

    /// A short, stable directory name. Both names are length-framed, so no
    /// pair of names can produce another pair's directory.
    fn directory(&self) -> String {
        let mut digest = Sha256::new();
        for part in [&self.environment, &self.vessel] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
        format!("{:x}", digest.finalize())[..16].to_string()
    }
}

/// Where one vessel lives on its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VesselPaths {
    /// The agent's private runtime directory: its executable, control socket,
    /// supervisor lock and log, and the crews' daemon socket.
    pub(super) agent: PathBuf,
    /// The vessel's durable state: its generation layer and cleat state.
    pub(super) state: PathBuf,
    /// The durable content store every vessel of the host user shares.
    pub(super) store: PathBuf,
}

impl VesselPaths {
    /// Deterministic, so sessions launched before a controller restart find
    /// their vessel where they left it.
    pub(super) fn new(bag: &EnvironmentBag, key: &VesselKey) -> Result<Self, String> {
        let paths =
            Self { agent: agent_root(bag)?.join(key.directory()), state: state_root(bag)?.join(key.directory()), store: store_root(bag)? };
        for (socket, room) in [
            (paths.daemon_socket(), 0),
            (paths.agent.join(flotilla_drydock::host::AGENT_SOCKET), 0),
            (paths.cleat_runtime(), CLEAT_SOCKET_SUFFIX),
        ] {
            let length = socket.as_os_str().len() + room;
            if length > MAX_SOCKET_PATH_BYTES {
                return Err(format!(
                    "vessel socket path {} needs {length} bytes; SUN_LEN permits at most {MAX_SOCKET_PATH_BYTES}",
                    socket.display()
                ));
            }
        }
        Ok(paths)
    }

    /// What crews' `FLOTILLA_DAEMON_SOCKET` names: the agent's guest forward.
    pub(super) fn daemon_socket(&self) -> PathBuf {
        self.agent.join("flotilla.sock")
    }

    /// The `PATH` directories of the layers published under the vessel's
    /// state, in their declared order (#3874). With [`Self::generation_variables`]
    /// this is the "generation layers" part of a prepared base layer (#3908).
    pub(super) fn generation_path(&self) -> Vec<String> {
        flotilla_environment::composition::flotilla_tools()
            .path_entries(&self.state)
            .into_iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect()
    }

    /// The variables those layers contribute, with their install paths.
    pub(super) fn generation_variables(&self) -> Vec<(String, String)> {
        flotilla_environment::composition::flotilla_tools().variables(&self.state)
    }

    /// Where the published CLI entry point lands in the vessel.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn cli(&self) -> PathBuf {
        let cli = flotilla_environment::composition::flotilla_tools().member(flotilla_environment::composition::role::CLI);
        self.state.join(cli.and_then(|member| member.install.as_deref()).expect("the CLI is published"))
    }

    pub(super) fn cleat_runtime(&self) -> PathBuf {
        self.state.join("cleat")
    }
}

/// `$XDG_RUNTIME_DIR/flotilla/v`: owner-only and on a tmpfs, so a reboot
/// clears it and the next connect bootstraps again. No root, no sticky
/// directory.
/// Every vessel agent directory of the login is a sibling here: retirement
/// finds a neighbour's executable to collect the shared store with by that
/// (`flotilla_drydock::host`'s retire script).
pub(super) fn agent_root(bag: &EnvironmentBag) -> Result<PathBuf, String> {
    bag.find_env_var("XDG_RUNTIME_DIR")
        .filter(|directory| directory.starts_with('/'))
        .map(|directory| Path::new(directory).join("flotilla/v"))
        .ok_or_else(|| "the host login has no XDG_RUNTIME_DIR for vessel agents".to_string())
}

pub(super) fn state_root(bag: &EnvironmentBag) -> Result<PathBuf, String> {
    Ok(durable_base(bag)?.join("vessels"))
}

/// The host user's shared content store: durable, unlike the runtime dir,
/// and one for all of the user's vessels (#4087).
pub(super) fn store_root(bag: &EnvironmentBag) -> Result<PathBuf, String> {
    Ok(durable_base(bag)?.join("content-store"))
}

/// `$XDG_STATE_HOME/flotilla`, or `~/.local/state/flotilla`.
fn durable_base(bag: &EnvironmentBag) -> Result<PathBuf, String> {
    let state = match bag.find_env_var("XDG_STATE_HOME").filter(|directory| directory.starts_with('/')) {
        Some(state) => PathBuf::from(state),
        None => Path::new(bag.find_env_var("HOME").ok_or("the host login has no HOME")?).join(".local/state"),
    };
    Ok(state.join("flotilla"))
}

/// A host's operating system and architecture, as drydock names them: one
/// normalisation of `uname` output ([`platform_from_uname`]) serves both the
/// comparison with the controller and the choice of agent build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostPlatform(flotilla_environment::platform::Platform);

impl HostPlatform {
    #[cfg(test)]
    pub(super) fn controller() -> Self {
        Self(flotilla_environment::platform::Platform::current())
    }

    /// From `uname -s` and `uname -m`.
    pub(super) fn from_uname(system: &str, machine: &str) -> Self {
        Self(platform_from_uname(system, machine))
    }

    pub(super) async fn probe(runner: &dyn CommandRunner) -> Result<Self, String> {
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

    /// The platform that selects the agent build.
    pub(super) fn drydock(&self) -> flotilla_environment::platform::Platform {
        self.0.clone()
    }
}

impl std::fmt::Display for HostPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.0.os, self.0.architecture)
    }
}

/// One vessel's agent, as the manager drives it.
#[async_trait]
pub(super) trait VesselHandle: Send + Sync {
    /// Commands and files in the vessel.
    fn runner(&self) -> Arc<dyn CommandRunner>;
    /// Readiness, tool layers and attach forwards.
    fn agent(&self) -> Arc<dyn VesselAgent>;
    /// Replace the environment every command in the vessel runs with.
    async fn configure_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String>;
    /// Serve `guest` in the vessel, carrying each connection to `host` here.
    async fn forward_daemon(&self, guest: &Path, host: &Path) -> Result<(), String>;
    /// Drop the connection and attach listeners; nothing reconnects after.
    async fn close(&self);
}

struct AgentVessel {
    agent: Arc<AgentRunner>,
    /// The host's own SSH runner, which a retired vessel's holders fall back to.
    host: Arc<dyn CommandRunner>,
}

#[async_trait]
impl VesselHandle for AgentVessel {
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(VesselRunner { agent: self.agent.clone(), host: self.host.clone() })
    }
    fn agent(&self) -> Arc<dyn VesselAgent> {
        Arc::new(self.agent.clone())
    }
    async fn configure_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        self.agent.configure_environment(environment).await
    }
    async fn forward_daemon(&self, guest: &Path, host: &Path) -> Result<(), String> {
        self.agent.add_outward_forward(DAEMON_FORWARD, guest, host).await
    }
    async fn close(&self) {
        self.agent.close().await;
    }
}

/// A vessel's runner as its callers keep it. Some keep it past the vessel:
/// a GitHub App token delivery refreshes through the runner it was first
/// delivered with, and the host's other vessels share that delivery. Once
/// the vessel is retired its agent handle is closed and never reconnects, so
/// such a caller falls back to the host's SSH runner rather than restarting a
/// retired vessel's agent.
struct VesselRunner {
    agent: Arc<AgentRunner>,
    host: Arc<dyn CommandRunner>,
}

impl VesselRunner {
    fn current(&self) -> &dyn CommandRunner {
        if self.agent.is_closed() {
            self.host.as_ref()
        } else {
            self.agent.as_ref()
        }
    }
}

#[async_trait]
impl CommandRunner for VesselRunner {
    fn scratch_namespace(&self) -> &str {
        self.current().scratch_namespace()
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.current().run(cmd, args, cwd, label).await
    }
    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        self.current().run_with_timeout(cmd, args, cwd, label, timeout).await
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.current().run_output(cmd, args, cwd, label).await
    }
    async fn run_output_with_grace(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        grace: Duration,
    ) -> Result<CommandOutput, String> {
        self.current().run_output_with_grace(cmd, args, cwd, label, grace).await
    }
    async fn spawn_long_lived(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        self.current().spawn_long_lived(cmd, args, cwd, label).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        self.current().run_with_input(cmd, args, cwd, label, input).await
    }
    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        self.current().run_to_file(cmd, args, cwd, destination).await
    }
    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        self.current().run_from_file(cmd, args, cwd, source).await
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.current().read_file_to(source, destination).await
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.current().write_file_from(source, destination).await
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.current().exists(cmd, args).await
    }
    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        self.current().path_exists(path).await
    }
    async fn same_filesystem(&self, source: &Path, destination: &Path) -> Result<bool, String> {
        self.current().same_filesystem(source, destination).await
    }
    async fn writable_scratch_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.current().writable_scratch_base(preferred, fallback).await
    }
    async fn writable_config_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.current().writable_config_base(preferred, fallback).await
    }
    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        self.current().ensure_file(path, content).await
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.current().write_file(path, content).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.current().write_file_with_mode(path, content, mode).await
    }
}

/// A host the controller cannot run vessels on, as a readiness condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostRefusal {
    pub(super) condition: &'static str,
    pub(super) reason: &'static str,
    pub(super) message: String,
    /// Whether placement must refuse the host. A probe that never succeeded
    /// (an SSH blip) is diagnostic only.
    pub(super) blocks_readiness: bool,
}

/// How the controller reaches and runs a host's vessel agents. Production
/// uses drydock's SSH transport; tests substitute the agent.
#[async_trait]
pub(super) trait VesselAgents: Send + Sync {
    async fn observe_platform(&self, profile: &SshHostProfile) -> Result<HostPlatform, String> {
        HostPlatform::probe(profile.runner.as_ref()).await
    }
    /// Why this controller cannot run vessels on the host, if it cannot.
    async fn host_refusals(&self, profile: &SshHostProfile) -> Vec<HostRefusal>;
    /// A handle on `vessel`'s agent at `paths`, with its attach forwards
    /// bound under `local` on the controller.
    async fn open(
        &self,
        profile: &SshHostProfile,
        vessel: &VesselKey,
        platform: &HostPlatform,
        paths: &VesselPaths,
        local: &Path,
    ) -> Result<Arc<dyn VesselHandle>, String>;
    /// The vessel's providers, discovered through its own agent.
    async fn probe(
        &self,
        discovery: &DiscoveryRuntime,
        config: &ConfigStore,
        profile: &SshHostProfile,
        handle: &dyn VesselHandle,
        environment: &BTreeMap<String, String>,
    ) -> Result<Arc<ProviderRegistry>, String>;
}

/// Finds the controller's agent build for a host platform.
type AgentBuild = Box<dyn Fn(&flotilla_environment::platform::Platform) -> Result<PathBuf, String> + Send + Sync>;

/// Drydock's SSH transport to agents copied from this generation.
pub(super) struct SshVesselAgents {
    agent_build: AgentBuild,
    /// Last successfully probed platform per host. A host's platform does not
    /// change under a running controller, so a transient probe failure keeps
    /// the known value instead of flapping readiness.
    platforms: StdMutex<HashMap<EnvironmentId, HostPlatform>>,
    /// One read of each agent build, shared by its hosts' vessels.
    builds: StdMutex<HashMap<PathBuf, Arc<OnceCell<Vec<u8>>>>>,
}

impl Default for SshVesselAgents {
    /// Resolve the agent from the complete verified guest catalog.
    fn default() -> Self {
        Self::with_agent_build(Box::new(|platform| {
            flotilla_environment::guest_tools::GuestTools::installed(platform).map(|tools| tools.agent())
        }))
    }
}

impl SshVesselAgents {
    pub(super) fn with_agent_build(agent_build: AgentBuild) -> Self {
        Self { agent_build, platforms: StdMutex::default(), builds: StdMutex::default() }
    }

    async fn platform(&self, profile: &SshHostProfile) -> Result<HostPlatform, String> {
        match HostPlatform::probe(profile.runner.as_ref()).await {
            Ok(platform) => {
                self.platforms.lock().expect("platforms").insert(profile.environment_id.clone(), platform.clone());
                Ok(platform)
            }
            Err(error) => self.platforms.lock().expect("platforms").get(&profile.environment_id).cloned().ok_or(error),
        }
    }

    /// Admit the observed guest target before selecting its complete catalog build.
    fn supported(&self, platform: &HostPlatform) -> Result<PathBuf, (&'static str, String)> {
        if platform.drydock() != flotilla_environment::guest_tools::linux_amd64() {
            return Err(("NoPlatformBuild", format!("unsupported guest platform {platform}; only linux/amd64 is admitted")));
        }
        (self.agent_build)(&platform.drydock()).map_err(|error| ("NoAgentBuild", format!("no drydock agent build: {error}")))
    }
}

#[async_trait]
impl VesselAgents for SshVesselAgents {
    async fn observe_platform(&self, profile: &SshHostProfile) -> Result<HostPlatform, String> {
        self.platform(profile).await
    }
    async fn host_refusals(&self, profile: &SshHostProfile) -> Vec<HostRefusal> {
        let mut refusals = Vec::new();
        let refuse = |reason, message: String, blocks_readiness| HostRefusal {
            condition: "Transport/VesselAgent",
            reason,
            message,
            blocks_readiness,
        };
        if let Err(error) = &profile.vessel_agent {
            refusals.push(refuse("MissingReach", error.clone(), true));
        }
        if let Err(error) = agent_root(&profile.env_bag) {
            refusals.push(refuse("NoRuntimeDirectory", error, true));
        }
        match self.platform(profile).await {
            Ok(platform) => {
                if let Err((reason, message)) = self.supported(&platform) {
                    refusals.push(HostRefusal { condition: "Tooling/VesselTools", reason, message, blocks_readiness: true });
                }
            }
            Err(error) => refusals.push(HostRefusal {
                condition: "Tooling/VesselTools",
                reason: "PlatformUnknown",
                message: error,
                blocks_readiness: false,
            }),
        }
        refusals
    }

    async fn open(
        &self,
        profile: &SshHostProfile,
        vessel: &VesselKey,
        platform: &HostPlatform,
        paths: &VesselPaths,
        local: &Path,
    ) -> Result<Arc<dyn VesselHandle>, String> {
        let reach = profile.vessel_agent.clone()?;
        let build = self.supported(platform).map_err(|(_, message)| message)?;
        let cache = self.builds.lock().expect("builds").entry(build.clone()).or_default().clone();
        let connector = HostAgentConnector::new(
            profile.runner.clone(),
            Arc::new(SshAgentTransport::new(reach, Duration::from_secs(30))),
            paths.agent.clone(),
            platform.drydock(),
            build,
            Duration::from_secs(60),
            &vessel_base(profile, paths)?,
        )
        .with_executable_cache(cache)
        .with_store(paths.store.clone())
        .with_vessel(format!("{}/{}", vessel.environment, vessel.vessel));
        let runner = AgentRunner::new(Arc::new(connector), local.into()).with_writable_bases(WritableBases::Host);
        Ok(Arc::new(AgentVessel { agent: Arc::new(runner), host: profile.runner.clone() }))
    }

    async fn probe(
        &self,
        discovery: &DiscoveryRuntime,
        config: &ConfigStore,
        profile: &SshHostProfile,
        handle: &dyn VesselHandle,
        environment: &BTreeMap<String, String>,
    ) -> Result<Arc<ProviderRegistry>, String> {
        let runner = handle.runner();
        let variables = environment.iter().map(|(key, value)| (key.clone(), value.clone())).collect::<HashMap<_, _>>();
        let bag = flotilla_discovery::run_provisioned_host_detectors(&discovery.host_detectors, runner.as_ref(), &variables).await;
        let root = ExecutionEnvironmentPath::new(&profile.provisioning.repo_default_dir);
        Ok(Arc::new(discovery.factories.probe_all(&bag, config, &root, runner).await))
    }
}

/// A prepared vessel: its agent ready, its generation layer published, its
/// environment pushed and its daemon forward up.
pub(super) struct VesselAccess {
    pub(super) key: VesselKey,
    pub(super) paths: VesselPaths,
    pub(super) handle: Arc<dyn VesselHandle>,
    pub(super) registry: Arc<ProviderRegistry>,
}

impl VesselAccess {
    pub(super) fn runner(&self) -> Arc<dyn CommandRunner> {
        self.handle.runner()
    }
}

/// What preparing a vessel needs from the controller.
pub(super) struct VesselPreparation<'a> {
    pub(super) tools: &'a EnvironmentToolProvisioner,
    pub(super) discovery: &'a DiscoveryRuntime,
    pub(super) config: &'a ConfigStore,
}

/// Lends a [`VesselPreparation`] to a preparation that outlives its caller.
pub(super) trait VesselPreparer: Send + Sync + 'static {
    fn preparation(&self) -> VesselPreparation<'_>;
}

#[derive(Default)]
struct VesselSlot {
    prepared: Mutex<Option<Arc<VesselAccess>>>,
}

/// The vessel agents on this controller's SSH hosts.
pub(super) struct SshHostVessels {
    agents: Arc<dyn VesselAgents>,
    sockets: EnvironmentSocketService,
    /// Controller-side directories for attach forwards, one per vessel.
    local_root: PathBuf,
    vessels: Mutex<HashMap<VesselKey, Arc<VesselSlot>>>,
    /// Tombstones for retired vessels: a late observation of one of their
    /// sessions must not resurrect the agent. A vessel live in the store again
    /// is admitted by [`SshHostVessels::retire_absent`].
    retired: Mutex<HashSet<VesselKey>>,
}

impl SshHostVessels {
    pub(super) fn new(agents: Arc<dyn VesselAgents>, sockets: EnvironmentSocketService, local_root: PathBuf) -> Self {
        Self { agents, sockets, local_root, vessels: Mutex::default(), retired: Mutex::default() }
    }

    pub(super) async fn host_refusals(&self, profile: &SshHostProfile) -> Vec<HostRefusal> {
        self.agents.host_refusals(profile).await
    }

    /// The vessel, prepared on first use. Every later use checks the agent
    /// first: one killed between commands is bootstrapped again here.
    ///
    /// The work runs in its own task, so a caller that stops waiting, such as
    /// an attach request past its deadline (#4054), does not cut off the
    /// agent's connect or upgrade partway. The prepared vessel still lands in
    /// its slot for the next caller.
    ///
    /// Nothing owns the task but its slot. It is bounded by the connector's
    /// own timeouts (the agent connect's 60 s, SSH's 30 s). A [`Self::retire`]
    /// meanwhile marks the vessel retired, then waits on the slot lock and
    /// closes whatever the preparation stored, so nothing outlives teardown.
    pub(super) async fn access(
        self: &Arc<Self>,
        preparer: Arc<dyn VesselPreparer>,
        profile: &SshHostProfile,
        vessel: &str,
    ) -> Result<Arc<VesselAccess>, String> {
        let (vessels, profile, name) = (Arc::clone(self), profile.clone(), vessel.to_string());
        tokio::spawn(async move { vessels.access_here(&preparer.preparation(), &profile, &name).await }).await.map_err(|error| {
            if error.is_panic() {
                error!(%vessel, %error, "vessel preparation panicked");
            }
            format!("vessel {vessel} preparation stopped: {error}")
        })?
    }

    async fn access_here(
        &self,
        preparation: &VesselPreparation<'_>,
        profile: &SshHostProfile,
        vessel: &str,
    ) -> Result<Arc<VesselAccess>, String> {
        let key = VesselKey::new(&profile.environment_id, vessel);
        let slot = {
            let mut vessels = self.vessels.lock().await;
            if self.retired.lock().await.contains(&key) {
                return Err(format!("vessel {vessel} on {} has been torn down", profile.environment_id));
            }
            vessels.entry(key.clone()).or_default().clone()
        };
        let mut prepared = slot.prepared.lock().await;
        // Retirement may have taken the slot while this call waited for it.
        if self.retired.lock().await.contains(&key) {
            return Err(format!("vessel {vessel} on {} has been torn down", profile.environment_id));
        }
        if let Some(access) = prepared.as_ref() {
            access.handle.agent().ready().await.map_err(|error| format!("vessel agent for {vessel}: {error}"))?;
            return Ok(access.clone());
        }
        let access = Arc::new(self.prepare(preparation, profile, key).await?);
        *prepared = Some(access.clone());
        Ok(access)
    }

    async fn prepare(&self, preparation: &VesselPreparation<'_>, profile: &SshHostProfile, key: VesselKey) -> Result<VesselAccess, String> {
        let paths = VesselPaths::new(&profile.env_bag, &key)?;
        let local = self.local_directory(&key)?;
        let platform = self.agents.observe_platform(profile).await?;
        let context = LayerContext { prefix: paths.state.clone(), platform: platform.drydock() };
        let tools = preparation.tools.prepare(SSH_HOST_PROVIDER_KIND, &key.environment, &context).await?;
        let handle = self.agents.open(profile, &key, &platform, &paths, &local).await?;
        // A failed preparation closes its handle at once: its listeners and
        // connection never linger beside the next attempt's.
        match self.prepare_opened(preparation, profile, &key, &paths, &handle, &tools).await {
            Ok(registry) => {
                info!(environment = %key.environment, vessel = %key.vessel, agent = %paths.agent.display(), "vessel agent ready");
                Ok(VesselAccess { key, paths, handle, registry })
            }
            Err(error) => {
                handle.close().await;
                Err(error)
            }
        }
    }

    async fn prepare_opened(
        &self,
        preparation: &VesselPreparation<'_>,
        profile: &SshHostProfile,
        key: &VesselKey,
        paths: &VesselPaths,
        handle: &Arc<dyn VesselHandle>,
        tools: &[flotilla_environment::environment::EnvironmentTool],
    ) -> Result<Arc<ProviderRegistry>, String> {
        let agent = handle.agent();
        agent.ready().await.map_err(|error| format!("vessel agent for {}: {error}", key.vessel))?;
        let runner = handle.runner();
        let marker = serde_json::to_string(key).map_err(|error| error.to_string())?;
        for directory in [&paths.agent, &paths.state] {
            runner.write_file(&directory.join(MARKER), &marker).await.map_err(|error| format!("mark vessel directory: {error}"))?;
        }
        let cleat = paths.cleat_runtime();
        runner
            .run("mkdir", &["-p", &cleat.to_string_lossy()], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("create vessel cleat state: {error}"))?;
        agent.push_tools(tools).await.map_err(|error| format!("push vessel tools: {error}"))?;
        let environment = vessel_environment(profile, paths)?;
        handle.configure_environment(environment.clone()).await.map_err(|error| format!("configure vessel environment: {error}"))?;
        let socket = self.sockets.prepare_vessel_forwarded(profile.environment_id.clone(), &key.vessel).await?;
        handle.forward_daemon(&paths.daemon_socket(), socket.as_path()).await.map_err(|error| format!("forward daemon socket: {error}"))?;
        let discovery = ConfigStore::with_base(preparation.config.base_path().as_path().join(format!(
            "env-discovery/{}/{}",
            key.environment,
            key.directory()
        )));
        self.agents.probe(preparation.discovery, &discovery, profile, handle.as_ref(), &environment).await
    }

    /// Owner-only, as drydock requires of attach forward listeners.
    fn local_directory(&self, key: &VesselKey) -> Result<PathBuf, String> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let directory = self.local_root.join(key.directory());
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|error| format!("create {}: {error}", directory.display()))?;
        for path in [&self.local_root, &directory] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("protect {}: {error}", path.display()))?;
        }
        Ok(directory)
    }

    /// Remove a vessel: its forwards, its agent and its directories. An agent
    /// that is stopped or unreachable is logged and does not block teardown;
    /// a later [`SshHostVessels::retire_absent`] retries the host cleanup.
    pub(super) async fn retire(&self, profile: &SshHostProfile, vessel: &str) {
        let key = VesselKey::new(&profile.environment_id, vessel);
        let slot = {
            let mut vessels = self.vessels.lock().await;
            self.retired.lock().await.insert(key.clone());
            vessels.remove(&key)
        };
        if let Some(slot) = slot {
            if let Some(access) = slot.prepared.lock().await.take() {
                access.handle.close().await;
            }
        }
        match VesselPaths::new(&profile.env_bag, &key) {
            Ok(paths) => {
                let store = SharedStore::new(paths.store.clone());
                if let Err(error) =
                    retire_host_agent(profile.runner.as_ref(), &paths.agent, std::slice::from_ref(&paths.state), Some(&store)).await
                {
                    warn!(environment = %key.environment, %vessel, %error, "vessel agent cleanup failed; continuing teardown");
                }
            }
            Err(error) => warn!(environment = %key.environment, %vessel, %error, "vessel paths unavailable; continuing teardown"),
        }
        if let Err(error) = self.sockets.remove_vessel(&profile.environment_id, vessel).await {
            warn!(environment = %key.environment, %vessel, %error, "vessel daemon socket removal failed");
        }
        let _ = std::fs::remove_dir_all(self.local_root.join(key.directory()));
        info!(environment = %key.environment, %vessel, "vessel agent retired");
    }

    /// Retire every vessel of `profile`'s host that is not in `live`: those
    /// this controller prepared, and those it finds marked on the host, which
    /// a restarted controller never prepared. Vessels live again are admitted.
    pub(super) async fn retire_absent(&self, profile: &SshHostProfile, live: &BTreeSet<String>) -> Result<(), String> {
        let environment = profile.environment_id.to_string();
        self.retired.lock().await.retain(|key| key.environment != environment || !live.contains(&key.vessel));
        let known = self
            .vessels
            .lock()
            .await
            .keys()
            .filter(|key| key.environment == environment && !live.contains(&key.vessel))
            .map(|key| key.vessel.clone())
            .collect::<BTreeSet<_>>();
        // Vessels this controller prepared are retired even when the host's
        // markers cannot be listed; the next pass retries the listing.
        for vessel in &known {
            self.retire(profile, vessel).await;
        }
        let marked = self
            .marked(profile)
            .await?
            .into_iter()
            .filter(|key| key.environment == environment && !live.contains(&key.vessel) && !known.contains(&key.vessel))
            .map(|key| key.vessel)
            .collect::<BTreeSet<_>>();
        for vessel in marked {
            self.retire(profile, &vessel).await;
        }
        Ok(())
    }

    /// Environment teardown: retire every vessel this controller knows on it.
    pub(super) async fn retire_environment(&self, profile: &SshHostProfile) {
        let environment = profile.environment_id.to_string();
        let known =
            self.vessels.lock().await.keys().filter(|key| key.environment == environment).map(|key| key.vessel.clone()).collect::<Vec<_>>();
        for vessel in known {
            self.retire(profile, &vessel).await;
        }
    }

    /// Vessels with a marker under the host's vessel roots.
    ///
    /// The sweep assumes this controller is the host's one owning daemon, as
    /// its Host's `SshHost { owning_daemon }` connection declares: a
    /// marker for this environment whose vessel this controller's store does
    /// not hold is retired. Two daemons configured with the same SSH
    /// host would retire each other's vessels; registering one host from two
    /// daemons is already unsupported.
    async fn marked(&self, profile: &SshHostProfile) -> Result<Vec<VesselKey>, String> {
        let mut roots = Vec::new();
        for root in [agent_root(&profile.env_bag), state_root(&profile.env_bag)].into_iter().flatten() {
            roots.push(root.to_string_lossy().into_owned());
        }
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["-c", LIST_MARKERS, "flotilla-vessel-markers"];
        args.extend(roots.iter().map(String::as_str));
        let output = profile
            .runner
            .run("sh", &args, Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("list vessel markers on {}: {error}", profile.destination))?;
        let mut keys = output.lines().filter_map(|line| serde_json::from_str::<VesselKey>(line).ok()).collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

/// The vessels that still need their agents: every Vessel in the store, and
/// every vessel a session on `environment` names, since a session is torn
/// down through its vessel's agent. Replicas count: a convoy homed on another
/// daemon may place its vessel on this daemon's host.
pub(super) async fn live_vessels(backend: &ResourceBackend, namespace: &str, environment: &str) -> Result<BTreeSet<String>, String> {
    let vessels = backend.including_replicas::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?;
    let mut live = vessels.items.into_iter().map(|vessel| vessel.object.metadata.name).collect::<BTreeSet<_>>();
    let sessions = backend.including_replicas::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?;
    live.extend(
        sessions
            .items
            .iter()
            .filter(|session| session.object.spec.env_ref == environment)
            .filter_map(|session| TerminalSessionTarget::from(&session.object).vessel_ref.map(str::to_string)),
    );
    Ok(live)
}

/// Core's way into sessions on SSH hosts: their files and attach go
/// through the vessel's agent.
pub(super) struct VesselSessions {
    pub(super) state: std::sync::Weak<super::state::ControllerRuntimeState>,
}

impl VesselSessions {
    async fn access(&self, spec: TerminalSessionTarget<'_>) -> Option<Result<Arc<VesselAccess>, String>> {
        let state = self.state.upgrade()?;
        let profile = state.ssh_host.get(&spec.env_ref)?;
        let Some(vessel) = spec.vessel_ref else {
            return Some(Err(format!("SSH host {} requires a session vessel identity", spec.env_ref)));
        };
        Some(state.vessel_access(profile, vessel).await)
    }
}

#[async_trait]
impl flotilla_core::in_process::SshHostVesselAccess for VesselSessions {
    async fn runner(&self, spec: TerminalSessionTarget<'_>) -> Option<Result<Arc<dyn CommandRunner>, String>> {
        Some(self.access(spec).await?.map(|access| access.runner()))
    }

    async fn attach(
        &self,
        spec: TerminalSessionTarget<'_>,
        session_id: &str,
        seat: flotilla_protocol::commands::AttachMode,
    ) -> Option<Result<(PathBuf, String), String>> {
        let access = match self.access(spec).await? {
            Ok(access) => access,
            Err(error) => return Some(Err(error)),
        };
        Some(
            async {
                let pool = access
                    .registry
                    .terminal_pools
                    .get(&spec.pool)
                    .map(|(_, pool)| Arc::clone(pool))
                    .ok_or_else(|| format!("terminal pool {} unavailable in vessel {}", spec.pool, access.key.vessel))?;
                match pool.session_liveness(session_id).await? {
                    flotilla_terminal::TerminalSessionLiveness::Running => {}
                    flotilla_terminal::TerminalSessionLiveness::Lost(reason) => {
                        return Err(format!("session {session_id} is unreachable: {reason}"))
                    }
                    _ => return Err(format!("session {session_id} no longer exists in vessel {}", access.key.vessel)),
                }
                pool.preflight_attach(seat).await?;
                let endpoint = pool.cleat_endpoint(session_id).await?.ok_or("vessel attach requires a cleat socket endpoint")?;
                let guest = Path::new(&endpoint.runtime_root).join(&endpoint.daemon).join("socket");
                let socket = access.handle.agent().forward_socket(&guest).await?;
                Ok((socket, endpoint.session))
            }
            .await,
        )
    }
}

/// Print each vessel marker under the given roots, one per line.
const LIST_MARKERS: &str = r#"for root in "$@"; do
  for marker in "$root"/*/vessel.json; do
    [ -f "$marker" ] || continue
    cat -- "$marker"
    printf '\n'
  done
done"#;

/// The layer is published under the vessel's state directory.
struct LayerContext {
    platform: flotilla_environment::platform::Platform,
    prefix: PathBuf,
}

#[async_trait]
impl EnvironmentToolContext for LayerContext {
    fn guest_platform(&self) -> flotilla_environment::platform::Platform {
        self.platform.clone()
    }
    async fn rust_build_jobs(&self) -> Result<usize, String> {
        Err("vessel layers carry no build limits".into())
    }
    fn tools_prefix(&self) -> Option<&Path> {
        Some(&self.prefix)
    }
}

/// The vessel's environment, pushed to its agent: the login identity, the
/// prepared base layer and flotilla's own variables. Crews reach the daemon
/// through the vessel's forward.
pub(super) fn vessel_environment(profile: &SshHostProfile, paths: &VesselPaths) -> Result<BTreeMap<String, String>, String> {
    let bag = &profile.env_bag;
    let base = vessel_base(profile, paths)?;
    let mut environment = IDENTITY_VARIABLES
        .iter()
        .filter_map(|name| bag.find_env_var(name).map(|value| (name.to_string(), value.to_string())))
        .collect::<BTreeMap<_, _>>();
    let flotilla = [
        (DAEMON_SOCKET_ENV.to_string(), paths.daemon_socket().to_string_lossy().into_owned()),
        ("FLOTILLA_ENVIRONMENT_ID".to_string(), profile.environment_id.to_string()),
        (CONTAINED_DAEMON_REQUIRED_ENV.to_string(), "1".to_string()),
        ("CLEAT_RUNTIME_DIR".to_string(), paths.cleat_runtime().to_string_lossy().into_owned()),
    ]
    .into_iter()
    .chain(paths.generation_variables())
    .collect::<Vec<_>>();
    if let Some((name, _)) = flotilla.iter().find(|(name, _)| base.sets(name)) {
        return Err(format!(
            "host configuration error for {}: supply `{}` sets {name}, which flotilla sets",
            profile.environment_id,
            base.supply_of(name).unwrap_or_default()
        ));
    }
    environment.extend(base.environment());
    environment.extend(flotilla);
    Ok(environment)
}

/// The vessel's base layer: its generation layer, then the host's.
pub(super) fn vessel_base(profile: &SshHostProfile, paths: &VesselPaths) -> Result<BaseLayer, String> {
    profile.preparer(paths.generation_path()).base().map_err(|error| error.for_environment(profile.environment_id.as_str()))
}

/// Point a session at its vessel's daemon forward. The CLI never spawns a
/// daemon of its own there: a missing socket means the forward is down.
pub(super) fn apply_reach_environment(env: &mut Vec<(String, String)>, environment: &str, paths: &VesselPaths) {
    env.retain(|(name, _)| !matches!(name.as_str(), "FLOTILLA_ENVIRONMENT_ID" | DAEMON_SOCKET_ENV | CONTAINED_DAEMON_REQUIRED_ENV));
    env.extend([
        (DAEMON_SOCKET_ENV.to_string(), paths.daemon_socket().to_string_lossy().into_owned()),
        ("FLOTILLA_ENVIRONMENT_ID".to_string(), environment.to_string()),
        (CONTAINED_DAEMON_REQUIRED_ENV.to_string(), "1".to_string()),
    ]);
}

/// The last step before an SSH host session starts: the vessel's prepared
/// context rendered onto the launch, with `bound` the crew's account
/// material. Nothing after it adds to `PATH` or sets a base variable, and the
/// session runs with exactly this environment: no login profile (#4173).
pub(super) fn compose_session_environment(
    env: &mut Vec<(String, String)>,
    paths: &VesselPaths,
    profile: &SshHostProfile,
    bound: BoundLayer,
) -> Result<(), String> {
    let environment = &profile.environment_id;
    let prepared =
        profile.preparer(paths.generation_path()).prepare(Some(bound)).map_err(|error| error.for_environment(environment.as_str()))?;
    prepared.render_crew(env, &profile.accounts).map_err(|error| error.for_environment(environment.as_str()))
}

#[cfg(test)]
#[path = "ssh_host_vessels/tests.rs"]
mod tests;
