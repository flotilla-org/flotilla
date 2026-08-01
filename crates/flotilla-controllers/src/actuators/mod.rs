use std::{path::PathBuf, sync::Arc};

use flotilla_core::{
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        environment::{
            contained_daemon_socket_path, CreateOpts, EnvironmentProvider, EnvironmentTool, EnvironmentToolAsset,
            EnvironmentToolAssetAccess, EnvironmentToolAssetKind, EnvironmentVariableUpdate, ProvisionedMount, ProvisionedMountMode,
            CONTAINED_DAEMON_REQUIRED_ENV,
        },
        terminal::TerminalPool,
        vcs::{CloneInspection, CloneProvisioner},
    },
};
use flotilla_resources::{
    DockerEnvironmentSpec, EnvironmentMount, EnvironmentMountMode, FreshCloneCheckoutSpec, TerminalSessionSource, TerminalSessionSpec,
};

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
        let environment_socket_path = contained_daemon_socket_path(self.daemon_socket_path.as_path());
        CreateOpts {
            tokens: self.tokens.clone(),
            working_directory: None,
            image_pull_policy: spec.pull_policy.into(),
            docker_config_dir: None,
            provisioned_mounts: spec.mounts.iter().map(provisioned_mount).collect(),
            tools: vec![EnvironmentTool::new("flotilla-daemon-access", "/usr/local/bin/flotilla")
                .with_asset(EnvironmentToolAsset::new(
                    self.daemon_socket_path.as_path().to_path_buf(),
                    environment_socket_path.clone(),
                    EnvironmentToolAssetKind::UnixSocket,
                    EnvironmentToolAssetAccess::SharedWritable,
                    "the daemon socket",
                ))
                .with_environment(EnvironmentVariableUpdate::set(
                    "FLOTILLA_DAEMON_SOCKET",
                    environment_socket_path.to_string_lossy(),
                    "the daemon socket",
                ))
                .with_environment(EnvironmentVariableUpdate::set(
                    CONTAINED_DAEMON_REQUIRED_ENV,
                    "1",
                    "the contained host-daemon requirement",
                ))],
        }
    }
}

pub fn provisioned_mount(mount: &EnvironmentMount) -> ProvisionedMount {
    let mode = match mount.mode {
        EnvironmentMountMode::Ro => ProvisionedMountMode::Ro,
        EnvironmentMountMode::Rw => ProvisionedMountMode::Rw,
    };
    ProvisionedMount::new(&mount.source_path, &mount.target_path, mode)
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
        self.pool.ensure_session(name, command, &ExecutionEnvironmentPath::new(spec.cwd.clone()), &env_vars, &[]).await
    }
}

pub fn fresh_clone_transport_url(spec: &FreshCloneCheckoutSpec) -> &str {
    &spec.url
}
