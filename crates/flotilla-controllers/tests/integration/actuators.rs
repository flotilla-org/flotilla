use async_trait::async_trait;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_resources::TerminalSessionSpec;
use flotilla_terminal::{TerminalEnvVars, TerminalPool, TerminalSession as PoolSession};
use std::sync::Arc;

#[derive(Default)]
struct FakeTerminalPool;

#[async_trait]
impl TerminalPool for FakeTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<PoolSession>, String> {
        Ok(vec![])
    }

    async fn ensure_session(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
        _tags: &[flotilla_terminal::TerminalSessionTag],
    ) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(vec![])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn terminal_actuator_uses_literal_command_and_cwd() {
    let actuator = flotilla_controllers::actuators::TerminalActuator::new(Arc::new(FakeTerminalPool));
    let spec = TerminalSessionSpec {
        env_ref: "env-a".to_string(),
        role: "coder".to_string(),
        source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/workspace".to_string(),
        env: Default::default(),
        pool: "cleat".to_string(),
    };

    actuator.ensure_session("term-a", &spec, &[]).await.expect("ensure should succeed");
}
