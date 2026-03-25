use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

/// A `CommandRunner` decorator that executes all commands inside a Docker container
/// via `docker exec`. The caller's working directory (a path inside the container)
/// is forwarded as a `-w` flag; the host-side cwd is always `/` (irrelevant).
pub struct EnvironmentRunner {
    container_name: String,
    inner: Arc<dyn CommandRunner>,
}

impl EnvironmentRunner {
    pub fn new(container_name: String, inner: Arc<dyn CommandRunner>) -> Self {
        Self { container_name, inner }
    }
}

#[async_trait]
impl CommandRunner for EnvironmentRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let cwd_str = cwd.to_string_lossy();
        let mut docker_args = vec!["exec", "-w", &cwd_str, &self.container_name, cmd];
        docker_args.extend_from_slice(args);
        self.inner.run("docker", &docker_args, Path::new("/"), label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let cwd_str = cwd.to_string_lossy();
        let mut docker_args = vec!["exec", "-w", &cwd_str, &self.container_name, cmd];
        docker_args.extend_from_slice(args);
        self.inner.run_output("docker", &docker_args, Path::new("/"), label).await
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        let docker_args = ["exec", &self.container_name, "which", cmd];
        self.inner.run("docker", &docker_args, Path::new("/"), &ChannelLabel::Noop).await.is_ok()
    }
}
