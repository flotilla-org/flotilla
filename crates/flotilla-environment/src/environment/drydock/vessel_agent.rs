//! The vessel agent of a drydock vessel: the agent's own facilities, with its
//! sockets carried as exposures drydock keeps (drydock#132 cases 1 and 2)
//! instead of forwards this process drives.
use super::*;
use std::net::SocketAddr;

pub(super) struct DrydockVesselAgent {
    pub runner: Arc<AgentRunner>,
    pub exposures: exposures::Exposures,
}

#[async_trait]
impl VesselAgent for DrydockVesselAgent {
    async fn ready(&self) -> Result<(), String> {
        VesselAgent::ready(&self.runner).await
    }
    /// Cleat's socket at `guest`, reached through an inward exposure's local socket.
    async fn forward_socket(&self, guest: &Path) -> Result<PathBuf, String> {
        if !guest.is_absolute() {
            return Err("guest socket path must be absolute".into());
        }
        let target = wire::InwardTarget::Unix(exposures::unix(guest));
        self.exposures.ready_inward(&exposures::path_name("cleat", guest), target).await
    }
    async fn push_tools(&self, tools: &[EnvironmentTool]) -> Result<(), String> {
        VesselAgent::push_tools(&self.runner, tools).await
    }
    /// The socket `guest` in the vessel, reaching `controller` through an outward
    /// exposure. Drydock makes no guest TCP listener beside a guest socket.
    async fn forward_controller_socket(&self, guest: &Path, tcp: Option<SocketAddr>, controller: &Path) -> Result<(), String> {
        if let Some(tcp) = tcp {
            return Err(format!("drydock exposes no guest TCP listener {tcp} beside {}", guest.display()));
        }
        let name = exposures::path_name("socket", guest);
        self.exposures.outward(&name, exposures::outward_unix(guest, controller)).await.map(drop)
    }
    async fn extend_environment(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        VesselAgent::extend_environment(&self.runner, environment).await
    }
    fn commands(&self) -> Arc<dyn CommandRunner> {
        self.runner.commands()
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        self.runner.writable_base().await
    }
    async fn home_base(&self) -> Result<PathBuf, String> {
        self.runner.home_base().await
    }
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
        VesselAgent::stat(&self.runner, path).await
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        VesselAgent::create_dir(&self.runner, path, mode).await
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        VesselAgent::read(&self.runner, path, limit).await
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        VesselAgent::write(&self.runner, path, bytes, mode).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        VesselAgent::symlink(&self.runner, path, target).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        VesselAgent::rename(&self.runner, from, to).await
    }
    async fn architecture(&self) -> Result<String, String> {
        VesselAgent::architecture(&self.runner).await
    }
    async fn fetch_artifact(&self, sha256: &str, urls: &[String], unpack: flotilla_resources::PinUnpack) -> Result<(), String> {
        VesselAgent::fetch_artifact(&self.runner, sha256, urls, unpack).await
    }
    async fn push_artifact(&self, sha256: &str, file: &Path, unpack: flotilla_resources::PinUnpack) -> Result<(), String> {
        VesselAgent::push_artifact(&self.runner, sha256, file, unpack).await
    }
    async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
        VesselAgent::link_artifact(&self.runner, path, sha256, member).await
    }
}
