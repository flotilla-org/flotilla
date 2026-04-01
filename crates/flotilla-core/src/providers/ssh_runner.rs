use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use uuid::Uuid;

use crate::providers::{
    helper_exec_script, install_managed_helper_script, ChannelLabel, CommandOutput, CommandRunner, FLOTILLA_HELPER_NAME,
    FLOTILLA_HELPER_SCRIPT,
};

/// Command runner that executes commands on a remote host over SSH.
///
/// This is intentionally narrow: it shells out to `ssh` for direct-environment
/// execution and discovery, but it does not model daemon-to-daemon transport.
pub struct SshCommandRunner {
    destination: String,
    multiplex: bool,
    runner: Arc<dyn CommandRunner>,
}

impl SshCommandRunner {
    pub fn new(destination: impl Into<String>, multiplex: bool, runner: Arc<dyn CommandRunner>) -> Self {
        Self { destination: destination.into(), multiplex, runner }
    }

    fn ssh_prefix_args(&self) -> Vec<&str> {
        let mut args = vec!["-T", "-o", "BatchMode=yes"];
        if self.multiplex {
            args.extend(["-o", "ControlMaster=auto", "-o", "ControlPath=/tmp/flotilla-ssh-%C", "-o", "ControlPersist=60"]);
        }
        args.push(self.destination.as_str());
        args
    }

    fn ssh_shell_args<'a>(&'a self, script: &'a str) -> Vec<&'a str> {
        let mut args = self.ssh_prefix_args();
        args.push("sh");
        args.push("-lc");
        args.push(script);
        args
    }

    fn remote_exec_script(&self, cmd: &str, args: &[&str], cwd: &Path) -> String {
        let mut parts = Vec::with_capacity(args.len() + 4);
        parts.push(format!("cd {}", flotilla_protocol::arg::shell_quote(&cwd.to_string_lossy())));
        parts.push("&&".to_string());
        parts.push("exec".to_string());
        parts.push(flotilla_protocol::arg::shell_quote(cmd));
        parts.extend(args.iter().map(|arg| flotilla_protocol::arg::shell_quote(arg)));
        parts.join(" ")
    }

    async fn execute(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let script = self.remote_exec_script(cmd, args, cwd);
        let ssh_args = self.ssh_shell_args(&script);
        self.runner.run("ssh", &ssh_args, Path::new("/"), label).await
    }
}

#[async_trait]
impl CommandRunner for SshCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.execute(cmd, args, cwd, label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let script = self.remote_exec_script(cmd, args, cwd);
        let ssh_args = self.ssh_shell_args(&script);
        self.runner.run_output("ssh", &ssh_args, Path::new("/"), label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.execute(cmd, args, Path::new("/"), &ChannelLabel::Noop).await.is_ok()
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        let temp_suffix = Uuid::new_v4().to_string();
        let path_str = path.to_string_lossy().into_owned();
        let helper_path =
            install_managed_helper_script(&*self.runner, "ssh", &self.ssh_prefix_args(), FLOTILLA_HELPER_NAME, FLOTILLA_HELPER_SCRIPT)
                .await?;
        let helper_script = helper_exec_script(&helper_path, "ensure-file-if-absent", &[&path_str, content, &temp_suffix])?;
        let mut owned_args: Vec<String> = self.ssh_prefix_args().into_iter().map(str::to_string).collect();
        owned_args.extend(["sh".to_string(), "-lc".to_string(), helper_script]);
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        self.runner.run("ssh", &arg_refs, Path::new("/"), &ChannelLabel::Noop).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use async_trait::async_trait;

    use super::SshCommandRunner;
    use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>, PathBuf)>>,
        run_results: Mutex<VecDeque<Result<String, String>>>,
        run_output_result: Mutex<Option<Result<CommandOutput, String>>>,
    }

    impl RecordingRunner {
        fn with_run_result(result: Result<String, String>) -> Self {
            Self::with_run_results(vec![result])
        }

        fn with_run_results(results: Vec<Result<String, String>>) -> Self {
            Self { calls: Mutex::new(Vec::new()), run_results: Mutex::new(results.into()), run_output_result: Mutex::new(None) }
        }

        fn with_run_output_result(result: Result<CommandOutput, String>) -> Self {
            Self { calls: Mutex::new(Vec::new()), run_results: Mutex::new(VecDeque::new()), run_output_result: Mutex::new(Some(result)) }
        }

        fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
            self.calls.lock().expect("calls mutex").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls mutex").push((
                cmd.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                cwd.to_path_buf(),
            ));
            self.run_results.lock().expect("run_results mutex").pop_front().expect("run result not configured")
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.calls.lock().expect("calls mutex").push((
                cmd.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                cwd.to_path_buf(),
            ));
            self.run_output_result.lock().expect("run_output_result mutex").take().expect("run output result not configured")
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    fn ssh_call_args(calls: &[(String, Vec<String>, PathBuf)]) -> &Vec<String> {
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ssh");
        &calls[0].1
    }

    #[tokio::test]
    async fn run_builds_ssh_command_with_working_directory() {
        let inner = std::sync::Arc::new(RecordingRunner::with_run_result(Ok("stdout".into())));
        let runner = SshCommandRunner::new("alice@feta.local", false, inner.clone());

        let output = runner.run("git", &["status", "--short"], Path::new("/repo with space"), &ChannelLabel::Noop).await;

        assert_eq!(output.unwrap(), "stdout");
        let calls = inner.calls();
        let args = ssh_call_args(&calls);
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "BatchMode=yes");
        assert_eq!(args[3], "alice@feta.local");
        assert_eq!(args[4], "sh");
        assert_eq!(args[5], "-lc");
        assert_eq!(args[6], "cd '/repo with space' && exec 'git' 'status' '--short'");
    }

    #[tokio::test]
    async fn run_output_preserves_stdout_and_stderr() {
        let inner = std::sync::Arc::new(RecordingRunner::with_run_output_result(Ok(CommandOutput {
            stdout: "out".into(),
            stderr: "err".into(),
            success: false,
        })));
        let runner = SshCommandRunner::new("alice@feta.local", true, inner.clone());

        let output = runner.run_output("git", &["status"], Path::new("/repo"), &ChannelLabel::Noop).await.unwrap();

        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        assert!(!output.success);

        let calls = inner.calls();
        let args = ssh_call_args(&calls);
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "BatchMode=yes");
        assert_eq!(&args[3..9], ["-o", "ControlMaster=auto", "-o", "ControlPath=/tmp/flotilla-ssh-%C", "-o", "ControlPersist=60"]);
        assert_eq!(args[9], "alice@feta.local");
        assert_eq!(args[10], "sh");
        assert_eq!(args[11], "-lc");
        assert_eq!(args[12], "cd '/repo' && exec 'git' 'status'");
    }

    #[tokio::test]
    async fn exists_uses_remote_command_lookup() {
        let inner = std::sync::Arc::new(RecordingRunner::with_run_result(Ok(String::new())));
        let runner = SshCommandRunner::new("alice@feta.local", false, inner.clone());

        assert!(runner.exists("cleat", &["--version"]).await);

        let calls = inner.calls();
        let args = ssh_call_args(&calls);
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "BatchMode=yes");
        assert_eq!(args[3], "alice@feta.local");
        assert_eq!(args[4], "sh");
        assert_eq!(args[5], "-lc");
        assert_eq!(args[6], "cd '/' && exec 'cleat' '--version'");
    }

    #[tokio::test]
    async fn ensure_file_writes_remote_file() {
        let inner = std::sync::Arc::new(RecordingRunner::with_run_results(vec![
            Ok("/remote/state/flotilla/helpers/helper-hash/flotilla-helper\n".into()),
            Ok(String::new()),
        ]));
        let runner = SshCommandRunner::new("alice@feta.local", false, inner.clone());

        let content = runner.ensure_file(Path::new("/etc/flotilla/config.toml"), "key = true\n").await.expect("ensure_file");
        assert_eq!(content, String::new());

        let calls = inner.calls();
        assert_eq!(calls.len(), 2);

        let install_args = &calls[0].1;
        assert_eq!(install_args[0], "-T");
        assert_eq!(install_args[1], "-o");
        assert_eq!(install_args[2], "BatchMode=yes");
        assert_eq!(install_args[3], "alice@feta.local");
        assert_eq!(install_args[4], "sh");
        assert_eq!(install_args[5], "-lc");
        assert!(install_args[6].contains("helpers/$helper_hash"));
        assert_eq!(install_args[7], "flotilla-bootstrap-install-managed-script");
        assert_eq!(install_args[8], "flotilla-helper");
        assert!(!install_args[9].is_empty());

        let args = &calls[1].1;
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "BatchMode=yes");
        assert_eq!(args[3], "alice@feta.local");
        assert_eq!(args[4], "sh");
        assert_eq!(args[5], "-lc");
        assert!(args[6].contains("PATH='/remote/state/flotilla/helpers/helper-hash':\"$PATH\""));
        assert!(args[6].contains("exec 'flotilla-helper' 'ensure-file-if-absent'"));
        assert!(args[6].contains("'/etc/flotilla/config.toml'"));
        assert!(args[6].contains("'key = true\n'"));
    }

    #[tokio::test]
    async fn run_propagates_runner_errors() {
        let inner = std::sync::Arc::new(RecordingRunner::with_run_result(Err("ssh failed".into())));
        let runner = SshCommandRunner::new("alice@feta.local", false, inner.clone());

        let error = runner.run("git", &["status"], Path::new("/repo"), &ChannelLabel::Noop).await;

        assert_eq!(error.unwrap_err(), "ssh failed");
    }
}
