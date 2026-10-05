pub mod ai_utility;
pub mod change_request;
pub mod coding_agent;
pub mod discovery;
pub mod environment;
pub mod github_api;
pub mod issue_tracker;
pub mod presentation;
pub mod registry;
pub(crate) mod scan_cache;
pub mod ssh_runner;
pub mod terminal;
pub mod types;
pub mod vcs;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Identifies the logical channel an interaction belongs to.
/// Within a replay round, interactions on the same channel are FIFO-ordered,
/// while different channels can be consumed in any order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelLabel {
    /// Use the default label rather than an explicit one; the interaction is still recorded for replay.
    Default,
    Command(String),
    GhApi(String),
    Http(String),
}

impl ChannelLabel {
    /// Extract host from a URL via simple string parsing.
    /// "https://api.example.com/v1/foo" → "api.example.com"
    pub fn http_from_url(url: &str) -> Self {
        let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).split(':').next().unwrap_or(url).to_string();
        ChannelLabel::Http(host)
    }
}

/// Request-side data passed to channel labeling strategies.
pub enum ChannelRequest<'a> {
    Command { cmd: &'a str, args: &'a [&'a str] },
    GhApi { method: &'a str, endpoint: &'a str },
    Http { method: &'a str, url: &'a str },
}

pub trait ChannelLabeler {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel;
}

pub struct DefaultLabeler;
impl ChannelLabeler for DefaultLabeler {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel {
        match request {
            ChannelRequest::Command { cmd, args } => match args.first() {
                Some(sub) if !sub.is_empty() => ChannelLabel::Command(format!("{} {}", cmd, sub)),
                _ => ChannelLabel::Command(cmd.to_string()),
            },
            ChannelRequest::GhApi { endpoint, .. } => ChannelLabel::GhApi(endpoint.to_string()),
            ChannelRequest::Http { url, .. } => ChannelLabel::http_from_url(url),
        }
    }
}

pub struct TaskId(pub &'static str);
impl ChannelLabeler for TaskId {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel {
        match request {
            ChannelRequest::Command { .. } => ChannelLabel::Command(self.0.into()),
            ChannelRequest::GhApi { .. } => ChannelLabel::GhApi(self.0.into()),
            ChannelRequest::Http { .. } => ChannelLabel::Http(self.0.into()),
        }
    }
}

pub(crate) const REPLAY_LABELS_ENABLED: bool = cfg!(any(test, feature = "replay"));
pub(crate) const INSTALL_MANAGED_SCRIPT: &str = include_str!("scripts/install_managed_script.sh");
pub(crate) const INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME: &str = "flotilla-bootstrap-install-managed-script";
pub(crate) const FLOTILLA_HELPER_NAME: &str = "flotilla-helper";
pub(crate) const FLOTILLA_HELPER_SCRIPT: &str = include_str!("scripts/flotilla_helper.sh");

#[inline]
pub(crate) fn default_channel_label() -> ChannelLabel {
    ChannelLabel::Default
}

#[inline]
pub(crate) fn command_channel_label(cmd: &str, args: &[&str]) -> ChannelLabel {
    command_channel_label_with::<REPLAY_LABELS_ENABLED, _>(cmd, args, &DefaultLabeler)
}

#[inline]
pub(crate) fn command_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    cmd: &str,
    args: &[&str],
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::Command { cmd, args };
        labeler.label_for(&request)
    } else {
        default_channel_label()
    }
}

#[inline]
pub(crate) fn gh_api_channel_label(method: &'static str, endpoint: &str) -> ChannelLabel {
    gh_api_channel_label_with::<REPLAY_LABELS_ENABLED, _>(method, endpoint, &DefaultLabeler)
}

#[inline]
pub(crate) fn gh_api_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    method: &'static str,
    endpoint: &str,
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::GhApi { method, endpoint };
        labeler.label_for(&request)
    } else {
        default_channel_label()
    }
}

#[inline]
pub(crate) fn http_channel_label(method: &str, url: &str) -> ChannelLabel {
    http_channel_label_with::<REPLAY_LABELS_ENABLED, _>(method, url, &DefaultLabeler)
}

#[inline]
pub(crate) fn http_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    method: &str,
    url: &str,
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::Http { method, url };
        labeler.label_for(&request)
    } else {
        default_channel_label()
    }
}

/// Raw output from a command, preserving stdout/stderr regardless of exit status.
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// Numeric exit status, or None when the process terminated without a code.
    pub exit_code: Option<i32>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Handle to a supervised long-lived command spawned by a [`CommandRunner`].
#[async_trait]
pub trait CommandProcess: Send + Sync {
    /// Return the exit status when the command has exited, without waiting.
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String>;

    /// Request termination of the command.
    async fn kill(&mut self) -> Result<(), String>;

    /// Wait for the command to exit.
    async fn wait(&mut self) -> Result<std::process::ExitStatus, String>;
}

pub(crate) fn helper_exec_script(helper_path: &str, subcommand: &str, args: &[&str]) -> Result<String, String> {
    let helper_dir = Path::new(helper_path).parent().ok_or_else(|| format!("installed helper path has no parent: {helper_path}"))?;
    let mut parts = vec![
        format!("PATH={}:\"$PATH\"", flotilla_protocol::arg::shell_quote(&helper_dir.to_string_lossy())),
        "exec".to_string(),
        flotilla_protocol::arg::shell_quote("flotilla-helper"),
        flotilla_protocol::arg::shell_quote(subcommand),
    ];
    parts.extend(args.iter().map(|arg| flotilla_protocol::arg::shell_quote(arg)));
    Ok(parts.join(" "))
}

pub(crate) fn atomic_write_script(path: &Path, temp_suffix: &str, mode: Option<u32>) -> Result<String, String> {
    let parent = path.parent().ok_or_else(|| format!("file path has no parent: {}", path.display()))?;
    let target = path.to_string_lossy();
    let temporary = format!("{target}.flotilla-tmp-{temp_suffix}");
    if mode.is_some_and(|mode| mode > 0o777) {
        return Err("file mode must contain only permission bits".to_string());
    }
    let protect = mode.map(|mode| format!("chmod {mode:o} \"$tmp\"; ")).unwrap_or_default();
    let private_create = if mode.is_some() { "umask 077; " } else { "" };
    Ok(format!(
        "set -eu; mkdir -p {}; tmp={}; trap 'rm -f \"$tmp\"' EXIT; {private_create}cat > \"$tmp\"; {protect}mv \"$tmp\" {}; trap - EXIT",
        flotilla_protocol::arg::shell_quote(&parent.to_string_lossy()),
        flotilla_protocol::arg::shell_quote(&temporary),
        flotilla_protocol::arg::shell_quote(&target),
    ))
}

/// `shell_flag` follows the runner's shell policy: contained writes avoid
/// login files, while static SSH hosts retain their configured login path.
pub(crate) async fn install_managed_helper_script(
    runner: &dyn CommandRunner,
    command: &str,
    command_prefix: &[&str],
    shell_flag: &str,
    helper_name: &str,
    helper_content: &str,
) -> Result<String, String> {
    let helper_hash = format!("{:x}", Sha256::digest(helper_content.as_bytes()));
    let mut owned_args: Vec<String> = command_prefix.iter().map(|arg| (*arg).to_string()).collect();
    owned_args.extend([
        "sh".to_string(),
        shell_flag.to_string(),
        INSTALL_MANAGED_SCRIPT.to_string(),
        // `sh -c` treats the next argument as `$0`; this is only a diagnostic
        // placeholder for the one-time bootstrap script text above.
        INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME.to_string(),
        helper_name.to_string(),
        helper_hash,
        helper_content.to_string(),
    ]);
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let helper_path = runner.run(command, &arg_refs, Path::new("/"), &ChannelLabel::Default).await?;
    let helper_path = helper_path.trim();
    if helper_path.is_empty() {
        return Err(format!("managed helper installer returned empty path for {helper_name}"));
    }
    Ok(helper_path.to_string())
}

/// Trait abstracting command execution so providers can be tested without
/// spawning real processes.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run a command and return stdout on success, stderr on failure.
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String>;

    /// Run a command with a deadline. Dropping the command future must stop
    /// its local child processes; wrappers must forward the deadline to their
    /// inner runner. Killing an SSH or Docker client cannot guarantee that a
    /// command on the remote host or inside the container has stopped.
    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        tokio::time::timeout(timeout, self.run(cmd, args, cwd, label)).await.map_err(|_| command_timeout_message(cmd, timeout))?
    }

    /// Run a command and return full output regardless of exit status.
    /// `Err` only if the process could not be spawned at all.
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String>;

    /// Spawn a supervised command that is expected to outlive this call.
    ///
    /// Implementations should ensure dropping the returned handle terminates
    /// the command so cancellation cannot orphan a child process.
    async fn spawn_long_lived(
        &self,
        _cmd: &str,
        _args: &[&str],
        _cwd: &Path,
        _label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        Err("command runner does not support long-lived processes".to_string())
    }

    /// Run a command with bytes supplied on stdin. The input is deliberately
    /// separate from argv so sensitive content cannot leak into process lists
    /// or recorded command transcripts.
    async fn run_with_input(
        &self,
        _cmd: &str,
        _args: &[&str],
        _cwd: &Path,
        _label: &ChannelLabel,
        _input: &[u8],
    ) -> Result<String, String> {
        Err("command runner does not support stdin input".to_string())
    }

    /// Stream a command's binary stdout into a daemon-host file.
    async fn run_to_file(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _destination: &Path) -> Result<(), String> {
        Err("command runner does not support binary file reads".to_string())
    }

    /// Stream a daemon-host file into a command's binary stdin.
    async fn run_from_file(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _source: &Path) -> Result<(), String> {
        Err("command runner does not support binary file writes".to_string())
    }

    /// Copy a file from this runner's environment to the daemon host.
    async fn read_file_to(&self, _source: &Path, _destination: &Path) -> Result<(), String> {
        Err("command runner does not support binary file reads".to_string())
    }

    /// Copy a daemon-host file into this runner's environment.
    async fn write_file_from(&self, _source: &Path, _destination: &Path) -> Result<(), String> {
        Err("command runner does not support binary file writes".to_string())
    }

    /// Check if a command is available by running it.
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool;

    /// Check whether a path exists in the runner's execution environment.
    ///
    /// Unlike checking from the daemon process, this preserves container and
    /// remote-host path semantics.
    async fn path_exists(&self, path: &Path) -> Result<bool, String> {
        let path = path.to_string_lossy();
        let args = ["-e", &*path];
        self.run_output("test", &args, Path::new("/"), &command_channel_label("test", &args)).await.map(|output| output.success())
    }

    /// Choose a Flotilla-owned writable scratch base in this runner's
    /// filesystem world.
    ///
    /// Direct runners use the host's preferred runtime directory when it is
    /// still usable, then fall back to the daemon-owned state directory.
    /// Runners that cross into another filesystem namespace must override
    /// this method and choose a path owned by that environment instead.
    async fn writable_scratch_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        if let Some(preferred) = preferred {
            let preferred_arg = preferred.to_string_lossy();
            let output = self
                .run_output(
                    "sh",
                    &["-c", "test -d \"$1\" && test -w \"$1\"", "flotilla-xdg-runtime-dir", &preferred_arg],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await;
            if output.is_ok_and(|output| output.success()) {
                return Ok(preferred.join("flotilla"));
            }
            tracing::debug!(rejected_path = %preferred.display(), "preferred scratch directory is missing or unwritable; using fallback");
        }
        Ok(fallback.to_path_buf())
    }

    /// Choose a Flotilla-owned writable base for files that must persist for
    /// the lifetime of this runner's execution environment.
    ///
    /// This is deliberately distinct from [`Self::writable_scratch_base`]:
    /// callers must not attach scratch-probe cleanup to paths returned here.
    /// The default filesystem policy is shared, while namespace-crossing
    /// runners may resolve the base in their own environment.
    async fn writable_config_base(&self, preferred: Option<&Path>, fallback: &Path) -> Result<PathBuf, String> {
        self.writable_scratch_base(preferred, fallback).await
    }

    /// Ensure `path` exists with `content` if absent, returning the resulting
    /// file contents. Existing files are preserved.
    async fn ensure_file(&self, _path: &Path, content: &str) -> Result<String, String> {
        Ok(content.to_owned())
    }

    /// Atomically replace `path` with `content`. Implementations must transport
    /// the content outside argv and command transcripts.
    async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), String> {
        Err("command runner does not support secure file writes".to_string())
    }

    /// Atomically publish content with its final Unix permissions. The
    /// temporary file must be private while it is written. Wrappers that may
    /// deliver credentials must override this method; the default fails closed.
    /// On Windows, ProcessCommandRunner falls back to an atomic write with
    /// inherited ACLs and warns that the requested mode is not enforced.
    async fn write_file_with_mode(&self, _path: &Path, _content: &str, _mode: u32) -> Result<(), String> {
        Err("command runner does not support protected file writes".to_string())
    }
}

pub(crate) fn command_timeout_message(cmd: &str, timeout: Duration) -> String {
    format!("{cmd} timed out after {timeout:?}")
}

pub(crate) fn rename_command_timeout(error: String, from: &str, to: &str, timeout: Duration) -> String {
    if error == command_timeout_message(from, timeout) {
        command_timeout_message(to, timeout)
    } else {
        error
    }
}

/// Production implementation that delegates to `tokio::process::Command`.
pub struct ProcessCommandRunner;

#[cfg(unix)]
struct ProcessGroupGuard(i32);

#[cfg(unix)]
impl ProcessGroupGuard {
    fn for_child(child: &tokio::process::Child) -> Result<Self, String> {
        let pid = child.id().expect("spawned child has pid");
        let pgid = i32::try_from(pid).map_err(|_| format!("child PID {pid} exceeds pid_t range"))?;
        Ok(Self(pgid))
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // The child was placed in a new process group at spawn. Never send a
        // negative PID to a shell utility: killpg targets exactly this group.
        if self.0 != 0 {
            unsafe { libc::killpg(self.0, libc::SIGKILL) };
        }
    }
}

impl ProcessCommandRunner {
    async fn checked_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<tokio::process::Command, String> {
        crate::vcs::guard_host_git_config_async(cmd, args, cwd).await?;
        let mut command = tokio::process::Command::new(cmd);
        command.args(args).current_dir(cwd);
        #[cfg(unix)]
        command.process_group(0);
        Ok(command)
    }

    async fn command_output(cmd: &str, args: &[&str], cwd: &Path) -> Result<std::process::Output, String> {
        let child = Self::checked_command(cmd, args, cwd)
            .await?
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let mut guard = ProcessGroupGuard::for_child(&child)?;
        let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            guard.0 = 0;
        }
        Ok(output)
    }
}

struct TokioCommandProcess {
    child: tokio::process::Child,
}

#[async_trait]
impl CommandProcess for TokioCommandProcess {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        self.child.try_wait().map_err(|error| error.to_string())
    }

    async fn kill(&mut self) -> Result<(), String> {
        self.child.kill().await.map_err(|error| error.to_string())
    }

    async fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
        self.child.wait().await.map_err(|error| error.to_string())
    }
}

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        let output = Self::command_output(cmd, args, cwd).await?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        let output = Self::command_output(cmd, args, cwd).await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        })
    }

    async fn spawn_long_lived(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        _label: &ChannelLabel,
    ) -> Result<Box<dyn CommandProcess>, String> {
        let child = Self::checked_command(cmd, args, cwd)
            .await?
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(Box::new(TokioCommandProcess { child }))
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        use tokio::io::AsyncWriteExt;

        let mut child = Self::checked_command(cmd, args, cwd)
            .await?
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let mut guard = ProcessGroupGuard::for_child(&child)?;
        let mut stdin = child.stdin.take().expect("piped stdin should be available");
        let write_input = async move {
            stdin.write_all(input).await.map_err(|e| e.to_string())?;
            stdin.shutdown().await.map_err(|e| e.to_string())
        };
        let wait_for_output = async move { child.wait_with_output().await.map_err(|e| e.to_string()) };
        let ((), output) = tokio::try_join!(write_input, wait_for_output)?;
        #[cfg(unix)]
        {
            guard.0 = 0;
        }
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        let mut child = Self::checked_command(cmd, args, cwd)
            .await?
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let mut output = tokio::fs::File::create(destination).await.map_err(|error| error.to_string())?;
        tokio::io::copy(&mut child.stdout.take().expect("piped stdout"), &mut output).await.map_err(|error| error.to_string())?;
        let status = child.wait().await.map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{cmd} exited with {status}"))
        }
    }

    async fn run_from_file(&self, cmd: &str, args: &[&str], cwd: &Path, source: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut child = Self::checked_command(cmd, args, cwd)
            .await?
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let mut input = tokio::fs::File::open(source).await.map_err(|error| error.to_string())?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        tokio::io::copy(&mut input, &mut stdin).await.map_err(|error| error.to_string())?;
        stdin.shutdown().await.map_err(|error| error.to_string())?;
        drop(stdin);
        let status = child.wait().await.map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{cmd} exited with {status}"))
        }
    }

    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        tokio::fs::copy(source, destination).await.map(|_| ()).map_err(|error| error.to_string())
    }

    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let temporary = destination.with_extension(format!("flotilla-tmp-{}", uuid::Uuid::new_v4()));
        tokio::fs::copy(source, &temporary).await.map_err(|error| error.to_string())?;
        tokio::fs::rename(&temporary, destination).await.map_err(|error| error.to_string())
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(path).await {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                file.write_all(content.as_bytes()).await.map_err(|e| format!("write {}: {e}", path.display()))?;
                file.flush().await.map_err(|e| format!("flush {}: {e}", path.display()))?;
                drop(file);
                Ok(content.to_owned())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::read_to_string(path).await.map_err(|e| format!("read {}: {e}", path.display()))
            }
            Err(err) => Err(format!("open {}: {err}", path.display())),
        }
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        let temporary = path.with_extension(format!("flotilla-tmp-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary, content).await.map_err(|e| format!("write {}: {e}", temporary.display()))?;
        tokio::fs::rename(&temporary, path).await.map_err(|e| format!("rename {} to {}: {e}", temporary.display(), path.display()))
    }

    #[cfg(unix)]
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;

        if mode > 0o777 {
            return Err("file mode must contain only permission bits".to_string());
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(format!(".flotilla-tmp-{}", uuid::Uuid::new_v4()));
        let temporary = PathBuf::from(temporary);
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .await
                .map_err(|e| format!("open {}: {e}", temporary.display()))?;
            use tokio::io::AsyncWriteExt;
            file.write_all(content.as_bytes()).await.map_err(|e| format!("write {}: {e}", temporary.display()))?;
            file.flush().await.map_err(|e| format!("flush {}: {e}", temporary.display()))?;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .await
                .map_err(|e| format!("protect {}: {e}", temporary.display()))?;
            file.sync_all().await.map_err(|e| format!("sync {}: {e}", temporary.display()))?;
            drop(file);
            tokio::fs::rename(&temporary, path).await.map_err(|e| format!("rename {} to {}: {e}", temporary.display(), path.display()))
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    #[cfg(not(unix))]
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        if mode > 0o777 {
            return Err("file mode must contain only permission bits".to_string());
        }
        // Windows writes inherit directory ACLs; POSIX mode bits cannot enforce
        // the requested protection. Explicit ACL policy belongs here when
        // Windows hosts start materializing daemon credentials (#2468).
        tracing::warn!(mode, path = %path.display(), "writing with inherited ACLs; requested POSIX file permissions are not enforced");
        self.write_file(path, content).await
    }
}

macro_rules! run {
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr, $labeler:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label =
            $crate::providers::command_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(__cmd, __args, &$labeler);
        $runner.run(__cmd, __args, $cwd, &__label).await
    }};
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label = $crate::providers::command_channel_label(__cmd, __args);
        $runner.run(__cmd, __args, $cwd, &__label).await
    }};
}
pub(crate) use run;

macro_rules! run_output {
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr, $labeler:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label =
            $crate::providers::command_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(__cmd, __args, &$labeler);
        $runner.run_output(__cmd, __args, $cwd, &__label).await
    }};
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label = $crate::providers::command_channel_label(__cmd, __args);
        $runner.run_output(__cmd, __args, $cwd, &__label).await
    }};
}
pub(crate) use run_output;

/// Macro that calls `GhApi::get`, auto-deriving the channel label from the endpoint.
macro_rules! gh_api_get {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label =
            $crate::providers::gh_api_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>("GET", __endpoint, &$labeler);
        $api.get(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = $crate::providers::gh_api_channel_label("GET", __endpoint);
        $api.get(__endpoint, $repo_root, &__label).await
    }};
}
pub(crate) use gh_api_get;

/// Macro that calls `GhApi::get_with_headers`, auto-deriving the channel label from the endpoint.
macro_rules! gh_api_get_with_headers {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label =
            $crate::providers::gh_api_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>("GET", __endpoint, &$labeler);
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = $crate::providers::gh_api_channel_label("GET", __endpoint);
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
}
pub(crate) use gh_api_get_with_headers;

/// Macro that calls `HttpClient::execute`, auto-deriving the channel label from the request.
macro_rules! http_execute {
    ($http:expr, $request:expr, $labeler:expr $(,)?) => {{
        let __request = $request;
        let __label = $crate::providers::http_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(
            __request.method().as_str(),
            __request.url().as_str(),
            &$labeler,
        );
        $http.execute(__request, &__label).await
    }};
    ($http:expr, $request:expr $(,)?) => {{
        let __request = $request;
        let __label = $crate::providers::http_channel_label(__request.method().as_str(), __request.url().as_str());
        $http.execute(__request, &__label).await
    }};
}
pub(crate) use http_execute;

/// Trait abstracting HTTP request execution so providers can be tested
/// without making real network calls.
///
/// Uses reqwest::Request as input (callers build with the reqwest builder API)
/// and returns http::Response<bytes::Bytes> (the standard Rust HTTP type that
/// reqwest is built on, trivially constructable in tests).
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String>;

    /// Stream a response body to a file. Test clients may use the buffered
    /// `execute` implementation; production overrides this to bound memory.
    async fn execute_to_file(&self, request: reqwest::Request, label: &ChannelLabel, path: &Path) -> Result<http::StatusCode, String> {
        let response = self.execute(request, label).await?;
        let status = response.status();
        if status.is_success() {
            tokio::fs::write(path, response.body()).await.map_err(|error| error.to_string())?;
        }
        Ok(status)
    }
}

/// Production implementation that delegates to `reqwest::Client`.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let client = crate::tls::client_builder().timeout(REQUEST_TIMEOUT).build().expect("build HTTP client");
        Self { client }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let resp = self.client.execute(request).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(|e| e.to_string())
    }

    async fn execute_to_file(&self, request: reqwest::Request, _label: &ChannelLabel, path: &Path) -> Result<http::StatusCode, String> {
        use tokio::io::AsyncWriteExt;
        let mut response = self.client.execute(request).await.map_err(|error| error.to_string())?;
        let status = response.status();
        if status.is_success() {
            let mut file = tokio::fs::File::create(path).await.map_err(|error| error.to_string())?;
            while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                file.write_all(&chunk).await.map_err(|error| error.to_string())?;
            }
            file.flush().await.map_err(|error| error.to_string())?;
        }
        Ok(status)
    }
}

#[cfg(any(test, feature = "replay"))]
pub mod replay;

#[cfg(test)]
pub(crate) mod testing {
    use std::{
        collections::VecDeque,
        future::Future,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use tracing::instrument::WithSubscriber;

    use super::*;

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log capture lock should be healthy").write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Capture tracing for one future without changing the process-wide subscriber.
    pub async fn capture_logs<F: Future>(level: tracing::Level, future: F) -> (F::Output, String) {
        let log_output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(level)
            .with_writer(move || writer.clone())
            .finish();

        let output = future.with_subscriber(subscriber).await;
        let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
        (output, logs)
    }

    pub type TimeoutCall = (String, Vec<String>, PathBuf, Duration);

    /// Stub that proves decorators use the timeout seam rather than `run`.
    pub struct TimeoutOnlyRunner {
        pub calls: std::sync::Mutex<Vec<TimeoutCall>>,
        result: Result<String, String>,
    }

    impl TimeoutOnlyRunner {
        pub fn new(result: Result<String, String>) -> Self {
            Self { calls: std::sync::Mutex::new(Vec::new()), result }
        }
    }

    #[async_trait]
    impl CommandRunner for TimeoutOnlyRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            panic!("run called instead of run_with_timeout")
        }

        async fn run_with_timeout(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            _label: &ChannelLabel,
            timeout: Duration,
        ) -> Result<String, String> {
            self.calls.lock().expect("calls mutex").push((
                cmd.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                cwd.to_path_buf(),
                timeout,
            ));
            self.result.clone()
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            panic!("run_output not expected")
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    /// A mock command runner that returns canned responses in order.
    /// Each call to `run()` or `run_output()` pops the next response from the queue.
    pub struct MockRunner {
        responses: std::sync::Mutex<VecDeque<Result<CommandOutput, String>>>,
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        pub fn new(responses: Vec<Result<String, String>>) -> Self {
            Self::with_outputs(
                responses
                    .into_iter()
                    .map(|response| {
                        Ok(match response {
                            Ok(stdout) => CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) },
                            Err(stderr) => CommandOutput { stdout: String::new(), stderr, exit_code: Some(1) },
                        })
                    })
                    .collect(),
            )
        }

        /// Queue raw subprocess outputs or spawn errors, preserving both output streams.
        /// `new` retains its legacy stderr-only unsuccessful-command semantics.
        pub fn with_outputs(responses: Vec<Result<CommandOutput, String>>) -> Self {
            Self { responses: std::sync::Mutex::new(responses.into()), calls: std::sync::Mutex::new(vec![]) }
        }

        /// Returns the number of unconsumed canned responses.
        pub fn remaining(&self) -> usize {
            self.responses.lock().expect("MockRunner responses mutex not poisoned").len()
        }

        /// Returns a snapshot of all recorded (cmd, args) calls made so far.
        pub fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let output = self.run_output(cmd, args, cwd, label).await?;
            if output.success() {
                Ok(output.stdout)
            } else {
                Err(output.stderr)
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|a| (*a).into()).collect()));
            self.responses.lock().expect("responses").pop_front().expect("MockRunner: no more responses")
        }

        async fn run_with_input(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
            _input: &[u8],
        ) -> Result<String, String> {
            self.run(cmd, args, cwd, label).await
        }

        async fn run_to_file(&self, cmd: &str, args: &[&str], _cwd: &Path, _destination: &Path) -> Result<(), String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
            Ok(())
        }

        async fn run_from_file(&self, cmd: &str, args: &[&str], _cwd: &Path, _source: &Path) -> Result<(), String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
            Ok(())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }

        async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), String> {
            Ok(())
        }
    }

    // #2586: the subprocess fake preserves both streams and distinguishes exit failure
    // from spawn failure. Glue: explicit rows cover success, failure, empty output, and spawn errors.
    #[tokio::test]
    async fn mock_runner_preserves_outputs_and_order() {
        let runner = MockRunner::with_outputs(vec![
            Ok(CommandOutput { stdout: "headers/body".into(), stderr: "exit failure".into(), exit_code: Some(1) }),
            Err("spawn failure".into()),
            Ok(CommandOutput { stdout: "success".into(), stderr: "warning".into(), exit_code: Some(0) }),
            Ok(CommandOutput { stdout: "success".into(), stderr: "warning".into(), exit_code: Some(0) }),
            Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(1) }),
        ]);
        let label = ChannelLabel::Default;
        // Exit failure still exposes both streams through the raw-output API.
        let output = runner.run_output("gh", &["api"], Path::new("/"), &label).await.expect("output");
        assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("headers/body", "exit failure", false));
        // A spawn failure has no command output.
        assert_eq!(runner.run_output("missing", &[], Path::new("/"), &label).await.err().as_deref(), Some("spawn failure"));
        // Successful raw output also preserves stderr.
        let output = runner.run_output("raw-ok", &[], Path::new("/"), &label).await.expect("successful output");
        assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("success", "warning", true));
        // The convenience API selects stdout on success, stderr on failure.
        assert_eq!(runner.run("ok", &[], Path::new("/"), &label).await, Ok("success".into()));
        assert_eq!(runner.run("empty", &[], Path::new("/"), &label).await, Err(String::new()));
        assert_eq!(runner.remaining(), 0);
        assert_eq!(runner.calls(), vec![
            ("gh".into(), vec!["api".into()]),
            ("missing".into(), vec![]),
            ("raw-ok".into(), vec![]),
            ("ok".into(), vec![]),
            ("empty".into(), vec![])
        ]);
        let legacy = MockRunner::new(vec![Ok("legacy success".into()), Err("legacy failure".into())]);
        assert_eq!(legacy.run("ok", &[], Path::new("/"), &label).await, Ok("legacy success".into()));
        let output = legacy.run_output("fail", &[], Path::new("/"), &label).await.expect("legacy output");
        assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("", "legacy failure", false));
    }

    // Exhausting the subprocess queue remains a hard failure, even for raw output calls.
    #[tokio::test]
    #[should_panic(expected = "MockRunner: no more responses")]
    async fn mock_runner_rejects_unexpected_command() {
        MockRunner::with_outputs(vec![]).run_output("unexpected", &[], Path::new("/"), &ChannelLabel::Default).await.ok();
    }

    /// Build the path to a provider fixture file.
    ///
    /// `provider_dir` is the subdirectory under `src/providers/` (e.g. `"vcs"`, `"change_request"`).
    pub fn fixture_path(provider_dir: &str, name: &str) -> String {
        format!("{}/src/providers/{}/fixtures/{}", env!("CARGO_MANIFEST_DIR"), provider_dir, name)
    }

    #[tokio::test]
    async fn process_runner_ensure_file_creates_parents_and_writes_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/config.toml");
        let runner = super::ProcessCommandRunner;
        let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
        let on_disk = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(ensured, "hello = true\n");
        assert_eq!(on_disk, "hello = true\n");
    }

    #[tokio::test]
    async fn process_runner_ensure_file_preserves_existing_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/config.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "existing = true\n").expect("seed file");

        let runner = super::ProcessCommandRunner;
        let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
        let on_disk = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(ensured, "existing = true\n");
        assert_eq!(on_disk, "existing = true\n");
    }

    #[tokio::test]
    async fn process_runner_write_file_replaces_existing_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/brief.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "old brief").expect("seed file");

        let runner = super::ProcessCommandRunner;
        runner.write_file(&path, "new secret brief").await.expect("write_file");

        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "new secret brief");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_runner_publishes_secret_with_requested_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/token");
        let runner = super::ProcessCommandRunner;
        runner.write_file_with_mode(&path, "secret", 0o600).await.expect("write secret");
        assert_eq!(std::fs::read_to_string(&path).expect("read token"), "secret");
        assert_eq!(std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn process_runner_run_with_input_drains_output_while_writing_stdin() {
        let runner = super::ProcessCommandRunner;
        let input = vec![b'x'; 131_072];

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            runner.run_with_input(
                "sh",
                &["-c", "dd if=/dev/zero bs=131072 count=1 2>/dev/null; cat >/dev/null"],
                Path::new("/"),
                &ChannelLabel::Default,
                &input,
            ),
        )
        .await
        .expect("stdin writing and output draining must not deadlock")
        .expect("child command");

        assert_eq!(output.len(), 131_072);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_command_kills_its_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("child.pid");
        let pid_path_arg = pid_path.to_string_lossy();
        let runner = super::ProcessCommandRunner;
        let args = ["-c", "sleep 30 & echo $! > \"$1\"; wait", "sh", &pid_path_arg];
        let command = runner.run_with_timeout("sh", &args, Path::new("/"), &ChannelLabel::Default, std::time::Duration::from_secs(1));
        let result = command.await;
        assert!(result.expect_err("command must time out").contains("timed out"));
        let pid: i32 = std::fs::read_to_string(&pid_path).expect("child pid").trim().parse().expect("numeric pid");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                #[cfg(target_os = "linux")]
                let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| stat.rsplit_once(')').and_then(|(_, tail)| tail.trim().chars().next()))
                    == Some('Z');
                #[cfg(not(target_os = "linux"))]
                let zombie = false;
                if !alive || zombie {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("grandchild must terminate");
    }

    #[tokio::test]
    async fn process_runner_long_lived_process_supports_lifecycle_contract() {
        let runner = super::ProcessCommandRunner;
        let mut process = runner.spawn_long_lived("sleep", &["30"], Path::new("/"), &ChannelLabel::Default).await.expect("spawn sleep");

        assert!(process.try_wait().expect("poll running child").is_none());
        process.kill().await.expect("kill child");
        let status = process.wait().await.expect("reap killed child");
        assert!(!status.success());
        assert!(process.try_wait().expect("poll reaped child").is_some());
    }

    #[tokio::test]
    async fn process_runner_long_lived_process_drop_does_not_leak_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_path = dir.path().join("child.pid");
        let pid_path_arg = pid_path.to_string_lossy();
        let runner = super::ProcessCommandRunner;
        let process = runner
            .spawn_long_lived("sh", &["-c", "echo $$ > \"$1\"; exec sleep 30", "sh", &pid_path_arg], Path::new("/"), &ChannelLabel::Default)
            .await
            .expect("spawn tracked sleep");

        let pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&pid_path).await {
                    let pid = contents.trim();
                    if !pid.is_empty() {
                        break pid.to_string();
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child should publish pid");

        drop(process);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = tokio::process::Command::new("kill")
                    .args(["-0", &pid])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .expect("poll child pid");
                if !status.success() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped process must terminate its child");
    }
}

#[cfg(test)]
pub(crate) mod github_test_support {
    use std::{path::PathBuf, sync::Arc};

    use crate::providers::{github_api::GhApi, replay, CommandRunner};

    pub fn repo_root_for_recording() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CARGO_MANIFEST_DIR should have a parent")
            .parent()
            .expect("CARGO_MANIFEST_DIR should have a grandparent")
            .to_path_buf()
    }

    pub fn build_api_and_runner(session: &replay::Session) -> (Arc<dyn GhApi>, Arc<dyn CommandRunner>) {
        let runner = replay::test_runner(session);
        let api = replay::test_gh_api(session);
        (api, runner)
    }
}

#[cfg(test)]
mod tests {
    // HTTP audit (#1512): ReqwestHttpClient is service-neutral transport, including
    // execute_to_file. Service rules belong to its callers' contract/replay tests;
    // it has no separate remote service contract or fixture. TLS User-Agent wire
    // coverage lives in flotilla-resources/src/tls.rs.
    use std::path::PathBuf;

    use super::*;

    struct PanicLabeler;

    impl ChannelLabeler for PanicLabeler {
        fn label_for(&self, _request: &ChannelRequest) -> ChannelLabel {
            panic!("labeler should not be called");
        }
    }

    #[tokio::test]
    async fn process_runner_echo() {
        let runner = ProcessCommandRunner;
        let result = run!(runner, "echo", &["hello"], &PathBuf::from("/"));
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn process_runner_exists_true() {
        let runner = ProcessCommandRunner;
        assert!(runner.exists("echo", &["test"]).await);
    }

    #[tokio::test]
    async fn process_runner_exists_false() {
        let runner = ProcessCommandRunner;
        assert!(!runner.exists("nonexistent-binary-xyz", &[]).await);
    }

    #[tokio::test]
    async fn process_runner_transfers_binary_files_without_text_conversion() {
        let directory = tempfile::tempdir().expect("transfer directory");
        let source = directory.path().join("source");
        let copied = directory.path().join("copied");
        let streamed = directory.path().join("streamed");
        let returned = directory.path().join("returned");
        let bytes = [0_u8, 1, 127, 128, 255];
        tokio::fs::write(&source, bytes).await.expect("source bytes");
        let runner = ProcessCommandRunner;
        runner.read_file_to(&source, &copied).await.expect("read host file");
        runner.write_file_from(&copied, &returned).await.expect("write host file");
        runner.run_to_file("cat", &[source.to_str().expect("utf8 path")], Path::new("/"), &streamed).await.expect("stream stdout");
        assert_eq!(tokio::fs::read(&streamed).await.expect("streamed bytes"), bytes);
        assert_eq!(tokio::fs::read(&returned).await.expect("returned bytes"), bytes);
        let piped = directory.path().join("piped");
        runner
            .run_from_file("sh", &["-c", "cat > \"$1\"", "flotilla-test", piped.to_str().expect("utf8 path")], Path::new("/"), &source)
            .await
            .expect("stream stdin");
        assert_eq!(tokio::fs::read(piped).await.expect("piped bytes"), bytes);
    }

    #[test]
    fn command_label_enabled_uses_labeler() {
        let label = command_channel_label_with::<true, _>("git", &["status"], &TaskId("task"));
        assert_eq!(label, ChannelLabel::Command("task".to_string()));
    }

    #[test]
    fn command_label_disabled_skips_labeler() {
        let label = command_channel_label_with::<false, _>("git", &["status"], &PanicLabeler);
        assert_eq!(label, ChannelLabel::Default);
    }

    #[test]
    fn gh_api_label_enabled_uses_labeler() {
        let label = gh_api_channel_label_with::<true, _>("GET", "repos/a/b/issues", &TaskId("gh"));
        assert_eq!(label, ChannelLabel::GhApi("gh".to_string()));
    }

    #[test]
    fn gh_api_label_disabled_skips_labeler() {
        let label = gh_api_channel_label_with::<false, _>("GET", "repos/a/b/issues", &PanicLabeler);
        assert_eq!(label, ChannelLabel::Default);
    }

    #[test]
    fn http_label_enabled_uses_labeler() {
        let label = http_channel_label_with::<true, _>("POST", "https://api.example.com/v1", &TaskId("http"));
        assert_eq!(label, ChannelLabel::Http("http".to_string()));
    }

    #[test]
    fn http_label_disabled_skips_labeler() {
        let label = http_channel_label_with::<false, _>("POST", "https://api.example.com/v1", &PanicLabeler);
        assert_eq!(label, ChannelLabel::Default);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod http_contract;
