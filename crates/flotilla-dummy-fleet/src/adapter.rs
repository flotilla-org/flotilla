//! The dummy agent harness. Launching selects the crew script from the brief
//! and hands it to the dummy terminal pool through the launch environment;
//! screen classification reads the pool's rendered status line.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_agent::adapter::{AgentAdapter, AgentAdapterFactory, AgentLaunchPlan, AgentLaunchRequest, TRUSTED_IMPLICIT_STANCE};
use flotilla_discovery_api::EnvironmentBag;
use flotilla_environment::environment::dummy::DUMMY_FLEET_ENV;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{TerminalAttentionState, TerminalBrief};

use crate::crew::CrewScript;

/// Adapter id that workflow selectors name to run dummy crews.
pub const DUMMY_AGENT_ADAPTER: &str = "dummy";
/// Launch environment variable carrying the selected crew script.
pub const DUMMY_SCRIPT_ENV: &str = "FLOTILLA_DUMMY_SCRIPT";
pub(crate) const SCREEN_WORKING: &str = "working";
pub(crate) const SCREEN_IDLE: &str = "idle";
const LAUNCH_COMMAND: &str = "flotilla-dummy-crew";

pub struct DummyAgentAdapter {
    long_runner_turns: u32,
}

impl DummyAgentAdapter {
    pub fn new(long_runner_turns: u32) -> Self {
        Self { long_runner_turns }
    }
}

#[async_trait]
impl AgentAdapter for DummyAgentAdapter {
    fn id(&self) -> &'static str {
        DUMMY_AGENT_ADAPTER
    }

    async fn prepare(&self, _cwd: &ExecutionEnvironmentPath, _brief: &TerminalBrief) -> Result<(), String> {
        Ok(())
    }

    fn launch(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan, String> {
        let script = CrewScript::from_brief(&request.brief.content, self.long_runner_turns)?;
        let mut env = request.environment.clone();
        env.push((DUMMY_SCRIPT_ENV.into(), script.render()));
        Ok(AgentLaunchPlan { command: LAUNCH_COMMAND.into(), env, stance: TRUSTED_IMPLICIT_STANCE.into() })
    }

    fn classify_screen_attention(&self, screen: &str) -> Option<TerminalAttentionState> {
        let status = screen.lines().find(|line| line.starts_with("flotilla dummy crew"))?;
        match status.rsplit(" · ").next()? {
            SCREEN_WORKING => Some(TerminalAttentionState::Working),
            SCREEN_IDLE => Some(TerminalAttentionState::Idle),
            _ => None,
        }
    }
}

/// Registers the dummy adapter inside dummy environments only.
pub struct DummyAgentAdapterFactory {
    pub(crate) long_runner_turns: u32,
}

impl AgentAdapterFactory for DummyAgentAdapterFactory {
    fn probe(&self, env: &EnvironmentBag, _runner: Arc<dyn CommandRunner>) -> Option<Arc<dyn AgentAdapter>> {
        env.find_env_var(DUMMY_FLEET_ENV)
            .map(|_| Arc::new(DummyAgentAdapter { long_runner_turns: self.long_runner_turns }) as Arc<dyn AgentAdapter>)
    }
}

#[cfg(test)]
mod tests {
    use flotilla_discovery_api::EnvironmentAssertion;

    use super::*;

    fn request(brief: &str) -> AgentLaunchRequest {
        AgentLaunchRequest {
            role: "coder".into(),
            model: None,
            brief: TerminalBrief { path: "brief.md".into(), content: brief.into(), artifact_digest: None, copies: Vec::new() },
            environment: vec![("HOME".into(), "/home/crew".into())],
            fulfilment_grants: None,
        }
    }

    #[test]
    fn launch_carries_the_brief_script_to_the_pool() {
        let adapter = DummyAgentAdapter { long_runner_turns: 3 };
        let plan = adapter.launch(&request("Work.\ndummy-script: governor aide=aide\n")).expect("launch plan");
        assert_eq!(plan.env.last(), Some(&(DUMMY_SCRIPT_ENV.to_string(), "governor aide=aide turns=3".to_string())));
        assert!(plan.env.contains(&("HOME".into(), "/home/crew".into())));
        assert!(adapter.launch(&request("dummy-script: pilot")).is_err());
    }

    #[test]
    fn screens_classify_as_working_or_idle() {
        let adapter = DummyAgentAdapter { long_runner_turns: 1 };
        assert_eq!(adapter.classify_screen_attention("flotilla dummy crew · coder · working\n"), Some(TerminalAttentionState::Working));
        assert_eq!(adapter.classify_screen_attention("flotilla dummy crew · coder · idle\n"), Some(TerminalAttentionState::Idle));
        assert_eq!(adapter.classify_screen_attention("$ cargo test\n"), None);
    }

    #[test]
    fn the_factory_registers_only_inside_dummy_environments() {
        let factory = DummyAgentAdapterFactory { long_runner_turns: 1 };
        let runner: Arc<dyn CommandRunner> = Arc::new(flotilla_environment::environment::dummy::DummyCommandRunner::default());
        assert!(factory.probe(&EnvironmentBag::new(), Arc::clone(&runner)).is_none());
        let dummy = EnvironmentBag::new().with(EnvironmentAssertion::env_var(DUMMY_FLEET_ENV, "1"));
        assert_eq!(factory.probe(&dummy, runner).map(|adapter| adapter.id()), Some(DUMMY_AGENT_ADAPTER));
    }
}
