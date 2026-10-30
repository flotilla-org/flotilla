//! One vessel's agent connection, through drydock's inward exposure of the
//! agent's socket (drydock#132 case 4). Drydock holds the vessel's link and
//! key; this end only opens the local socket drydock hands back. A vessel whose
//! reach is a relay has no agent socket at the guest path (drydock's `local`
//! module keeps one in each vessel's directory): its host carries the
//! connection, so the agent is opened through drydock's relay route instead,
//! as drydock's own agent client does.
use super::*;
use crate::environment::agent::{connect_current, AgentConnector};
use drydock_agent::Client;
use drydock_transport::ByteStream;
use flotilla_protocol::provider::AgentConnectionReport;

/// What the adapter reports for the agent's latest connection.
pub(super) const EXPOSURE_MODE: &str = "inward_exposure";
/// What the adapter reports for a connection through drydock's relay route.
pub(super) const RELAY_MODE: &str = "relay";

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
    /// Whether the vessel's reach is a relay, asked of drydock once.
    pub relayed: tokio::sync::OnceCell<bool>,
}

#[async_trait]
impl AgentConnector for DrydockAgentConnector {
    async fn connect(&self) -> Result<Client, String> {
        let build = agent_build(&self.executable, &self.platform).await?;
        let exposures = &self.exposures;
        let (connected, endpoint, mode) = if self.relayed().await? {
            // Each attempt is its own relayed connection.
            let connected = connect_current(
                || async move {
                    let stream = exposures.api.relay(&exposures.instance, &exposures.backing).await;
                    Ok(Box::new(stream.map_err(|cause| std::io::Error::other(api_error(cause)))?) as ByteStream)
                },
                &self.platform,
                build,
                self.timeout,
            )
            .await
            .map_err(|cause| format!("agent through drydock relay: {cause}"))?;
            (connected, RELAY_MODE.to_string(), RELAY_MODE)
        } else {
            let (local, error) = exposures.inward("agent", wire::InwardTarget::Unix(exposures::unix(&self.socket))).await?;
            // Each attempt is its own connection to the agent's socket.
            let connected = connect_current(
                || {
                    let local = local.clone();
                    async move { Ok(Box::new(tokio::net::UnixStream::connect(&local).await?) as ByteStream) }
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
            (connected, local.display().to_string(), EXPOSURE_MODE)
        };
        *self.report.lock().expect("agent report") = Some(AgentConnectionReport {
            provider: exposures.instance.clone(),
            module: self.module.clone(),
            vessel: exposures.backing.clone(),
            endpoint,
            mode: mode.into(),
        });
        Ok(connected)
    }
}

impl DrydockAgentConnector {
    /// Whether drydock's host carries this vessel's agent connection itself.
    /// Only the reach's kind is read; its endpoint and any secret are dropped.
    async fn relayed(&self) -> Result<bool, String> {
        let exposures = &self.exposures;
        self.relayed
            .get_or_try_init(|| async {
                let reach = exposures.api.reach(&exposures.instance, &exposures.backing).await.map_err(api_error)?;
                Ok::<_, String>(matches!(reach, wire::Reach::Relay { .. }))
            })
            .await
            .copied()
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
