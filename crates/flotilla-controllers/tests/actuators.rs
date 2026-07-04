use std::sync::Arc;

use async_trait::async_trait;
use flotilla_core::{
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        environment::{CreateOpts, EnvironmentProvider, ProvisionedEnvironment, ProvisionedMount},
        terminal::{TerminalEnvVars, TerminalPool, TerminalSession as PoolSession},
        vcs::CloneProvisioner,
    },
};
use flotilla_protocol::{EnvironmentId, ImageId};
use flotilla_resources::{DockerEnvironmentSpec, EnvironmentMount, EnvironmentMountMode, FreshCloneCheckoutSpec, TerminalSessionSpec};

#[derive(Default)]
struct FakeCloneProvisioner;

#[async_trait]
impl CloneProvisioner for FakeCloneProvisioner {
    async fn clone_repo(&self, _repo_url: &str, _target_path: &ExecutionEnvironmentPath) -> Result<(), String> {
        Ok(())
    }

    async fn inspect_clone(
        &self,
        _target_path: &ExecutionEnvironmentPath,
    ) -> Result<flotilla_core::providers::vcs::CloneInspection, String> {
        Ok(flotilla_core::providers::vcs::CloneInspection { default_branch: Some("main".to_string()) })
    }
}

#[derive(Default)]
struct FakeEnvironmentProvider;

#[async_trait]
impl EnvironmentProvider for FakeEnvironmentProvider {
    async fn ensure_image(&self, _spec: &flotilla_protocol::EnvironmentSpec, _repo_root: &std::path::Path) -> Result<ImageId, String> {
        Ok(ImageId::new("image-1"))
    }

    async fn create(&self, _id: EnvironmentId, _image: &ImageId, _opts: CreateOpts) -> Result<Arc<dyn ProvisionedEnvironment>, String> {
        unimplemented!("test double")
    }

    async fn list(&self) -> Result<Vec<Arc<dyn ProvisionedEnvironment>>, String> {
        Ok(vec![])
    }
}

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
async fn clone_actuator_returns_default_branch_from_provisioner() {
    let actuator = flotilla_controllers::actuators::CloneActuator::new(Arc::new(FakeCloneProvisioner));
    let outcome = actuator
        .clone_and_inspect("git@github.com:flotilla-org/flotilla.git", &ExecutionEnvironmentPath::new("/tmp/flotilla"))
        .await
        .expect("clone should succeed");

    assert_eq!(outcome.default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn environment_actuator_translates_mounts_into_provider_create_opts() {
    let actuator = flotilla_controllers::actuators::DockerEnvironmentActuator::new(
        Arc::new(FakeEnvironmentProvider),
        std::path::PathBuf::from("/repo"),
        DaemonHostPath::new("/tmp/flotilla.sock"),
        vec![("GITHUB_TOKEN".to_string(), "secret".to_string())],
    );
    let spec = DockerEnvironmentSpec {
        host_ref: "01HXYZ".to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        mounts: vec![EnvironmentMount {
            source_path: "/Users/alice/dev/flotilla".to_string(),
            target_path: "/workspace".to_string(),
            mode: EnvironmentMountMode::Rw,
        }],
        env: Default::default(),
    };

    let opts = actuator.build_create_opts(&spec);

    assert_eq!(opts.provisioned_mounts, vec![ProvisionedMount::new("/Users/alice/dev/flotilla", "/workspace")]);
    assert_eq!(opts.tokens, vec![("GITHUB_TOKEN".to_string(), "secret".to_string())]);
}

#[tokio::test]
async fn terminal_actuator_uses_literal_command_and_cwd() {
    let actuator = flotilla_controllers::actuators::TerminalActuator::new(Arc::new(FakeTerminalPool));
    let spec = TerminalSessionSpec {
        env_ref: "env-a".to_string(),
        role: "coder".to_string(),
        command: "cargo test".to_string(),
        cwd: "/workspace".to_string(),
        pool: "cleat".to_string(),
    };

    actuator.ensure_session("term-a", &spec, &[]).await.expect("ensure should succeed");
}

#[tokio::test]
async fn checkout_actuator_exposes_fresh_clone_transport_url() {
    let spec = FreshCloneCheckoutSpec {
        env_ref: "env-a".to_string(),
        r#ref: "main".to_string(),
        target_path: "/workspace".to_string(),
        url: "git@github.com:flotilla-org/flotilla.git".to_string(),
    };
    let command = flotilla_controllers::actuators::fresh_clone_transport_url(&spec);

    assert_eq!(command, "git@github.com:flotilla-org/flotilla.git");
}
