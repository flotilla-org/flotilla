use std::{path::PathBuf, sync::Arc};

use flotilla_core::{
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        environment::{CreateOpts, EnvironmentProvider, ProvisionedMount},
        terminal::TerminalPool,
        vcs::{CloneInspection, CloneProvisioner},
    },
};
use flotilla_resources::{DockerEnvironmentSpec, FreshCloneCheckoutSpec, TerminalSessionSource, TerminalSessionSpec};

pub struct CloneActuator {
    provisioner: Arc<dyn CloneProvisioner>,
}

impl CloneActuator {
    pub fn new(provisioner: Arc<dyn CloneProvisioner>) -> Self {
        Self { provisioner }
    }

    pub async fn clone_and_inspect(&self, repo_url: &str, target_path: &ExecutionEnvironmentPath) -> Result<CloneInspection, String> {
        self.provisioner.clone_repo(repo_url, target_path).await?;
        self.provisioner.inspect_clone(target_path).await
    }
}

pub struct DockerEnvironmentActuator {
    daemon_socket_path: DaemonHostPath,
    tokens: Vec<(String, String)>,
}

impl DockerEnvironmentActuator {
    pub fn new(
        _provider: Arc<dyn EnvironmentProvider>,
        _repo_root: PathBuf,
        daemon_socket_path: DaemonHostPath,
        tokens: Vec<(String, String)>,
    ) -> Self {
        Self { daemon_socket_path, tokens }
    }

    pub fn build_create_opts(&self, spec: &DockerEnvironmentSpec) -> CreateOpts {
        CreateOpts {
            tokens: self.tokens.clone(),
            daemon_socket_path: self.daemon_socket_path.clone(),
            working_directory: None,
            provisioned_mounts: spec.mounts.iter().map(|mount| ProvisionedMount::new(&mount.source_path, &mount.target_path)).collect(),
        }
    }
}

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
        self.pool.ensure_session(name, command, &ExecutionEnvironmentPath::new(spec.cwd.clone()), &env_vars).await
    }
}

pub fn fresh_clone_transport_url(spec: &FreshCloneCheckoutSpec) -> &str {
    &spec.url
}
