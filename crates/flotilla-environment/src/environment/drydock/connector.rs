//! One vessel's agent connection, through drydock's inward exposure of the
//! agent's socket (drydock#132 case 4). Drydock holds the vessel's link and
//! key; this end only opens the local socket drydock hands back.
use super::*;
use crate::environment::agent::{
    client::{AgentClient, ByteStream},
    AgentConnector,
};
use flotilla_protocol::provider::AgentConnectionReport;

/// What the adapter reports for the agent's latest connection.
pub(super) const EXPOSURE_MODE: &str = "inward_exposure";

pub(super) struct DrydockAgentConnector {
    pub link: Arc<dyn AgentLink>,
    pub exposures: exposures::Exposures,
    pub module: String,
    /// The agent's socket in the vessel.
    pub socket: PathBuf,
    pub timeout: Duration,
    pub platform: wire::Platform,
    /// The agent build for `platform`, read once and shared by the instance's vessels.
    pub executable: Arc<tokio::sync::OnceCell<Vec<u8>>>,
    /// Where the latest connection went.
    pub report: Arc<std::sync::Mutex<Option<AgentConnectionReport>>>,
}

#[async_trait]
impl AgentConnector for DrydockAgentConnector {
    async fn connect(&self) -> Result<Arc<dyn AgentClient>, String> {
        let build = agent_build(&self.executable, &self.platform).await?;
        let (local, error) = self.exposures.inward("agent", wire::InwardTarget::Unix(exposures::unix(&self.socket))).await?;
        // Each attempt is its own connection to the agent's socket.
        let mut open = || {
            let local = local.clone();
            Box::pin(async move { Ok(Box::new(tokio::net::UnixStream::connect(&local).await?) as ByteStream) }) as _
        };
        let connected = self.link.connect(&mut open, &self.platform, build, self.timeout).await.map_err(|cause| match &error {
            Some(error) => format!("agent through drydock: {cause}; exposure {:?}: {}", error.code, error.reason),
            None => format!("agent through drydock: {cause}"),
        })?;
        *self.report.lock().expect("agent report") = Some(AgentConnectionReport {
            provider: self.exposures.instance.clone(),
            module: self.module.clone(),
            vessel: self.exposures.backing.clone(),
            endpoint: local.display().to_string(),
            mode: EXPOSURE_MODE.into(),
        });
        Ok(connected)
    }
}

/// The agent build for `platform`: the installed generation's companion, read
/// once into `cell` unless the instance was given one.
pub(super) async fn agent_build<'a>(cell: &'a tokio::sync::OnceCell<Vec<u8>>, platform: &wire::Platform) -> Result<&'a [u8], String> {
    cell.get_or_try_init(|| async { installed_agent_build(platform).await }).await.map(Vec::as_slice)
}

/// The installed generation's agent companion for `platform`: the one build
/// both drydock and this adapter push, so neither replaces the other's.
pub async fn installed_agent_build(platform: &wire::Platform) -> Result<Vec<u8>, String> {
    let path = crate::guest_tools::GuestTools::installed(platform)?.agent();
    tokio::fs::read(&path).await.map_err(|cause| format!("drydock-agent companion {} unavailable: {cause}", path.display()))
}
