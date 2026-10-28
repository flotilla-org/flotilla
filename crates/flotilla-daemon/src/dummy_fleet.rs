//! Composition-root switch for the dummy fleet (#3206). A daemon whose
//! `daemon.toml` has a `[dummy_fleet]` section installs the dummy providers
//! into its discovery factories before the daemon is built, then attaches the
//! fleet's crews to that daemon. Without the section neither step does anything.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_config::{dummy_fleet::DummyFleetConfig, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_dummy_fleet::{CrewCommandPort, DaemonCommandPort, DummyFleet};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_provisioning::reconcilers::ForgeDefaultBranchResolver;
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
    fleet.install(discovery);
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

#[cfg(test)]
mod tests {
    use flotilla_config::{dummy_fleet::DummyFleetConfig, DaemonConfig};
    use flotilla_discovery::{DiscoveryRuntime, FactoryRegistry};

    use super::install_dummy_fleet;

    /// Every factory's implementation name that is a dummy, by category.
    fn dummy_implementations(factories: &FactoryRegistry) -> Vec<String> {
        let mut names = Vec::new();
        names.extend(factories.vcs.iter().map(|factory| factory.descriptor().implementation));
        names.extend(factories.change_requests.iter().map(|factory| factory.descriptor().implementation));
        names.extend(factories.terminal_pools.iter().map(|factory| factory.descriptor().implementation));
        names.extend(factories.environment_providers.iter().map(|factory| factory.descriptor().implementation));
        names.retain(|name| name == "dummy");
        names
    }

    // Behaviour (#3206): the dummy providers exist only in a daemon whose
    // `daemon.toml` has a `[dummy_fleet]` section. Without it the default
    // factories are untouched, so default discovery can never select one.
    #[test]
    fn dummy_providers_are_installed_only_with_the_config_section() {
        let mut discovery = DiscoveryRuntime::for_process();
        assert!(dummy_implementations(&discovery.factories).is_empty(), "default factories hold no dummies");
        let adapters = discovery.factories.agent_adapters.len();
        assert!(install_dummy_fleet(&DaemonConfig::default(), &mut discovery).is_none());
        assert!(dummy_implementations(&discovery.factories).is_empty(), "no section installs nothing");
        assert_eq!(discovery.factories.agent_adapters.len(), adapters);

        let config = DaemonConfig { dummy_fleet: Some(DummyFleetConfig::default()), ..DaemonConfig::default() };
        assert!(install_dummy_fleet(&config, &mut discovery).is_some());
        assert_eq!(dummy_implementations(&discovery.factories).len(), 4, "VCS, change requests, terminal pool, environment");
        assert_eq!(discovery.factories.agent_adapters.len(), adapters + 1);
    }
}
