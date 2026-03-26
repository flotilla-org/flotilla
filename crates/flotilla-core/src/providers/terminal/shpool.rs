use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, TerminalStatus};

use super::{TerminalEnvVars, TerminalPool, TerminalSession};
use crate::{
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{run, run_output, CommandRunner},
};

pub struct ShpoolTerminalPool {
    runner: Arc<dyn CommandRunner>,
    socket_path: DaemonHostPath,
    config_path: DaemonHostPath,
}

/// Shpool config content managed by flotilla.
/// Disables prompt prefix (flotilla manages its own UI) and forwards
/// terminal environment variables that would otherwise be lost when
/// the shpool daemon spawns shells outside the terminal emulator.
/// Note: `forward_env` only takes effect when creating new sessions,
/// not when reattaching to existing ones (shpool limitation).
const FLOTILLA_SHPOOL_CONFIG: &str = include_str!("shpool_config.toml");
const SHPOOL_DAEMON_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShpoolDaemonState {
    Missing,
    HealthyWithPid,
    HealthyWithoutPid,
    InconclusiveWithoutPid,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShpoolNoPidProbe {
    Healthy,
    Inconclusive,
    Stale,
}

impl ShpoolTerminalPool {
    /// Create a new ShpoolTerminalPool, cleaning up stale sockets and
    /// spawning the daemon with flotilla's managed config.
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        let config_stale = Self::config_needs_update(config_path.as_path());
        let mut daemon_state = Self::detect_daemon_state(Arc::clone(&runner), socket_path.as_path(), config_path.as_path()).await;

        if daemon_state == ShpoolDaemonState::Stale {
            let pid_path = socket_path.as_path().with_file_name("daemonized-shpool.pid");
            if pid_path.exists() {
                let _ = Self::stop_daemon(socket_path.as_path(), "shpool").await;
            } else {
                Self::clean_stale_socket(socket_path.as_path());
            }
            daemon_state = ShpoolDaemonState::Missing;
        }

        if config_stale && daemon_state == ShpoolDaemonState::HealthyWithPid {
            // Daemon is alive but config changed. Validate we can persist
            // the new config BEFORE killing the daemon, so a write failure
            // doesn't tear down sessions for nothing.
            let tmp_path = config_path.as_path().with_extension("toml.tmp");
            if Self::write_config(&tmp_path) {
                tracing::info!("shpool config changed, restarting daemon");
                if Self::stop_daemon(socket_path.as_path(), "shpool").await {
                    if let Err(e) = std::fs::rename(&tmp_path, config_path.as_path()) {
                        tracing::warn!(err = %e, "failed to rename config, cleaning up temp");
                        let _ = std::fs::remove_file(&tmp_path);
                    }
                } else {
                    // Stop failed — delete temp, old config stays for retry.
                    let _ = std::fs::remove_file(&tmp_path);
                }
            }
        } else if config_stale && daemon_state == ShpoolDaemonState::HealthyWithoutPid {
            tracing::warn!("shpool config changed but daemon has no pid file; writing config for future restart");
            Self::write_config(config_path.as_path());
        } else if config_stale && daemon_state == ShpoolDaemonState::InconclusiveWithoutPid {
            tracing::warn!("shpool probe was inconclusive without a pid file; leaving socket and config untouched");
        } else if config_stale {
            // No daemon running, safe to write config directly.
            Self::write_config(config_path.as_path());
        }
        Self::start_daemon(socket_path.as_path(), config_path.as_path()).await;
        Self { runner, socket_path, config_path }
    }

    /// Sync constructor for tests — skips daemon lifecycle.
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        Self::write_config(config_path.as_path());
        Self { runner, socket_path, config_path }
    }

    /// Check if a process is alive. Returns true for both "alive and ours"
    /// (kill returns 0) and "alive but not ours" (EPERM).
    #[cfg(unix)]
    fn is_process_alive(pid: i32) -> bool {
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        // EPERM means the process exists but we can't signal it
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Verify that the process at `pid` has a name containing `expected_name`.
    /// Used to guard against PID reuse: if shpool died uncleanly and another
    /// process got the same PID, we must not SIGTERM it.
    #[cfg(unix)]
    fn is_expected_process(pid: i32, expected_name: &str) -> bool {
        use sysinfo::{Pid, System};
        let mut sys = System::new();
        let sysinfo_pid = Pid::from(pid as usize);
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]), true, sysinfo::ProcessRefreshKind::nothing());
        sys.process(sysinfo_pid).map(|p| p.name().to_string_lossy().contains(expected_name)).unwrap_or(false)
    }

    /// Remove stale shpool socket and pid files when the daemon is dead.
    ///
    /// On macOS, `connect()` to a stale Unix socket succeeds (unlike Linux
    /// where it returns ConnectionRefused), causing shpool's auto-daemonize
    /// to think a daemon is running when it isn't.
    #[cfg(unix)]
    fn clean_stale_socket(socket_path: &Path) {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        if !socket_path.exists() {
            return;
        }

        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                if let Some(pid) = contents.trim().parse::<i32>().ok().filter(|&p| p > 0) {
                    if Self::is_process_alive(pid) {
                        tracing::debug!(%pid, "shpool daemon is alive, keeping socket");
                        return;
                    }
                    tracing::info!(%pid, "shpool daemon is dead, removing stale socket");
                }
                // PID file exists but daemon is dead (or unparseable) — remove both
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
            }
            Err(_) => {
                // No pid file but socket exists — can't verify liveness, remove it
                tracing::info!("no pid file found, removing orphaned shpool socket");
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }

    #[cfg(not(unix))]
    fn clean_stale_socket(_socket_path: &Path) {
        // Unix sockets don't exist on non-Unix platforms
    }

    #[cfg(unix)]
    async fn detect_daemon_state(runner: Arc<dyn CommandRunner>, socket_path: &Path, config_path: &Path) -> ShpoolDaemonState {
        if !socket_path.exists() {
            return ShpoolDaemonState::Missing;
        }

        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");
        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                let Some(pid) = contents.trim().parse::<i32>().ok().filter(|&p| p > 0) else {
                    return ShpoolDaemonState::Stale;
                };
                if Self::is_process_alive(pid) && Self::is_expected_process(pid, "shpool") {
                    ShpoolDaemonState::HealthyWithPid
                } else {
                    ShpoolDaemonState::Stale
                }
            }
            Err(_) => match Self::probe_daemon_without_pid_file(runner, socket_path, config_path).await {
                ShpoolNoPidProbe::Healthy => ShpoolDaemonState::HealthyWithoutPid,
                ShpoolNoPidProbe::Inconclusive => ShpoolDaemonState::InconclusiveWithoutPid,
                ShpoolNoPidProbe::Stale => ShpoolDaemonState::Stale,
            },
        }
    }

    #[cfg(not(unix))]
    async fn detect_daemon_state(_runner: Arc<dyn CommandRunner>, socket_path: &Path, _config_path: &Path) -> ShpoolDaemonState {
        if socket_path.exists() {
            ShpoolDaemonState::HealthyWithoutPid
        } else {
            ShpoolDaemonState::Missing
        }
    }

    #[cfg(unix)]
    async fn probe_daemon_without_pid_file(runner: Arc<dyn CommandRunner>, socket_path: &Path, config_path: &Path) -> ShpoolNoPidProbe {
        let socket_path_str = socket_path.display().to_string();
        let config_path_str = config_path.display().to_string();
        let args = ["--no-daemonize", "--socket", &socket_path_str, "-c", &config_path_str, "list", "--json"];
        let label = crate::providers::command_channel_label("shpool", &args);

        match tokio::time::timeout(SHPOOL_DAEMON_PROBE_TIMEOUT, runner.run_output("shpool", &args, Path::new("/"), &label)).await {
            Ok(Ok(output)) if output.success => ShpoolNoPidProbe::Healthy,
            Ok(Ok(output)) => {
                if Self::probe_failure_is_definitely_stale(&output.stderr) {
                    ShpoolNoPidProbe::Stale
                } else {
                    ShpoolNoPidProbe::Inconclusive
                }
            }
            Ok(Err(err)) => {
                if Self::probe_failure_is_definitely_stale(&err) {
                    ShpoolNoPidProbe::Stale
                } else {
                    ShpoolNoPidProbe::Inconclusive
                }
            }
            Err(_) => ShpoolNoPidProbe::Inconclusive,
        }
    }

    #[cfg(not(unix))]
    async fn probe_daemon_without_pid_file(_runner: Arc<dyn CommandRunner>, _socket_path: &Path, _config_path: &Path) -> ShpoolNoPidProbe {
        ShpoolNoPidProbe::Inconclusive
    }

    fn probe_failure_is_definitely_stale(message: &str) -> bool {
        let lower = message.to_ascii_lowercase();
        lower.contains("connection refused")
            || lower.contains("no such file")
            || lower.contains("not found")
            || lower.contains("enoent")
            || lower.contains("does not exist")
    }

    /// Spawn the shpool daemon if one isn't already running.
    ///
    /// `clean_stale_socket` must be called first — it removes sockets for dead
    /// daemons, so if the socket still exists here the daemon is alive and we
    /// can reuse it. If the spawn fails, logs a warning — shpool's built-in
    /// auto-daemonize from `attach` will still work as a fallback.
    #[cfg(unix)]
    async fn start_daemon(socket_path: &Path, config_path: &Path) {
        // If socket exists after clean_stale_socket, a live daemon is already
        // running — reuse it rather than tearing down persistent sessions.
        if socket_path.exists() {
            tracing::debug!("shpool daemon already running, reusing existing");
            return;
        }

        // Ensure the parent directory exists before creating the socket,
        // log file, or pid file.
        if let Some(parent) = socket_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool state dir");
                return;
            }
        }

        let socket_str = socket_path.display().to_string();
        let config_str = config_path.display().to_string();
        let log_path = socket_path.with_file_name("daemonized-shpool.log");
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        match std::fs::File::create(&log_path) {
            Ok(log_file) => {
                // Clone for stderr before consuming for stdout
                let log_stderr = match log_file.try_clone() {
                    Ok(f) => f.into(),
                    Err(_) => std::process::Stdio::null(),
                };
                let result = tokio::process::Command::new("shpool")
                    .args(["--socket", &socket_str, "-c", &config_str, "daemon"])
                    .stdin(std::process::Stdio::null())
                    .stdout(log_file)
                    .stderr(log_stderr)
                    .spawn();

                match result {
                    Ok(child) => {
                        if let Some(pid) = child.id() {
                            if let Err(e) = std::fs::write(&pid_path, pid.to_string()) {
                                tracing::warn!(err = %e, path = %pid_path.display(), "failed to write shpool pid file");
                            }
                        } else {
                            tracing::warn!("shpool daemon spawned without a pid");
                        }
                        // Child handle is intentionally dropped — tokio does not
                        // kill on drop, so the daemon outlives this handle.
                        tracing::info!("spawned shpool daemon");
                        // Wait for socket to appear (up to 2s)
                        for _ in 0..20 {
                            if socket_path.exists() {
                                tracing::debug!("shpool socket is ready");
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        tracing::warn!("shpool socket did not appear within 2s");
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to spawn shpool daemon");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to create shpool log file");
            }
        }
    }

    #[cfg(not(unix))]
    async fn start_daemon(_socket_path: &Path, _config_path: &Path) {
        // shpool is Unix-only
    }

    /// Gracefully stop a running shpool daemon by sending SIGTERM and
    /// waiting for it to exit. Removes the socket and pid files afterward.
    /// This is load-bearing: `start_daemon()` checks socket existence as
    /// its first guard, so the socket must be gone for a replacement to spawn.
    /// Returns true if the daemon was stopped (or was already dead),
    /// false if it's still alive. `expected_name` is checked against the
    /// process name to guard against PID reuse.
    #[cfg(unix)]
    async fn stop_daemon(socket_path: &Path, expected_name: &str) -> bool {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        // Read and parse the pid — if we can't, just clean up files
        let pid = match std::fs::read_to_string(&pid_path) {
            Ok(contents) => match contents.trim().parse::<i32>().ok().filter(|&p| p > 0) {
                Some(pid) => pid,
                None => {
                    tracing::warn!("shpool pid file unparseable, removing socket");
                    let _ = std::fs::remove_file(socket_path);
                    let _ = std::fs::remove_file(&pid_path);
                    return true;
                }
            },
            Err(_) => {
                tracing::warn!("no shpool pid file found, removing socket");
                let _ = std::fs::remove_file(socket_path);
                return true;
            }
        };

        // Guard against PID reuse: if shpool died uncleanly and another
        // process got the same PID, we must not SIGTERM it.
        if !Self::is_expected_process(pid, expected_name) {
            tracing::warn!(%pid, "pid file references non-shpool process, removing stale artifacts");
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(&pid_path);
            return true;
        }

        // Send SIGTERM
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            // ESRCH = process already dead, safe to clean up.
            // EPERM = alive but can't signal — leave socket so start_daemon reuses it.
            if !Self::is_process_alive(pid) {
                tracing::debug!(%pid, "shpool daemon already dead, cleaning up");
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
                return true;
            }
            tracing::warn!(%pid, "cannot signal shpool daemon, keeping existing");
            return false;
        }

        // Wait for process to exit (up to 2s).
        // If the daemon was started in this same process (same-session config
        // restart), it becomes a zombie after SIGTERM until reaped. waitpid
        // with WNOHANG reaps it so is_process_alive sees it as dead. If the
        // daemon is not our child, waitpid returns ECHILD which we ignore.
        for _ in 0..20 {
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if !Self::is_process_alive(pid) {
                tracing::debug!(%pid, "shpool daemon exited after SIGTERM");
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Daemon still alive after timeout — leave socket in place so
        // start_daemon() reuses the existing daemon rather than racing
        // a second one against it.
        tracing::warn!(%pid, "shpool daemon did not exit within 2s after SIGTERM, keeping existing");
        false
    }

    #[cfg(not(unix))]
    async fn stop_daemon(_socket_path: &Path, _expected_name: &str) -> bool {
        true
    }

    /// Check whether the config file needs updating (missing or stale).
    fn config_needs_update(path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(existing) => existing != FLOTILLA_SHPOOL_CONFIG,
            Err(_) => true,
        }
    }

    /// Write the flotilla-managed shpool config. Returns true on success.
    fn write_config(path: &Path) -> bool {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool config dir");
                return false;
            }
        }
        if let Err(e) = std::fs::write(path, FLOTILLA_SHPOOL_CONFIG) {
            tracing::warn!(path = %path.display(), err = %e, "failed to write shpool config");
            return false;
        }
        true
    }

    /// Check whether a session with the given name exists in the shpool daemon.
    /// Unlike `list_sessions()`, this checks ALL sessions (no `flotilla/` prefix filter).
    async fn session_exists(&self, session_name: &str) -> bool {
        let socket_str = self.socket_path.as_path().display().to_string();
        let config_str = self.config_path.as_path().display().to_string();
        let result = run!(self.runner, "shpool", &["--socket", &socket_str, "-c", &config_str, "list", "--json"], Path::new("/"));
        match result {
            Ok(json) => {
                let parsed: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                parsed["sessions"]
                    .as_array()
                    .map(|sessions| sessions.iter().any(|s| s["name"].as_str() == Some(session_name)))
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    /// Parse the JSON output of `shpool list --json`.
    fn parse_list_json(json: &str) -> Result<Vec<TerminalSession>, String> {
        let parsed: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("failed to parse shpool list: {e}"))?;

        let sessions = parsed["sessions"].as_array().ok_or("shpool list: no sessions array")?;

        let mut terminals = Vec::new();
        for session in sessions {
            let name = session["name"].as_str().ok_or("shpool session missing name")?;

            // Only show flotilla-managed sessions (prefixed "flotilla/")
            if !name.starts_with("flotilla/") {
                continue;
            }

            // Validate the session name has the expected structure:
            // "flotilla/{checkout}/{role}/{index}"
            let rest = &name["flotilla/".len()..];
            let Some((before_index, index_str)) = rest.rsplit_once('/') else {
                continue;
            };
            if before_index.rsplit_once('/').is_none() {
                continue;
            }
            // Validate index is parseable
            if index_str.parse::<u32>().is_err() {
                tracing::warn!(session = name, index = index_str, "failed to parse managed terminal index, skipping");
                continue;
            }

            let status_str = session["status"].as_str().unwrap_or("").to_ascii_lowercase();
            let status = match status_str.as_str() {
                "attached" => TerminalStatus::Running,
                "disconnected" => TerminalStatus::Disconnected,
                _ => TerminalStatus::Disconnected,
            };

            terminals.push(TerminalSession {
                session_name: name.to_string(),
                status,
                command: None,           // shpool doesn't report the original command
                working_directory: None, // shpool doesn't report cwd
            });
        }

        Ok(terminals)
    }
}

#[async_trait]
impl TerminalPool for ShpoolTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        let socket_path_str = self.socket_path.as_path().display().to_string();
        let config_path_str = self.config_path.as_path().display().to_string();
        let result = run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "list", "--json"], Path::new("/"));

        match result {
            Ok(json) => Self::parse_list_json(&json),
            Err(e) => {
                tracing::debug!(err = %e, "shpool list failed (daemon may not be running)");
                Ok(vec![])
            }
        }
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<(), String> {
        // If the session already exists, leave it alone — don't disrupt a
        // live attach by re-running attach+detach. We can't use list_sessions()
        // here because it filters to flotilla/-prefixed names, but session
        // names are UUIDs (attachable IDs). Query the raw shpool list instead.
        if self.session_exists(session_name).await {
            return Ok(());
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        // Build --cmd value. shpool uses shell-words for tokenization (no
        // variable expansion), so all values must be literal. Quote values
        // so paths with spaces are handled correctly.
        //
        // Inject TERM/COLORTERM defaults when the daemon process doesn't have
        // them (common for remote daemons started via non-interactive SSH).
        // shpool captures env at session creation, not at attach time, so
        // sessions created without TERM get no color support. xterm-256color
        // is the safe universal default (same as tmux/zellij).
        let mut cmd_parts: Vec<String> = Vec::new();
        let needs_env_prefix = !env_vars.is_empty()
            || std::env::var("TERM").unwrap_or_default().is_empty()
            || std::env::var("COLORTERM").unwrap_or_default().is_empty();
        if needs_env_prefix {
            cmd_parts.push("env".to_string());
            if std::env::var("TERM").unwrap_or_default().is_empty() {
                cmd_parts.push("TERM=xterm-256color".to_string());
            }
            if std::env::var("COLORTERM").unwrap_or_default().is_empty() {
                cmd_parts.push("COLORTERM=truecolor".to_string());
            }
            for (k, v) in env_vars {
                cmd_parts.push(format!("{k}={}", flotilla_protocol::arg::shell_quote(v)));
            }
        }
        cmd_parts.push(flotilla_protocol::arg::shell_quote(&shell));
        if !command.is_empty() {
            cmd_parts.push("-lic".to_string());
            cmd_parts.push(flotilla_protocol::arg::shell_quote(command));
        }
        let cmd_str = cmd_parts.join(" ");

        let socket_str = self.socket_path.as_path().display().to_string();
        let config_str = self.config_path.as_path().display().to_string();
        let cwd_str = cwd.as_path().display().to_string();

        // Create the session by attaching. Without a TTY, shpool creates the
        // session and the attach process exits on its own with a non-zero
        // status. We use run_output! to tolerate non-zero exit and only
        // fail if the process couldn't be spawned at all.
        let output = run_output!(
            self.runner,
            "shpool",
            &["--socket", &socket_str, "-c", &config_str, "attach", "--cmd", &cmd_str, "--dir", &cwd_str, session_name],
            Path::new("/")
        )?;
        if !output.success {
            tracing::debug!(
                %session_name,
                stderr = %output.stderr,
                "shpool attach exited non-zero (expected without TTY)",
            );
        }

        // Detach for robustness — ensures session is disconnected even if
        // the attach process didn't exit cleanly.
        let _ = run_output!(self.runner, "shpool", &["--socket", &socket_str, "-c", &config_str, "detach", session_name], Path::new("/"));

        Ok(())
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<Arg>, String> {
        Ok(vec![
            Arg::Literal("shpool".into()),
            Arg::Literal("--socket".into()),
            Arg::Quoted(self.socket_path.as_path().display().to_string()),
            Arg::Literal("-c".into()),
            Arg::Quoted(self.config_path.as_path().display().to_string()),
            Arg::Literal("attach".into()),
            Arg::Literal("--force".into()),
            Arg::Literal("--dir".into()),
            Arg::Quoted(cwd.as_path().display().to_string()),
            Arg::Literal(session_name.into()),
        ])
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        let socket_path_str = self.socket_path.as_path().display().to_string();
        let config_path_str = self.config_path.as_path().display().to_string();
        run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "kill", session_name], Path::new("/"))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests;
