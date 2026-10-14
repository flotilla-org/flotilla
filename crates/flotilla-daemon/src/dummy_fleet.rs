//! Composition-root switch for the dummy fleet (#3206). A daemon whose
//! `daemon.toml` has a `[dummy_fleet]` section installs the dummy providers
//! into its discovery factories before the daemon is built, then attaches the
//! fleet's crews to that daemon. Without the section neither step does anything.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_config::{dummy_fleet::DummyFleetConfig, DaemonConfig};
use flotilla_controllers::reconcilers::ForgeDefaultBranchResolver;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_dummy_fleet::{CrewCommandPort, DaemonCommandPort, DummyFleet};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_resources::TerminalAttentionState;
use flotilla_store::ResourceBackend;
use tracing::warn;

/// Install the configured dummy fleet's factories, if any.
pub fn install_dummy_fleet(config: &DaemonConfig, discovery: &mut DiscoveryRuntime) -> Option<Arc<DummyFleet>> {
    let config = config.dummy_fleet.clone()?;
    warn!("dummy fleet enabled: dummy environments, crews, checkouts and change requests are installed for load testing");
    // The terminal runtime confirms a delivery after continuous work for this long.
    let confirmation = crate::runtime::DELIVERY_CONFIRMATION_STABLE_FOR + crate::runtime::DELIVERY_CONFIRMATION_POLL;
    if u128::from(config.agent.turn_delay_ms) < confirmation.as_millis() {
        warn!(
            turn_delay_ms = config.agent.turn_delay_ms,
            "dummy crews idle before the terminal runtime confirms a delivery; their turns will be held as unconfirmed"
        );
    }
    let fleet = DummyFleet::new(config);
    fleet.install(&mut discovery.factories);
    Some(fleet)
}

/// Connect the fleet's crews to the daemon whose discovery installed it:
/// their crew commands through its command path, and their attention reports
/// through the same recording as harness hook events.
pub fn attach_dummy_fleet(fleet: &DummyFleet, daemon: &Arc<InProcessDaemon>) -> Result<(), String> {
    let handle = Arc::clone(daemon) as Arc<dyn DaemonHandle>;
    fleet.attach(Arc::new(DaemonCrewPort { commands: DaemonCommandPort::new(&handle), backend: daemon.resource_backend() }))
}

struct DaemonCrewPort {
    commands: Arc<DaemonCommandPort>,
    backend: ResourceBackend,
}

#[async_trait]
impl CrewCommandPort for DaemonCrewPort {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        self.commands.execute(action).await
    }

    async fn report_attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
        let state = if working { TerminalAttentionState::Working } else { TerminalAttentionState::Idle };
        crate::agent_attention::observe_hook_attention(&self.backend, namespace, session, state, false).await
    }
}

/// Every repository on the dummy forge has the default branch `main`.
pub(crate) fn dummy_forge_default_branches(config: Option<&DummyFleetConfig>) -> Option<Arc<dyn ForgeDefaultBranchResolver>> {
    let service_url = config?.forge_service_url.trim_end_matches('/').to_string();
    Some(Arc::new(DummyForgeDefaultBranch { service_url }))
}

struct DummyForgeDefaultBranch {
    service_url: String,
}

#[async_trait]
impl ForgeDefaultBranchResolver for DummyForgeDefaultBranch {
    async fn default_branch(&self, forge: &flotilla_resources::ForgeIdentity) -> Result<Option<String>, String> {
        Ok((forge.service_url.trim_end_matches('/') == self.service_url).then(|| "main".to_string()))
    }
}
