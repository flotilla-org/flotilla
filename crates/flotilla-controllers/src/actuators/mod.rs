use std::sync::Arc;

use flotilla_environment::environment::{ProvisionedMount, ProvisionedMountMode};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_resources::{EnvironmentMount, EnvironmentMountMode, TerminalSessionSource, TerminalSessionSpec};
use flotilla_terminal::TerminalPool;

pub struct TerminalActuator {
    pool: Arc<dyn TerminalPool>,
}

impl TerminalActuator {
    pub fn new(pool: Arc<dyn TerminalPool>) -> Self {
        Self { pool }
    }

    pub async fn ensure_session(&self, name: &str, spec: &TerminalSessionSpec, env_vars: &[(String, String)]) -> Result<(), String> {
        let env_vars = env_vars.to_vec();
        let TerminalSessionSource::Tool { command } = &spec.source else {
            return Err("agent sessions require an AgentAdapter-aware runtime".to_string());
        };
        self.pool.ensure_session(name, command, &ExecutionEnvironmentPath::new(spec.cwd.clone()), &env_vars, &[]).await
    }
}

pub fn provisioned_mount(mount: &EnvironmentMount) -> ProvisionedMount {
    let mode = match mount.mode {
        EnvironmentMountMode::Ro => ProvisionedMountMode::Ro,
        EnvironmentMountMode::Rw => ProvisionedMountMode::Rw,
    };
    ProvisionedMount::new(&mount.source_path, &mount.target_path, mode)
}
