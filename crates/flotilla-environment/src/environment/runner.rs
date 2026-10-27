use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use uuid::Uuid;

use flotilla_provider_runtime::atomic_write_script;
use flotilla_provider_runtime::helper_exec_script;
use flotilla_provider_runtime::install_managed_helper_script;
use flotilla_provider_runtime::rename_command_timeout;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::FLOTILLA_HELPER_NAME;
use flotilla_provider_runtime::FLOTILLA_HELPER_SCRIPT;

/// Persistent writable base reserved inside provisioned containers. Unlike a
/// user's home, this location is known before container creation, so writable
/// material mounts and post-create runner writes can share one path contract.
pub const CONTAINED_WRITABLE_CONFIG_BASE: &str = "/tmp/flotilla-config";
pub const CONTAINED_CODEX_HOME: &str = "/tmp/flotilla-codex";
/// Credential delivery and capture share this filename contract.
pub const CONTAINED_CODEX_AUTH_FILE: &str = "auth.json";

/// Authentication files linked from delivered material into writable harness homes.
/// This list also drives crew links and archive exclusions.
pub const HARNESS_AUTH_FILES: &[&str] = &[CONTAINED_CODEX_AUTH_FILE, ".credentials.json"];

/// Shared delivery layout: opaque credential kinds stay in private directories;
/// harness scratch lives separately and can be recovered without those directories.
pub struct CredentialDeliveryPaths {
    pub base: PathBuf,
    pub git_config: PathBuf,
}
impl CredentialDeliveryPaths {
    pub fn new(base: PathBuf) -> Self {
        let git_config = base.join("credentials/gitconfig");
        Self { base, git_config }
    }
    pub fn credential_dir(&self, name: &str) -> PathBuf {
        self.base.join("credentials").join(Self::component(name))
    }
    pub fn harness_home(&self, name: &str, harness: &str) -> PathBuf {
        self.base.join("agent-homes").join(Self::component(name)).join(harness)
    }
    /// Where the in-guest skill stager kept source tokens before #3663.
    /// Vessels that predate host-side fetching may still hold stale tokens
    /// there, so session capture keeps excluding it; remove one roll later.
    pub fn skill_source_dir(&self) -> PathBuf {
        self.base.join("skill-sources")
    }
    fn component(name: &str) -> String {
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
    }
}

/// Resolves the user's durable state base: `$XDG_STATE_HOME/flotilla` when that is absolute
/// and usable, otherwise `~/.local/state/flotilla`. Anything it creates is owner-only.
const DURABLE_STATE_BASE_SCRIPT: &str = "umask 077; \
                                         for state in \"${XDG_STATE_HOME:-}\" \"${HOME:+$HOME/.local/state}\"; do \
                                             case \"$state\" in /*) ;; *) continue ;; esac; \
                                             if mkdir -p \"$state/flotilla\" 2>/dev/null \
                                                 && [ -d \"$state/flotilla\" ] && [ -w \"$state/flotilla\" ]; then \
                                                 printf '%s' \"$state/flotilla\"; exit 0; \
                                             fi; \
                                         done; \
                                         exit 1";

/// The durable base a target's harness homes live under (#4012), resolved in the runner's own
/// filesystem world. A runner whose config base differs from its scratch base declares a
/// persistent place of its own, such as a container's disk, and homes stay there. A runner
/// whose config base is its scratch base, such as a host login's `$XDG_RUNTIME_DIR` (a tmpfs),
/// declares none, so homes go under the user's durable state base instead.
pub async fn home_base(runner: &dyn CommandRunner) -> Result<PathBuf, String> {
    let fallback = Path::new(CONTAINED_WRITABLE_CONFIG_BASE);
    let config = runner.writable_config_base(None, fallback).await?;
    if config != runner.writable_scratch_base(None, fallback).await? {
        return Ok(config);
    }
    let base = runner
        .run("sh", &["-c", DURABLE_STATE_BASE_SCRIPT, "flotilla-durable-base"], Path::new("/"), &ChannelLabel::Default)
        .await
        .map_err(|error| format!("find the durable state directory: {error}"))?;
    let base = PathBuf::from(base.trim());
    if !base.is_absolute() {
        return Err(format!("find the durable state directory: `{}` is not absolute", base.display()));
    }
    Ok(base)
}

/// Delivery-derived exclusions; auth links are excluded at every depth too.
#[cfg(unix)]
pub(crate) fn contained_credential_exclusions() -> Vec<String> {
    let paths = CredentialDeliveryPaths::new(CONTAINED_WRITABLE_CONFIG_BASE.into());
    [paths.base.join("credentials"), paths.skill_source_dir(), paths.base.join(flotilla_resources::account_access::REQUESTER_MATERIAL_DIR)]
        .into_iter()
        .map(|p| p.to_string_lossy().trim_start_matches('/').to_string())
        .chain(HARNESS_AUTH_FILES.iter().map(|p| p.to_string()))
        .collect()
}

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
    async fn same_filesystem(&self, _source: &Path, _destination: &Path) -> Result<bool, String> {
        Err("host clone filesystem comparison must run before container provisioning".into())
    }

    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let docker_args = self.docker_exec_args(cmd, args, cwd, false);
        let arg_refs = docker_args.iter().map(String::as_str).collect::<Vec<_>>();
        self.inner.run("docker", &arg_refs, Path::new("/"), label).await
    }

    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        let docker_args = self.docker_exec_args(cmd, args, cwd, false);
        let arg_refs = docker_args.iter().map(String::as_str).collect::<Vec<_>>();
        self.inner
            .run_with_timeout("docker", &arg_refs, Path::new("/"), label, timeout)
            .await
            .map_err(|error| rename_command_timeout(error, "docker", cmd, timeout))
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
        let helper_path = install_managed_helper_script(
            &*self.inner,
            "docker",
            &self.docker_exec_prefix(),
            "-c",
            FLOTILLA_HELPER_NAME,
            FLOTILLA_HELPER_SCRIPT,
        )
        .await?;
        let mut owned_args: Vec<String> = self.docker_exec_prefix().into_iter().map(str::to_string).collect();
        let helper_script = helper_exec_script(&helper_path, "ensure-file-if-absent", &[&path_str, content, &temp_suffix])?;
        owned_args.extend(["sh".to_string(), "-c".to_string(), helper_script]);
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        self.inner.run("docker", &arg_refs, Path::new("/"), &ChannelLabel::Default).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let script = atomic_write_script(path, &Uuid::new_v4().to_string(), None)?;
        let args = ["exec", "-i", &self.container_name, "sh", "-c", script.as_str()];
        self.inner.run_with_input("docker", &args, Path::new("/"), &ChannelLabel::Default, content.as_bytes()).await.map(|_| ())
    }

    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        let script = atomic_write_script(path, &Uuid::new_v4().to_string(), Some(mode))?;
        let args = ["exec", "-i", &self.container_name, "sh", "-c", script.as_str()];
        self.inner.run_with_input("docker", &args, Path::new("/"), &ChannelLabel::Default, content.as_bytes()).await.map(|_| ())
    }

    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let source = source.to_string_lossy();
        self.inner.run_to_file("docker", &["exec", &self.container_name, "cat", &source], Path::new("/"), destination).await
    }

    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let script = atomic_write_script(destination, &Uuid::new_v4().to_string(), None)?;
        self.inner.run_from_file("docker", &["exec", "-i", &self.container_name, "sh", "-c", &script], Path::new("/"), source).await
    }
}

#[cfg(test)]
mod tests {
    use std::{future, path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;
    #[cfg(unix)]
    use uuid::Uuid;

    use super::DockerEnvironmentRunner;
    #[cfg(unix)]
    use super::CONTAINED_CODEX_HOME;
    use flotilla_provider_runtime::command_timeout_message;
    use flotilla_provider_runtime::ChannelLabel;
    use flotilla_provider_runtime::CommandOutput;
    use flotilla_provider_runtime::CommandRunner;
    #[cfg(unix)]
    use flotilla_provider_runtime::ProcessCommandRunner;
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_replay_testkit::testing::TimeoutOnlyRunner;

    #[tokio::test]
    async fn timeout_is_forwarded_to_docker_exec() {
        let inner = Arc::new(TimeoutOnlyRunner::new(Ok("done".into())));
        let runner = DockerEnvironmentRunner::new("container".into(), inner.clone());
        let result = runner.run_with_timeout("cmd", &["arg"], Path::new("/work"), &ChannelLabel::Default, Duration::from_secs(3)).await;
        assert_eq!(result.expect("command result"), "done");
        let calls = inner.calls.lock().expect("calls mutex");
        assert_eq!(
            calls.as_slice(),
            &[(
                "docker".into(),
                vec!["exec".into(), "-w".into(), "/work".into(), "container".into(), "cmd".into(), "arg".into()],
                Path::new("/").to_path_buf(),
                Duration::from_secs(3),
            )]
        );
    }

    #[tokio::test]
    async fn docker_timeout_names_the_requested_command() {
        let timeout = Duration::from_millis(500);
        let inner = Arc::new(TimeoutOnlyRunner::new(Err(command_timeout_message("docker", timeout))));
        let runner = DockerEnvironmentRunner::new("container".into(), inner);
        let result = runner.run_with_timeout("zellij", &["action"], Path::new("/work"), &ChannelLabel::Default, timeout).await;
        assert_eq!(result.expect_err("deadline"), "zellij timed out after 500ms");
    }

    // Behaviour (#4012): a host's durable state base is `$XDG_STATE_HOME/flotilla` when that
    // is absolute and usable, otherwise `~/.local/state/flotilla`, whatever the login's
    // `XDG_RUNTIME_DIR`; what it creates is owner-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_durable_state_base_follows_xdg_state_home_then_home() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let (home, state, runtime) = (temp.path().join("home"), temp.path().join("state"), temp.path().join("runtime"));
        for directory in [&home, &runtime] {
            std::fs::create_dir(directory).expect("directory");
        }
        let resolve = |xdg_state_home: &str| {
            let assignments = [
                "PATH=/usr/bin:/bin".to_string(),
                format!("HOME={}", home.display()),
                format!("XDG_RUNTIME_DIR={}", runtime.display()),
                format!("XDG_STATE_HOME={xdg_state_home}"),
            ];
            async move {
                let mut args = vec!["-i"];
                args.extend(assignments.iter().map(String::as_str));
                args.extend(["sh", "-c", super::DURABLE_STATE_BASE_SCRIPT, "flotilla-durable-base"]);
                ProcessCommandRunner.run("env", &args, Path::new("/"), &ChannelLabel::Default).await.expect("durable base")
            }
        };
        assert_eq!(resolve(&state.display().to_string()).await, state.join("flotilla").display().to_string());
        assert_eq!(std::fs::metadata(state.join("flotilla")).expect("base").permissions().mode() & 0o777, 0o700);
        for unusable in ["", "relative/state", "/proc/flotilla-unwritable"] {
            assert_eq!(resolve(unusable).await, home.join(".local/state/flotilla").display().to_string(), "{unusable:?}");
        }
    }

    // Behaviour (#4012): which base each runner's harness homes take. A container's runner
    // declares a persistent config base apart from its scratch base, so homes stay on the
    // container's disk; an SSH host's runtime base is its scratch base too, so homes go to
    // the host user's durable state base.
    #[tokio::test]
    async fn each_runner_kind_places_homes_on_its_durable_base() {
        let docker =
            DockerEnvironmentRunner::new("container".into(), Arc::new(MockRunner::new(vec![Ok(String::new()), Ok("/home/crew".into())])));
        assert_eq!(super::home_base(&docker).await.expect("container home base"), Path::new(super::CONTAINED_WRITABLE_CONFIG_BASE));

        let remote = Arc::new(MockRunner::new(vec![
            Ok("/run/user/1000/flotilla".into()),
            Ok("/run/user/1000/flotilla".into()),
            Ok("/home/crew/.local/state/flotilla".into()),
        ]));
        let ssh = flotilla_provider_runtime::ssh_runner::SshCommandRunner::new("crew@host", false, remote.clone());
        assert_eq!(super::home_base(&ssh).await.expect("SSH home base"), Path::new("/home/crew/.local/state/flotilla"));
        assert!(remote.calls().last().expect("durable probe").1.iter().any(|arg| arg.contains("flotilla-durable-base")));
    }

    #[cfg(unix)]
    struct DockerContainer(String);

    #[cfg(unix)]
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
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
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

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires Docker and the busybox:latest image"]
    async fn persistent_delivery_paths_work_in_a_real_non_root_container() {
        use std::os::unix::fs::PermissionsExt;

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

        let content = runner.ensure_file(Path::new("/app/config/cleat.toml"), "key = true\n").await.expect("ensure_file");
        assert_eq!(content, String::new());

        let calls = inner.calls();
        assert_eq!(calls.len(), 2);

        assert_eq!(calls[0].0, "docker");
        let install_args = &calls[0].1;
        assert!(install_args.contains(&"exec".to_string()));
        assert!(install_args.contains(&"my-container".to_string()));
        assert!(install_args.contains(&"sh".to_string()));
        assert!(install_args.contains(&"-c".to_string()));
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
        assert_eq!(args.get(3).map(String::as_str), Some("-c"));
        let script = args.get(4).expect("docker helper script");
        assert!(script.contains("PATH='/remote/state/flotilla/helpers/helper-hash':\"$PATH\""));
        assert!(script.contains("exec 'flotilla-helper' 'ensure-file-if-absent'"));
        assert!(script.contains("'/app/config/cleat.toml'"));
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
        assert!(!calls[0].1.last().expect("write script").contains("umask 077"), "ordinary writes retain the environment umask");
    }

    #[tokio::test]
    async fn protected_write_sets_mode_before_container_rename() {
        let inner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());
        runner.write_file_with_mode(Path::new("/app/token"), "secret token", 0o600).await.expect("write token");
        let calls = inner.calls();
        let script = calls[0].1.last().expect("write script");
        assert!(script.contains("chmod 600 \"$tmp\"; mv"), "script: {script}");
        assert!(calls[0].1.iter().all(|arg| !arg.contains("secret token")));
    }

    #[tokio::test]
    async fn binary_file_transfer_uses_docker_exec_streams() {
        let inner = Arc::new(MockRunner::new(vec![]));
        let runner = DockerEnvironmentRunner::new("my-container".into(), inner.clone());
        let host_path = Path::new("/tmp/daemon-body");
        let crew_path = Path::new("/work/body.bin");

        runner.read_file_to(crew_path, host_path).await.expect("read binary file");
        runner.write_file_from(host_path, crew_path).await.expect("write binary file");

        let calls = inner.calls();
        assert_eq!(calls[0], ("docker".into(), vec!["exec".into(), "my-container".into(), "cat".into(), "/work/body.bin".into()]));
        assert_eq!(calls[1].0, "docker");
        assert_eq!(&calls[1].1[..5], ["exec", "-i", "my-container", "sh", "-c"]);
        assert!(calls[1].1[5].contains("cat > \"$tmp\""));
    }
}
