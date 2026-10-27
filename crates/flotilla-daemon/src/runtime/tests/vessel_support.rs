//! #3792 test doubles for agentless vessels: a vessel agent that runs its
//! commands through the host runner it is given, as the real agent runs them
//! on the host, and records what the controller configured.

use super::*;

use super::agentless_vessels::{AgentlessVessels, HostRefusal, VesselAgents, VesselHandle, VesselPaths};
use crate::server::environment_sockets::{EnvironmentSocketRegistry, EnvironmentSocketService};
use flotilla_discovery::DiscoveryRuntime;
use flotilla_environment::environment::{vessel_ops::RunnerVesselAgent, EntryKind, EnvironmentTool, VesselAgent};

/// What the controller did to one vessel's agent.
#[derive(Clone, Debug, Default)]
pub(super) struct RecordedVessel {
    pub(super) environment: BTreeMap<String, String>,
    /// `(guest, host)` daemon forwards.
    pub(super) forwards: Vec<(PathBuf, PathBuf)>,
    pub(super) tool_pushes: usize,
    pub(super) attach_forwards: Vec<PathBuf>,
    /// Artifacts made store entries, by digest, with how their bytes arrived.
    pub(super) artifacts: Vec<(String, ArtifactArrival)>,
    pub(super) closed: bool,
}

/// How an artifact's bytes reached the vessel's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ArtifactArrival {
    /// The vessel fetched them from these upstream URLs.
    Fetched(Vec<String>),
    Pushed,
}

/// Where the fake agent of the vessel at `agent` keeps an entry, as drydock keeps its
/// store beside the agent.
pub(super) fn store_entry(agent: &Path, sha256: &str) -> PathBuf {
    agent.join("store/artifacts").join(sha256)
}

/// Opens every vessel onto `runner`, keyed by its agent directory, and gives
/// each the same provider `registry`.
pub(super) struct RunnerVesselAgents {
    runner: Arc<dyn CommandRunner>,
    registry: Arc<ProviderRegistry>,
    pub(super) vessels: Arc<StdMutex<BTreeMap<PathBuf, RecordedVessel>>>,
    pub(super) refusals: StdMutex<Vec<HostRefusal>>,
}

impl RunnerVesselAgents {
    pub(super) fn new(runner: Arc<dyn CommandRunner>, registry: Arc<ProviderRegistry>) -> Arc<Self> {
        Arc::new(Self { runner, registry, vessels: Arc::default(), refusals: StdMutex::default() })
    }

    pub(super) fn recorded(&self, paths: &VesselPaths) -> RecordedVessel {
        self.vessels.lock().expect("vessels").get(&paths.agent).cloned().unwrap_or_default()
    }
}

struct RunnerVessel {
    agent: PathBuf,
    runner: Arc<dyn CommandRunner>,
    vessels: Arc<StdMutex<BTreeMap<PathBuf, RecordedVessel>>>,
}

impl RunnerVessel {
    fn record(&self, change: impl FnOnce(&mut RecordedVessel)) {
        change(self.vessels.lock().expect("vessels").entry(self.agent.clone()).or_default());
    }
}

#[async_trait]
impl VesselAgent for RunnerVessel {
    fn commands(&self) -> Arc<dyn CommandRunner> {
        self.runner.clone()
    }
    // The vessel's file facilities run on the host runner, as the real agent runs them there.
    async fn writable_base(&self) -> Result<PathBuf, String> {
        RunnerVesselAgent(self.runner.clone()).writable_base().await
    }
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
        RunnerVesselAgent(self.runner.clone()).stat(path).await
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        RunnerVesselAgent(self.runner.clone()).create_dir(path, mode).await
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        RunnerVesselAgent(self.runner.clone()).read(path, limit).await
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        RunnerVesselAgent(self.runner.clone()).write(path, bytes, mode).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        RunnerVesselAgent(self.runner.clone()).symlink(path, target).await
    }
    // This process-boundary fake models a Linux amd64 host whose agent store records what
    // arrived, never reaching the network.
    async fn architecture(&self) -> Result<String, String> {
        Ok("amd64".into())
    }
    async fn fetch_artifact(&self, sha256: &str, urls: &[String], _: flotilla_resources::PinUnpack) -> Result<(), String> {
        self.record(|vessel| vessel.artifacts.push((sha256.into(), ArtifactArrival::Fetched(urls.to_vec()))));
        Ok(())
    }
    async fn push_artifact(&self, sha256: &str, _: &Path, _: flotilla_resources::PinUnpack) -> Result<(), String> {
        self.record(|vessel| vessel.artifacts.push((sha256.into(), ArtifactArrival::Pushed)));
        Ok(())
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        let host = RunnerVesselAgent(self.runner.clone());
        host.create_dir(path.parent().ok_or("a link needs a directory")?, 0o755).await?;
        let entry = store_entry(&self.agent, sha256);
        host.symlink(path, &if member.as_os_str().is_empty() { entry } else { entry.join(member) }).await
    }
    async fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    async fn push_tools(&self, _tools: &[EnvironmentTool]) -> Result<(), String> {
        self.record(|vessel| vessel.tool_pushes += 1);
        Ok(())
    }
    async fn forward_socket(&self, guest: &Path) -> Result<PathBuf, String> {
        self.record(|vessel| vessel.attach_forwards.push(guest.into()));
        Ok(self.agent.join("attach.sock"))
    }
}

#[async_trait]
impl VesselHandle for RunnerVessel {
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }
    fn agent(&self) -> Arc<dyn VesselAgent> {
        Arc::new(RunnerVessel { agent: self.agent.clone(), runner: Arc::clone(&self.runner), vessels: Arc::clone(&self.vessels) })
    }
    async fn configure_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        self.record(|vessel| vessel.environment = environment);
        Ok(())
    }
    async fn forward_daemon(&self, guest: &Path, host: &Path) -> Result<(), String> {
        self.record(|vessel| vessel.forwards.push((guest.into(), host.into())));
        Ok(())
    }
    async fn close(&self) {
        self.record(|vessel| vessel.closed = true);
    }
}

#[async_trait]
impl VesselAgents for RunnerVesselAgents {
    // This process-boundary fake models a Linux amd64 host without invoking uname.
    async fn observe_platform(&self, _: &AgentlessSshProfile) -> Result<super::agentless_vessels::HostPlatform, String> {
        Ok(super::agentless_vessels::HostPlatform::from_uname("Linux", "x86_64"))
    }
    async fn host_refusals(&self, _profile: &AgentlessSshProfile) -> Vec<HostRefusal> {
        self.refusals.lock().expect("refusals").clone()
    }
    async fn open(
        &self,
        _profile: &AgentlessSshProfile,
        _: &super::agentless_vessels::HostPlatform,
        paths: &VesselPaths,
        _local: &Path,
    ) -> Result<Arc<dyn VesselHandle>, String> {
        Ok(Arc::new(RunnerVessel { agent: paths.agent.clone(), runner: Arc::clone(&self.runner), vessels: Arc::clone(&self.vessels) }))
    }
    async fn probe(
        &self,
        _discovery: &DiscoveryRuntime,
        _config: &ConfigStore,
        _profile: &AgentlessSshProfile,
        _handle: &dyn VesselHandle,
        _environment: &BTreeMap<String, String>,
    ) -> Result<Arc<ProviderRegistry>, String> {
        Ok(Arc::clone(&self.registry))
    }
}

/// A real environment-socket service whose accept loop holds connections open.
pub(super) fn held_socket_service(state: &Path) -> EnvironmentSocketService {
    EnvironmentSocketService::new(
        Arc::new(tokio::sync::Mutex::new(EnvironmentSocketRegistry::new())),
        DaemonHostPath::new(state.join("env")),
        |listener, _, _| {
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await
            })
        },
    )
}

/// The vessel manager over `agents`, with sockets and attach directories in `state`.
pub(super) fn vessels_over(agents: Arc<dyn VesselAgents>, state: &Path) -> Arc<AgentlessVessels> {
    Arc::new(AgentlessVessels::new(agents, held_socket_service(state), state.join("vessels")))
}
