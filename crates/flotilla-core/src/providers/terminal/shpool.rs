use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{arg::Arg, TerminalStatus};

use super::{TerminalEnvVars, TerminalPool, TerminalSession};
use crate::providers::{run, CommandRunner};

pub struct ShpoolTerminalPool {
    runner: Arc<dyn CommandRunner>,
    socket_path: PathBuf,
    config_path: PathBuf,
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
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path.parent().unwrap_or(Path::new(".")).join("config.toml");
        let config_stale = Self::config_needs_update(&config_path);
        let mut daemon_state = Self::detect_daemon_state(Arc::clone(&runner), &socket_path, &config_path).await;

        if daemon_state == ShpoolDaemonState::Stale {
            let pid_path = socket_path.with_file_name("daemonized-shpool.pid");
            if pid_path.exists() {
                let _ = Self::stop_daemon(&socket_path, "shpool").await;
            } else {
                Self::clean_stale_socket(&socket_path);
            }
            daemon_state = ShpoolDaemonState::Missing;
        }

        if config_stale && daemon_state == ShpoolDaemonState::HealthyWithPid {
            // Daemon is alive but config changed. Validate we can persist
            // the new config BEFORE killing the daemon, so a write failure
            // doesn't tear down sessions for nothing.
            let tmp_path = config_path.with_extension("toml.tmp");
            if Self::write_config(&tmp_path) {
                tracing::info!("shpool config changed, restarting daemon");
                if Self::stop_daemon(&socket_path, "shpool").await {
                    if let Err(e) = std::fs::rename(&tmp_path, &config_path) {
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
            Self::write_config(&config_path);
        } else if config_stale && daemon_state == ShpoolDaemonState::InconclusiveWithoutPid {
            tracing::warn!("shpool probe was inconclusive without a pid file; leaving socket and config untouched");
        } else if config_stale {
            // No daemon running, safe to write config directly.
            Self::write_config(&config_path);
        }
        Self::start_daemon(&socket_path, &config_path).await;
        Self { runner, socket_path, config_path }
    }

    /// Sync constructor for tests — skips daemon lifecycle.
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path.parent().unwrap_or(Path::new(".")).join("config.toml");
        Self::write_config(&config_path);
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
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        let result = run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "list", "--json"], Path::new("/"));

        match result {
            Ok(json) => Self::parse_list_json(&json),
            Err(e) => {
                tracing::debug!(err = %e, "shpool list failed (daemon may not be running)");
                Ok(vec![])
            }
        }
    }

    async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &Path) -> Result<(), String> {
        // No-op: shpool creates sessions on first `attach`.
        Ok(())
    }

    fn attach_args(&self, session_name: &str, command: &str, cwd: &Path, env_vars: &TerminalEnvVars) -> Result<Vec<Arg>, String> {
        let mut args = vec![
            Arg::Quoted("shpool".into()),
            Arg::Literal("--socket".into()),
            Arg::Quoted(self.socket_path.display().to_string()),
            Arg::Literal("-c".into()),
            Arg::Quoted(self.config_path.display().to_string()),
            Arg::Literal("attach".into()),
        ];
        if !command.is_empty() || !env_vars.is_empty() {
            let mut cmd_inner: Vec<Arg> = Vec::new();
            if !env_vars.is_empty() {
                cmd_inner.push(Arg::Literal("env".into()));
                for (k, v) in env_vars {
                    cmd_inner.push(Arg::Literal(format!("{k}={}", flotilla_protocol::arg::shell_quote(v))));
                }
            }
            cmd_inner.push(Arg::Literal("${SHELL:-/bin/sh}".into()));
            if !command.is_empty() {
                cmd_inner.push(Arg::Literal("-lic".into()));
                cmd_inner.push(Arg::Quoted(command.into()));
            }
            args.push(Arg::Literal("--cmd".into()));
            args.push(Arg::NestedCommand(cmd_inner));
        }
        args.push(Arg::Literal("--dir".into()));
        args.push(Arg::Quoted(cwd.display().to_string()));
        args.push(Arg::Quoted(session_name.into()));
        Ok(args)
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "kill", session_name], Path::new("/"))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::providers::testing::MockRunner;

    /// Create a ShpoolTerminalPool in a temp dir so config writes succeed.
    fn test_pool(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir for shpool test");
        let socket_path = dir.path().join("shpool.socket");
        let pool = ShpoolTerminalPool::new(runner, socket_path);
        (pool, dir)
    }

    #[test]
    fn write_config_writes_expected_content() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_path = dir.path().join("config.toml");
        assert!(ShpoolTerminalPool::write_config(&config_path));
        let content = std::fs::read_to_string(&config_path).expect("config should have been written");
        assert!(content.contains("prompt_prefix = \"\""));
        assert!(content.contains("TERMINFO"));
        assert!(content.contains("COLORTERM"));
    }

    #[test]
    fn config_needs_update_tracks_staleness() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_path = dir.path().join("config.toml");

        // File doesn't exist → needs update
        assert!(ShpoolTerminalPool::config_needs_update(&config_path));

        // Write config, now it matches → no update needed
        ShpoolTerminalPool::write_config(&config_path);
        assert!(!ShpoolTerminalPool::config_needs_update(&config_path));

        // Modify externally → needs update again
        std::fs::write(&config_path, "stale config").expect("write stale");
        assert!(ShpoolTerminalPool::config_needs_update(&config_path));
    }

    #[test]
    fn parse_list_json_with_flotilla_named_sessions() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/my-feature/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/my-feature/agent/0",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                },
                {
                    "name": "user-manual-session",
                    "started_at_unix_ms": 1709900002000,
                    "status": "Attached"
                }
            ]
        }"#;

        let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(sessions.len(), 2); // user-manual-session filtered out

        assert_eq!(sessions[0].session_name, "flotilla/my-feature/shell/0");
        assert_eq!(sessions[0].status, TerminalStatus::Running);

        assert_eq!(sessions[1].session_name, "flotilla/my-feature/agent/0");
        assert_eq!(sessions[1].status, TerminalStatus::Disconnected);
    }

    #[test]
    fn parse_list_json_with_slashy_branch_names() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/feature/foo/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/feat/deep/nested/agent/1",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                }
            ]
        }"#;

        let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(sessions.len(), 2);

        assert_eq!(sessions[0].session_name, "flotilla/feature/foo/shell/0");
        assert_eq!(sessions[1].session_name, "flotilla/feat/deep/nested/agent/1");
    }

    #[test]
    fn parse_list_json_empty_sessions() {
        let json = r#"{"sessions": []}"#;
        let sessions = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn parse_list_json_invalid_json() {
        assert!(ShpoolTerminalPool::parse_list_json("not json").is_err());
    }

    // --- TerminalPool tests (via session names) ---

    #[tokio::test]
    async fn list_sessions_parses_json() {
        let json = r#"{
            "sessions": [
                {"name": "flotilla/feat/shell/0", "started_at_unix_ms": 1709900000000, "status": "Attached"},
                {"name": "flotilla/feat/agent/0", "started_at_unix_ms": 1709900001000, "status": "Disconnected"},
                {"name": "user-manual", "started_at_unix_ms": 1709900002000, "status": "Attached"}
            ]
        }"#;
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![Ok(json.into())])));

        let sessions = TerminalPool::list_sessions(&pool).await.expect("list sessions");

        assert_eq!(sessions.len(), 2); // user-manual filtered out
        assert_eq!(sessions[0].session_name, "flotilla/feat/shell/0");
        assert_eq!(sessions[0].status, TerminalStatus::Running);
        assert!(sessions[0].command.is_none());
        assert!(sessions[0].working_directory.is_none());
        assert_eq!(sessions[1].session_name, "flotilla/feat/agent/0");
        assert_eq!(sessions[1].status, TerminalStatus::Disconnected);
    }

    #[tokio::test]
    async fn attach_builds_command() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));

        let cmd =
            TerminalPool::attach_command(&pool, "flotilla/feat/shell/0", "bash", Path::new("/home/dev"), &vec![]).await.expect("attach");

        assert!(cmd.contains("shpool"), "should reference shpool binary: {cmd}");
        assert!(cmd.contains("attach"), "should include attach subcommand: {cmd}");
        assert!(cmd.contains("--cmd"), "should include --cmd for non-empty command: {cmd}");
        assert!(cmd.contains("-lic"), "should use login interactive shell: {cmd}");
        assert!(cmd.contains("bash"), "should contain original command: {cmd}");
        assert!(cmd.contains("--dir"), "should include --dir: {cmd}");
        assert!(cmd.contains("/home/dev"), "should include cwd: {cmd}");
        assert!(cmd.contains("'flotilla/feat/shell/0'"), "session name should be last: {cmd}");
    }

    #[tokio::test]
    async fn kill_calls_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let (pool, _dir) = test_pool(runner.clone());

        TerminalPool::kill_session(&pool, "flotilla/feat/shell/0").await.expect("kill session");

        assert_eq!(runner.remaining(), 0, "kill command should have consumed the response");
    }

    // ── attach_args tests ──────────────────────────────────────────

    #[test]
    fn attach_args_with_command_no_env() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let socket = pool.socket_path.display().to_string();
        let config = pool.config_path.display().to_string();
        let args = pool.attach_args("flotilla/feat/shell/0", "bash", Path::new("/home/dev"), &vec![]).expect("attach_args");

        assert_eq!(args, vec![
            Arg::Quoted("shpool".into()),
            Arg::Literal("--socket".into()),
            Arg::Quoted(socket),
            Arg::Literal("-c".into()),
            Arg::Quoted(config),
            Arg::Literal("attach".into()),
            Arg::Literal("--cmd".into()),
            Arg::NestedCommand(vec![Arg::Literal("${SHELL:-/bin/sh}".into()), Arg::Literal("-lic".into()), Arg::Quoted("bash".into()),]),
            Arg::Literal("--dir".into()),
            Arg::Quoted("/home/dev".into()),
            Arg::Quoted("flotilla/feat/shell/0".into()),
        ]);
    }

    #[test]
    fn attach_args_flatten_with_command_no_env() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let args = pool.attach_args("flotilla/feat/shell/0", "bash", Path::new("/home/dev"), &vec![]).expect("attach_args");
        let flat = flotilla_protocol::arg::flatten(&args, 0);

        assert!(flat.starts_with("'shpool' --socket "), "should start with shpool: {flat}");
        assert!(flat.contains("attach"), "should include attach: {flat}");
        assert!(flat.contains("--cmd"), "should include --cmd: {flat}");
        assert!(flat.contains("-lic"), "should use login interactive shell: {flat}");
        assert!(flat.contains("bash"), "should contain original command: {flat}");
        assert!(flat.contains("--dir"), "should include --dir: {flat}");
        assert!(flat.contains("'/home/dev'"), "should include cwd: {flat}");
        assert!(flat.ends_with("'flotilla/feat/shell/0'"), "session name should be last: {flat}");
    }

    #[test]
    fn attach_args_empty_command_no_env() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let args = pool.attach_args("sess", "", Path::new("/wd"), &vec![]).expect("attach_args");

        // No --cmd when both command and env_vars are empty
        assert!(!args.iter().any(|a| matches!(a, Arg::Literal(s) if s == "--cmd")), "no --cmd for empty command+env");
        // Should end with --dir, quoted cwd, quoted session name
        let len = args.len();
        assert_eq!(args[len - 3], Arg::Literal("--dir".into()));
        assert_eq!(args[len - 2], Arg::Quoted("/wd".into()));
        assert_eq!(args[len - 1], Arg::Quoted("sess".into()));
    }

    #[test]
    fn attach_args_with_env_vars() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = pool.attach_args("sess", "cmd", Path::new("/wd"), &env).expect("attach_args");

        // Verify the inner command structure via the NestedCommand
        let nested = args.iter().find(|a| matches!(a, Arg::NestedCommand(_)));
        assert!(nested.is_some(), "should have NestedCommand for --cmd");
        if let Some(Arg::NestedCommand(inner)) = nested {
            let inner_flat = flotilla_protocol::arg::flatten(inner, 0);
            assert!(inner_flat.contains("FOO='bar'"), "inner should contain env assignment: {inner_flat}");
            assert!(inner_flat.contains("${SHELL:-/bin/sh}"), "inner should reference $SHELL: {inner_flat}");
            assert!(inner_flat.contains("-lic"), "inner should have -lic: {inner_flat}");
        }
    }

    #[test]
    fn attach_args_with_env_vars_empty_command() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let env = vec![("KEY".to_string(), "val".to_string())];
        let args = pool.attach_args("sess", "", Path::new("/wd"), &env).expect("attach_args");

        // Should have --cmd with env prefix and $SHELL but no -lic
        let nested = args.iter().find(|a| matches!(a, Arg::NestedCommand(_)));
        assert!(nested.is_some(), "should have NestedCommand for --cmd");
        if let Some(Arg::NestedCommand(inner)) = nested {
            let inner_flat = flotilla_protocol::arg::flatten(inner, 0);
            assert!(inner_flat.contains("env KEY='val'"), "inner should contain env: {inner_flat}");
            assert!(inner_flat.contains("${SHELL:-/bin/sh}"), "inner should contain $SHELL: {inner_flat}");
            assert!(!inner_flat.contains("-lic"), "inner should not have -lic for empty command: {inner_flat}");
        }
    }
}
