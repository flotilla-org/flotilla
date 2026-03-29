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

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let parent = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
        let path_str = path.to_string_lossy();
        // Use printf with %s to avoid echo's backslash interpretation.
        // Content is passed as a shell argument — single-quote it and escape
        // embedded single quotes with the '\'' idiom.
        let escaped = content.replace('\'', "'\\''");
        let script = format!("mkdir -p '{parent}' && printf '%s' '{escaped}' > '{path_str}'");
        let docker_args = vec!["exec", &self.container_name, "sh", "-c", &script];
        self.inner.run("docker", &docker_args, Path::new("/"), &ChannelLabel::Noop).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::EnvironmentRunner;
    use crate::providers::{testing::MockRunner, CommandRunner};

    #[tokio::test]
    async fn ensure_file_delegates_via_docker_exec_sh() {
        let inner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let runner = EnvironmentRunner::new("my-container".into(), inner.clone());

        runner.ensure_file(Path::new("/app/config/shpool.toml"), "key = true\n").await.expect("ensure_file");

        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "docker");
        let args = &calls[0].1;
        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"my-container".to_string()));
        assert!(args.contains(&"sh".to_string()));
        assert!(args.contains(&"-c".to_string()));
        // The sh -c script should create the parent dir and write the file
        let script = args.last().expect("should have script arg");
        assert!(script.contains("mkdir -p"), "script should create parent dirs: {script}");
        assert!(script.contains("/app/config/shpool.toml"), "script should reference target path: {script}");
        assert!(script.contains("key = true"), "script should contain file content: {script}");
    }
}
