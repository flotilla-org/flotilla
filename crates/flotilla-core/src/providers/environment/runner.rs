use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use uuid::Uuid;

use crate::providers::{
    atomic_write_script, helper_exec_script, install_managed_helper_script, ChannelLabel, CommandOutput, CommandRunner,
    FLOTILLA_HELPER_NAME, FLOTILLA_HELPER_SCRIPT,
};

/// A `CommandRunner` decorator that executes all commands inside a Docker container
/// via `docker exec`. Absolute working directories are forwarded as `-w`; relative
/// paths use the container's configured working directory. The host-side cwd is `/`.
pub struct DockerEnvironmentRunner {
    container_name: String,
    inner: Arc<dyn CommandRunner>,
}

impl DockerEnvironmentRunner {
    pub fn new(container_name: String, inner: Arc<dyn CommandRunner>) -> Self {
        Self { container_name, inner }
    }

    fn docker_exec_args(&self, cmd: &str, args: &[&str], cwd: &Path, interactive: bool) -> Vec<String> {
        let mut docker_args = vec!["exec".to_string()];
        if interactive {
            docker_args.push("-i".to_string());
        }
        if cwd.is_absolute() {
            docker_args.extend(["-w".to_string(), cwd.to_string_lossy().into_owned()]);
        }
        docker_args.extend([self.container_name.clone(), cmd.to_string()]);
        docker_args.extend(args.iter().map(|arg| (*arg).to_string()));
        docker_args
    }

    fn docker_exec_prefix(&self) -> Vec<&str> {
        vec!["exec", &self.container_name]
    }
}

#[async_trait]
impl CommandRunner for DockerEnvironmentRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let docker_args = self.docker_exec_args(cmd, args, cwd, false);
        let arg_refs = docker_args.iter().map(String::as_str).collect::<Vec<_>>();
        self.inner.run("docker", &arg_refs, Path::new("/"), label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let docker_args = self.docker_exec_args(cmd, args, cwd, false);
        let arg_refs = docker_args.iter().map(String::as_str).collect::<Vec<_>>();
        self.inner.run_output("docker", &arg_refs, Path::new("/"), label).await
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let docker_args = self.docker_exec_args(cmd, args, cwd, true);
        let arg_refs = docker_args.iter().map(String::as_str).collect::<Vec<_>>();
        self.inner.run_with_input("docker", &arg_refs, Path::new("/"), label, input).await
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        let docker_args = ["exec", &self.container_name, "which", cmd];
        self.inner.run("docker", &docker_args, Path::new("/"), &ChannelLabel::Noop).await.is_ok()
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        let temp_suffix = Uuid::new_v4().to_string();
        let path_str = path.to_string_lossy().into_owned();
        let helper_path =
            install_managed_helper_script(&*self.inner, "docker", &self.docker_exec_prefix(), FLOTILLA_HELPER_NAME, FLOTILLA_HELPER_SCRIPT)
                .await?;
        let mut owned_args: Vec<String> = self.docker_exec_prefix().into_iter().map(str::to_string).collect();
        let helper_script = helper_exec_script(&helper_path, "ensure-file-if-absent", &[&path_str, content, &temp_suffix])?;
        owned_args.extend(["sh".to_string(), "-lc".to_string(), helper_script]);
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        self.inner.run("docker", &arg_refs, Path::new("/"), &ChannelLabel::Noop).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let script = atomic_write_script(path, &Uuid::new_v4().to_string())?;
        let args = ["exec", "-i", &self.container_name, "sh", "-lc", script.as_str()];
        self.inner.run_with_input("docker", &args, Path::new("/"), &ChannelLabel::Noop, content.as_bytes()).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::{future, path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;

    use super::DockerEnvironmentRunner;
    use crate::providers::{testing::MockRunner, ChannelLabel, CommandOutput, CommandRunner};

    struct InvalidWorkdirHangingRunner;

    #[async_trait]
    impl CommandRunner for InvalidWorkdirHangingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "docker");
            if args.windows(2).any(|pair| pair == ["-w", "."]) {
                return future::pending().await;
            }
            assert_eq!(args, ["exec", "my-container", "git", "--version"]);
            Ok("git version 2.51.0\n".to_string())
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), success: true })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn relative_cwd_version_probe_does_not_hang() {
        let runner = DockerEnvironmentRunner::new("my-container".into(), Arc::new(InvalidWorkdirHangingRunner));

        let output =
            tokio::time::timeout(Duration::from_millis(100), runner.run("git", &["--version"], Path::new("."), &ChannelLabel::Noop))
                .await
                .expect("relative-cwd docker exec should be reaped")
                .expect("version probe should succeed");

        assert_eq!(output, "git version 2.51.0\n");
    }

    #[test]
    fn interactive_exec_forwards_absolute_workdir() {
        let runner = DockerEnvironmentRunner::new("my-container".into(), Arc::new(MockRunner::new(Vec::new())));

        let args = runner.docker_exec_args("sh", &["-lc", "pwd"], Path::new("/workspace"), true);

        assert_eq!(args, ["exec", "-i", "-w", "/workspace", "my-container", "sh", "-lc", "pwd"]);
    }

    #[tokio::test]
    async fn ensure_file_delegates_via_docker_exec_sh() {
        let inner =
            Arc::new(MockRunner::new(vec![Ok("/remote/state/flotilla/helpers/helper-hash/flotilla-helper\n".into()), Ok(String::new())]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());

        let content = runner.ensure_file(Path::new("/app/config/shpool.toml"), "key = true\n").await.expect("ensure_file");
        assert_eq!(content, String::new());

        let calls = inner.calls();
        assert_eq!(calls.len(), 2);

        assert_eq!(calls[0].0, "docker");
        let install_args = &calls[0].1;
        assert!(install_args.contains(&"exec".to_string()));
        assert!(install_args.contains(&"my-container".to_string()));
        assert!(install_args.contains(&"sh".to_string()));
        assert!(install_args.contains(&"-lc".to_string()));
        let bootstrap_script = install_args.get(4).expect("should have install bootstrap script arg");
        assert!(bootstrap_script.contains("helpers/$helper_hash"));
        assert_eq!(install_args.get(5).map(String::as_str), Some("flotilla-bootstrap-install-managed-script"));
        assert_eq!(install_args.get(6).map(String::as_str), Some("flotilla-helper"));
        assert!(install_args.get(7).is_some());

        assert_eq!(calls[1].0, "docker");
        let args = &calls[1].1;
        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"my-container".to_string()));
        assert_eq!(args.get(2).map(String::as_str), Some("sh"));
        assert_eq!(args.get(3).map(String::as_str), Some("-lc"));
        let script = args.get(4).expect("docker helper script");
        assert!(script.contains("PATH='/remote/state/flotilla/helpers/helper-hash':\"$PATH\""));
        assert!(script.contains("exec 'flotilla-helper' 'ensure-file-if-absent'"));
        assert!(script.contains("'/app/config/shpool.toml'"));
        assert!(script.contains("'key = true\n'"));
    }

    #[tokio::test]
    async fn write_file_keeps_content_out_of_docker_argv() {
        let inner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());

        runner.write_file(Path::new("/app/.flotilla/briefs/coder.md"), "secret assignment").await.expect("write_file");

        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "docker");
        assert!(calls[0].1.iter().all(|arg| !arg.contains("secret assignment")));
        assert!(calls[0].1.contains(&"-i".to_string()));
        assert!(calls[0].1.last().expect("write script").contains("cat > \"$tmp\""));
    }
}
