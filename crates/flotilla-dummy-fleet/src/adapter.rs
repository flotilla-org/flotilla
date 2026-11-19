//! The dummy agent harness. Launching selects the crew script from the brief
//! and hands it to the dummy terminal pool through the launch environment.
//! Like Claude Code's adapter, it reports attention through hooks only.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_agent::adapter::{AgentAdapter, AgentAdapterFactory, AgentLaunchPlan, AgentLaunchRequest, Harness, TRUSTED_IMPLICIT_STANCE};
use flotilla_discovery_api::EnvironmentBag;
use flotilla_environment::environment::dummy::{DUMMY_CREW_BINARY, DUMMY_FLEET_ENV};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::TerminalBrief;

use crate::crew::CrewScript;

/// Adapter id that workflow selectors name to run dummy crews.
pub const DUMMY_AGENT_ADAPTER: &str = "dummy";
/// Launch environment variable carrying the selected crew script.
pub const DUMMY_SCRIPT_ENV: &str = "FLOTILLA_DUMMY_SCRIPT";
pub(crate) const SCREEN_WORKING: &str = "working";
pub(crate) const SCREEN_IDLE: &str = "idle";

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
        Ok(AgentLaunchPlan { command: DUMMY_CREW_BINARY.into(), env, stance: TRUSTED_IMPLICIT_STANCE.into() })
    }

    /// A dummy crew stands in for Claude Code, which reports its attention only through its
    /// hooks: the daemon gives it the hook URL and token Claude Code takes, and reads nothing
    /// from its screen.
    fn posts_claude_code_hooks(&self) -> bool {
        true
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

    fn harness(&self) -> Option<Harness> {
        Some(Harness { adapter: DUMMY_AGENT_ADAPTER, binary: DUMMY_CREW_BINARY })
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

    // As Claude Code's adapter: attention comes from hooks, never from the screen.
    #[test]
    fn attention_comes_from_hooks_not_the_screen() {
        let adapter = DummyAgentAdapter { long_runner_turns: 1 };
        assert!(adapter.posts_claude_code_hooks());
        assert_eq!(adapter.classify_screen_attention("flotilla dummy crew · coder · working\n"), None);
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
