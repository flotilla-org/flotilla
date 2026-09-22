use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::providers::{
    atomic_write_script, helper_exec_script, install_managed_helper_script, ChannelLabel, CommandOutput, CommandRunner,
    FLOTILLA_HELPER_NAME, FLOTILLA_HELPER_SCRIPT,
};

/// Persistent writable base reserved inside provisioned containers. Unlike a
/// user's home, this location is known before container creation, so writable
/// material mounts and post-create runner writes can share one path contract.
pub const CONTAINED_WRITABLE_CONFIG_BASE: &str = "/tmp/flotilla-config";
pub const CONTAINED_CODEX_HOME: &str = "/tmp/flotilla-codex";

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

    async fn writable_base(&self, purpose: &str) -> Result<PathBuf, String> {
        let base = self
            .run(
                "sh",
                &[
                    "-c",
                    "if [ -n \"${HOME:-}\" ] && [ -d \"$HOME\" ] && [ -w \"$HOME\" ]; then printf '%s' \"$HOME\"; elif [ -d /tmp ] && [ -w /tmp ]; then printf /tmp; else exit 1; fi",
                    purpose,
                ],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await
            .map_err(|error| format!("find writable directory in container: {error}"))?;
        let base = base.trim();
        if base.is_empty() {
            return Err("find writable directory in container: probe returned an empty path".to_string());
        }
        Ok(PathBuf::from(base).join("flotilla"))
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
        self.inner.run("docker", &docker_args, Path::new("/"), &ChannelLabel::Default).await.is_ok()
    }

    async fn writable_scratch_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        self.writable_base("flotilla-scratch-base").await
    }

    async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
        self.run("sh", &["-c", "test -d /tmp && test -w /tmp", "flotilla-config-base"], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("find writable config directory in container: {error}"))?;
        Ok(PathBuf::from(CONTAINED_WRITABLE_CONFIG_BASE))
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
        self.inner.run("docker", &arg_refs, Path::new("/"), &ChannelLabel::Default).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let script = atomic_write_script(path, &Uuid::new_v4().to_string())?;
        let args = ["exec", "-i", &self.container_name, "sh", "-lc", script.as_str()];
        self.inner.run_with_input("docker", &args, Path::new("/"), &ChannelLabel::Default, content.as_bytes()).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::{future, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use uuid::Uuid;

    use super::{DockerEnvironmentRunner, CONTAINED_CODEX_HOME};
    use crate::providers::{testing::MockRunner, ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner};

    struct DockerContainer(String);

    impl Drop for DockerContainer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }

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
            tokio::time::timeout(Duration::from_millis(100), runner.run("git", &["--version"], Path::new("."), &ChannelLabel::Default))
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
    async fn writable_scratch_base_is_resolved_inside_the_container() {
        let inner = Arc::new(MockRunner::new(vec![Ok("/home/crew".into())]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());

        let scratch = runner
            .writable_scratch_base(Some(Path::new("/run/user/1000")), Path::new("/host/state"))
            .await
            .expect("container scratch base");

        assert_eq!(scratch, Path::new("/home/crew/flotilla"));
        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "docker");
        assert!(calls[0].1.starts_with(&["exec".to_string(), "-w".to_string(), "/".to_string(), "my-container".to_string()]));
        assert!(calls[0].1.iter().all(|arg| arg != "/run/user/1000" && arg != "/host/state"));
    }

    #[tokio::test]
    async fn writable_config_base_is_resolved_inside_the_container() {
        let inner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());

        let config =
            runner.writable_config_base(Some(Path::new("/run/user/1000")), Path::new("/host/state")).await.expect("container config base");

        assert_eq!(config, Path::new("/tmp/flotilla-config"));
        let calls = inner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.iter().any(|arg| arg == "flotilla-config-base"));
        assert!(calls[0].1.iter().all(|arg| arg != "/run/user/1000" && arg != "/host/state"));
    }

    #[tokio::test]
    #[ignore = "requires Docker and the busybox:latest image"]
    async fn persistent_delivery_paths_work_in_a_real_non_root_container() {
        let container = DockerContainer(format!("flotilla-non-root-delivery-{}", Uuid::new_v4()));
        let codex_home = tempfile::tempdir().expect("Codex home mount source");
        std::fs::set_permissions(codex_home.path(), std::fs::Permissions::from_mode(0o777))
            .expect("make Codex mount writable to test user");
        let codex_mount = format!("{}:{CONTAINED_CODEX_HOME}:rw", codex_home.path().display());
        ProcessCommandRunner
            .run(
                "docker",
                &[
                    "run",
                    "-d",
                    "--name",
                    &container.0,
                    "--user",
                    "65534:65534",
                    "-e",
                    "HOME=/tmp",
                    "-v",
                    &codex_mount,
                    "busybox:latest",
                    "sleep",
                    "infinity",
                ],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await
            .expect("start non-root test container");
        let runner = DockerEnvironmentRunner::new(container.0.clone(), Arc::new(ProcessCommandRunner));
        let base = runner
            .writable_config_base(Some(Path::new("/run/user/65534")), Path::new("/host/state"))
            .await
            .expect("resolve container config base");
        let git_config = base.join("credentials/gitconfig");
        let agent_environment = base.join("agent-environment");

        runner.write_file(&git_config, "[credential]\n\thelper = test\n").await.expect("write Git config as non-root user");
        runner
            .write_file(&agent_environment, &format!("export GIT_CONFIG_GLOBAL='{}'\n", git_config.display()))
            .await
            .expect("write agent environment as non-root user");

        let agent_environment_arg = agent_environment.to_string_lossy();
        let observed = runner
            .run(
                "sh",
                &[
                    "-c",
                    ". \"$1\"; test -f \"$GIT_CONFIG_GLOBAL\"; printf '%s\\n%s' \"$GIT_CONFIG_GLOBAL\" \"$1\"",
                    "flotilla-verify-delivery",
                    &agent_environment_arg,
                ],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await
            .expect("resolve exported delivery paths");

        assert_eq!(observed, format!("{}\n{}", git_config.display(), agent_environment.display()));
        runner
            .run(
                "sh",
                &["-c", "test -w \"$1\"", "flotilla-verify-codex-home", CONTAINED_CODEX_HOME],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await
            .expect("Codex home mount is writable as non-root user");
        assert!(!git_config.starts_with("/run/flotilla"));
        assert!(!agent_environment.starts_with("/run/flotilla"));
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
