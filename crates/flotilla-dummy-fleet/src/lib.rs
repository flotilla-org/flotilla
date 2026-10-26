//! Dummy fleet (#3206): fake crews, checkouts and change requests that run
//! through the normal provider seams, so the control plane can be driven to
//! `WORKLOAD.md` scale without containers, agents or forges.
//!
//! The composition root installs a [`DummyFleet`] into its discovery factories
//! when the daemon's `[dummy_fleet]` settings are present, and attaches it to
//! the daemon it built. Nothing in the controllers knows about it:
//!
//! - the dummy environment provider instance (in `flotilla-environment`)
//!   realises Docker-kind environments selected by a PlacementPolicy whose
//!   provider-instance label is `dummy`, and marks them as dummy environments;
//! - inside a dummy environment, discovery finds the dummy terminal pool, the
//!   dummy agent adapter and the dummy VCS, and no real tools;
//! - the dummy change-request tracker claims repositories on the dummy forge;
//! - with `[dummy_fleet.shared_forge]`, coders instead open change requests on
//!   a Forgejo forge every rehearsal host shares (#3374), which the ordinary
//!   Forgejo provider observes.

mod adapter;
pub mod catalog;
mod crew;
mod fleet;
mod forge;
mod shared_forge;
mod terminal;
mod vcs;

use std::{sync::Arc, time::Duration};

use flotilla_discovery::FactoryRegistry;
use flotilla_environment::{
    environment::dummy::{DummyEnvironmentBehaviour, DummyEnvironmentProvider},
    factories::dummy::DummyEnvironmentFactory,
};

pub use adapter::{DummyAgentAdapter, DUMMY_AGENT_ADAPTER, DUMMY_SCRIPT_ENV};
pub use crew::{CrewScript, LAST_TURN_MARKER, SCRIPT_LINE_PREFIX};
pub use fleet::{CrewCommandPort, DaemonCommandPort, DummyFleet, DummyFleetSnapshot};
pub use flotilla_environment::environment::dummy::{DUMMY_ENVIRONMENT_INSTANCE, DUMMY_FLEET_ENV};
pub use forge::DummyChangeRequestTracker;
pub use terminal::{DummyTerminalPool, DUMMY_TERMINAL_POOL};
pub use vcs::DummyVcs;

impl DummyFleet {
    /// Add the dummy factories. Forge and VCS factories go first so they claim
    /// their repositories and environments ahead of the real providers; each
    /// refuses everything outside the dummy fleet.
    pub fn install(self: &Arc<Self>, factories: &mut FactoryRegistry) {
        let environment = &self.config.environment;
        let weak = Arc::downgrade(self);
        let provider = self.environments.get_or_init(|| {
            Arc::new(
                DummyEnvironmentProvider::new(DummyEnvironmentBehaviour {
                    seed: self.config.seed,
                    realise_delay: Duration::from_millis(environment.realise_delay_ms),
                    failure_rate: environment.failure_rate,
                })
                .with_cleanup(Arc::new(move |id| {
                    if let Some(fleet) = weak.upgrade() {
                        fleet.sessions.destroy_environment(id);
                        fleet.checkouts.destroy_environment(id);
                    }
                })),
            )
        });
        factories.environment_providers.push(Box::new(DummyEnvironmentFactory::new(Arc::clone(provider))));
        factories.change_requests.insert(0, Box::new(forge::DummyChangeRequestFactory { fleet: Arc::clone(self) }));
        factories.vcs.insert(0, Box::new(vcs::DummyVcsFactory { fleet: Arc::clone(self) }));
        factories.terminal_pools.push(Box::new(terminal::DummyTerminalPoolFactory { fleet: Arc::clone(self) }));
        factories
            .agent_adapters
            .push(Arc::new(adapter::DummyAgentAdapterFactory { long_runner_turns: self.config.agent.long_runner_turns }));
    }
}

#[cfg(test)]
mod factory_identity_tests {
    use flotilla_config::{dummy_fleet::DummyFleetConfig, ConfigStore};
    use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement};
    use flotilla_environment::environment::dummy::DummyCommandRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_provider_runtime::CommandRunner;

    use super::*;

    // Missing and empty identities must be refused at discovery; otherwise
    // environment-scoped destruction cannot find the provider's artifacts.
    #[tokio::test]
    async fn terminal_and_vcs_factories_require_a_nonempty_environment_identity() {
        let fleet = DummyFleet::new(DummyFleetConfig::default());
        let terminal = terminal::DummyTerminalPoolFactory { fleet: Arc::clone(&fleet) };
        let vcs = vcs::DummyVcsFactory { fleet };
        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let root = ExecutionEnvironmentPath::new("/workspace");
        let runner: Arc<dyn CommandRunner> = Arc::new(DummyCommandRunner::default());
        for identity in [None, Some(""), Some("env-one")] {
            let mut bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var(DUMMY_FLEET_ENV, "1"));
            if let Some(identity) = identity {
                bag = bag.with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", identity));
            }
            let terminal_result = terminal.probe(&bag, &config, &root, Arc::clone(&runner)).await;
            let vcs_result = vcs.probe(&bag, &config, &root, Arc::clone(&runner)).await;
            if identity == Some("env-one") {
                assert!(terminal_result.is_ok());
                assert!(vcs_result.is_ok());
            } else {
                for errors in [terminal_result.err().expect("terminal refusal"), vcs_result.err().expect("VCS refusal")] {
                    assert!(matches!(errors.as_slice(), [UnmetRequirement::MissingEnvVar(name)] if name == "FLOTILLA_ENVIRONMENT_ID"));
                }
            }
        }
    }
}
