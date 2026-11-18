//! Composition-root switch for the dummy fleet (#3206). A daemon whose
//! `daemon.toml` has a `[dummy_fleet]` section installs the dummy providers
//! into its discovery factories before the daemon is built, then attaches the
//! fleet's crews to that daemon. Without the section neither step does anything.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_config::{dummy_fleet::DummyFleetConfig, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_dummy_fleet::{CrewCommandPort, DaemonCommandPort, DummyFleet};
use flotilla_protocol::{commands::CommandValue, CommandAction};
use flotilla_provisioning::reconcilers::ForgeDefaultBranchResolver;
use flotilla_resources::TerminalAttentionState;
use flotilla_store::load_metrics::inherit_or;
use tracing::warn;

/// Install the configured dummy fleet's factories, if any: the fleet whose state file is in
/// `state_dir`, holding what it held when the previous daemon process ended.
pub fn install_dummy_fleet(
    config: &DaemonConfig,
    state_dir: &Path,
    discovery: &mut DiscoveryRuntime,
) -> Result<Option<Arc<DummyFleet>>, String> {
    let Some(config) = config.dummy_fleet.clone() else { return Ok(None) };
    warn!("dummy fleet enabled: dummy environments, crews, checkouts and change requests are installed for load testing");
    // The terminal runtime confirms a delivery after continuous work for this long.
    let confirmation = crate::runtime::DELIVERY_CONFIRMATION_STABLE_FOR + crate::runtime::DELIVERY_CONFIRMATION_POLL;
    if u128::from(config.agent.turn_delay_ms) < confirmation.as_millis() {
        warn!(
            turn_delay_ms = config.agent.turn_delay_ms,
            "dummy crews idle before the terminal runtime confirms a delivery; their turns will be held as unconfirmed"
        );
    }
    let fleet = DummyFleet::open(config, state_dir)?;
    fleet.install(discovery);
    Ok(Some(fleet))
}

/// Connect the fleet's crews to the daemon whose discovery installed it:
/// their crew commands through its command path, and their attention reports
/// through the same recording as harness hook events. Crews a previous process
/// hosted resume.
pub fn attach_dummy_fleet(fleet: &Arc<DummyFleet>, daemon: &Arc<InProcessDaemon>) -> Result<(), String> {
    fleet.attach(daemon_crew_port(daemon))
}

/// The fleet as part of the daemon's process: a graceful stop stops its crews and writes out its
/// state file before the runtime's tasks end.
pub fn dummy_fleet_component(fleet: &Arc<DummyFleet>) -> Arc<dyn crate::runtime::ProcessComponent> {
    Arc::new(FleetComponent(Arc::clone(fleet)))
}

struct FleetComponent(Arc<DummyFleet>);

#[async_trait]
impl crate::runtime::ProcessComponent for FleetComponent {
    async fn stop(&self) {
        self.0.shutdown().await;
    }
}

/// The port [`attach_dummy_fleet`] attaches: `daemon`'s command path and its
/// hook-attention recording. A harness that outlives one daemon, as a crew's
/// process outlives a daemon restart, forwards to the current daemon's port.
pub fn daemon_crew_port(daemon: &Arc<InProcessDaemon>) -> Arc<dyn CrewCommandPort> {
    let handle = Arc::clone(daemon) as Arc<dyn DaemonHandle>;
    Arc::new(DaemonCrewPort { commands: DaemonCommandPort::new(&handle), daemon: Arc::downgrade(daemon) })
}

struct DaemonCrewPort {
    commands: Arc<DaemonCommandPort>,
    // Weak: the daemon holds its fleet, which holds this port.
    daemon: std::sync::Weak<InProcessDaemon>,
}

#[async_trait]
impl CrewCommandPort for DaemonCrewPort {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        self.commands.execute(action).await
    }

    async fn report_attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
        // Its reads count as a harness hook's do.
        inherit_or(crate::agent_attention::AGENT_HOOKS, self.attention(namespace, session, working)).await
    }
}

impl DaemonCrewPort {
    async fn attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
        use flotilla_state_machines::machines::agent_session::Activity;
        let (state, activity, cause) = if working {
            (TerminalAttentionState::Working, Activity::Working, DUMMY_WORKING_CAUSE)
        } else {
            (TerminalAttentionState::Idle, Activity::Idle, DUMMY_IDLE_CAUSE)
        };
        if let Some(daemon) = self.daemon.upgrade() {
            daemon.crew_progress().hook(session, daemon.clock().now());
            if let Some(primitives) = daemon.message_primitives() {
                if let Err(error) =
                    crate::agent_attention::record_agent_activity(daemon.lifecycle_reads(), &primitives, session, activity, cause).await
                {
                    // As for a harness hook: the AgentSession is an additional observer.
                    warn!(%session, %error, "failed to record dummy crew activity");
                }
            }
        }
        let _ = namespace;
        let daemon = self.daemon.upgrade().ok_or("the daemon has stopped")?;
        let reads = daemon.lifecycle_reads();
        crate::agent_attention::observe_hook_attention(reads.terminals()?, session, state, false)
    }
}

/// The causes a dummy crew's reports carry on its AgentSession, in place of a harness's native
/// hook event names.
const DUMMY_WORKING_CAUSE: &str = "dummy-working";
const DUMMY_IDLE_CAUSE: &str = "dummy-idle";

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
        let state = tempfile::tempdir().expect("state directory");
        assert!(install_dummy_fleet(&DaemonConfig::default(), state.path(), &mut discovery).expect("install").is_none());
        assert!(dummy_implementations(&discovery.factories).is_empty(), "no section installs nothing");
        assert_eq!(discovery.factories.agent_adapters.len(), adapters);

        let config = DaemonConfig { dummy_fleet: Some(DummyFleetConfig::default()), ..DaemonConfig::default() };
        assert!(install_dummy_fleet(&config, state.path(), &mut discovery).expect("install").is_some());
        assert_eq!(dummy_implementations(&discovery.factories).len(), 4, "VCS, change requests, terminal pool, environment");
        assert_eq!(discovery.factories.agent_adapters.len(), adapters + 1);
    }
}
