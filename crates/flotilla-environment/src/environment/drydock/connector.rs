//! One vessel's agent connection, as drydock makes it. Every connect and
//! reconnect goes through drydock's entry point, which applies the provider's
//! declared agent capability, its reach and the configured link preference.
use super::*;
use crate::environment::agent::AgentConnector;
use drydock_agent::Client;

pub(super) struct DrydockAgentConnector {
    pub api: Arc<dyn ProviderApi>,
    pub reach: AgentReach,
    pub instance: String,
    pub backing: String,
    pub identity: PathBuf,
    pub platform: wire::Platform,
    /// The agent build for `platform`, read once and shared by the instance's vessels.
    pub executable: Arc<tokio::sync::OnceCell<Vec<u8>>>,
    /// What drydock reported it chose for the latest connection.
    pub report: Arc<std::sync::Mutex<Option<AgentReport>>>,
}

#[async_trait]
impl AgentConnector for DrydockAgentConnector {
    async fn connect(&self) -> Result<Client, String> {
        let build = agent_build(&self.executable, &self.platform).await?;
        let vessel = ::drydock::agent::Vessel {
            provider: &self.instance,
            environment: &self.backing,
            identity: &self.identity,
            platform: &self.platform,
            build,
        };
        let connected = self.reach.connect(&*self.api, &vessel).await.map_err(|cause| cause.to_string())?;
        *self.report.lock().expect("agent report") = Some(connected.report);
        Ok(connected.client)
    }
}

/// The agent build for `platform`: the installed generation's companion, read
/// once into `cell` unless the instance was given one.
pub(super) async fn agent_build<'a>(cell: &'a tokio::sync::OnceCell<Vec<u8>>, platform: &wire::Platform) -> Result<&'a [u8], String> {
    cell.get_or_try_init(|| async {
        let path = crate::guest_tools::GuestTools::installed(platform)?.agent();
        tokio::fs::read(&path).await.map_err(|cause| format!("drydock-agent companion {} unavailable: {cause}", path.display()))
    })
    .await
    .map(Vec::as_slice)
}
