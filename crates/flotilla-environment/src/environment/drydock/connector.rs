//! One vessel's agent connection, through drydock's inward exposure of the
//! agent's socket (drydock#132 case 4). Drydock holds the vessel's link and
//! key; this end only opens the local socket drydock hands back.
use super::*;
use crate::environment::agent::{connect_current, AgentConnector};
use drydock_agent::Client;
use drydock_transport::{ByteStream, Connections, Limits, Transport};
use flotilla_protocol::provider::AgentConnectionReport;

/// What the adapter reports for the agent's latest connection.
pub(super) const EXPOSURE_MODE: &str = "inward_exposure";

pub(super) struct DrydockAgentConnector {
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
    async fn connect(&self) -> Result<Client, String> {
        let build = agent_build(&self.executable, &self.platform).await?;
        let (local, error) = self.exposures.inward("agent", wire::InwardTarget::Unix(exposures::unix(&self.socket))).await?;
        // Each stream is its own connection to the agent: a multiplexed session
        // would become the agent's preferred link for its own streams, which
        // carry drydock's outward exposures.
        let connected = connect_current(
            || {
                let local = local.clone();
                async move { Transport::native(Arc::new(Streams(local)), Limits::default()) }
            },
            &self.platform,
            build,
            self.timeout,
        )
        .await
        .map_err(|cause| match &error {
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

/// Opens each stream to the agent as a connection to the exposure's local
/// socket. The agent never opens one this way, so nothing is accepted.
struct Streams(PathBuf);
#[async_trait]
impl Connections for Streams {
    async fn connect(&self) -> std::io::Result<ByteStream> {
        Ok(Box::new(tokio::net::UnixStream::connect(&self.0).await?))
    }
    async fn accept(&self) -> std::io::Result<ByteStream> {
        std::future::pending().await
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
