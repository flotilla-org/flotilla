use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId, TerminalStatus};

use super::TerminalPool;
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

impl ShpoolTerminalPool {
    /// Create a new ShpoolTerminalPool, cleaning up stale sockets and
    /// spawning the daemon with flotilla's managed config.
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("config.toml");
        Self::ensure_config(&config_path);
        Self::clean_stale_socket(&socket_path);
        Self::start_daemon(&socket_path, &config_path).await;
        Self {
            runner,
            socket_path,
            config_path,
        }
    }

    /// Sync constructor for tests — skips daemon lifecycle.
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: PathBuf) -> Self {
        let config_path = socket_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("config.toml");
        Self::ensure_config(&config_path);
        Self {
            runner,
            socket_path,
            config_path,
        }
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
                if let Ok(pid) = contents.trim().parse::<i32>() {
                    // Signal 0 checks process existence without sending a signal
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    if alive {
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

    /// Spawn the shpool daemon as a background process with flotilla's config.
    ///
    /// If a daemon is already running, kills it first so the new one picks up
    /// the latest config. If the spawn fails, logs a warning — shpool's
    /// built-in auto-daemonize from `attach` will still work as a fallback.
    async fn start_daemon(socket_path: &Path, config_path: &Path) {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        // Kill existing daemon if running (to apply fresh config)
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                #[cfg(unix)]
                if unsafe { libc::kill(pid, 0) } == 0 {
                    tracing::info!(%pid, "killing existing shpool daemon to apply fresh config");
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    // Give it a moment to shut down
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    // Clean up after killed daemon
                    let _ = std::fs::remove_file(socket_path);
                    let _ = std::fs::remove_file(&pid_path);
                }
            }
        }

        let socket_str = socket_path.display().to_string();
        let config_str = config_path.display().to_string();
        let log_path = socket_path.with_file_name("daemonized-shpool.log");

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
                    Ok(_child) => {
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

    /// Write the flotilla-managed shpool config if it doesn't exist or is stale.
    fn ensure_config(path: &Path) {
        let needs_write = match std::fs::read_to_string(path) {
            Ok(existing) => existing != FLOTILLA_SHPOOL_CONFIG,
            Err(_) => true,
        };
        if needs_write {
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool config dir");
                }
            }
            if let Err(e) = std::fs::write(path, FLOTILLA_SHPOOL_CONFIG) {
                tracing::warn!(path = %path.display(), err = %e, "failed to write shpool config");
            }
        }
    }

    /// Parse the JSON output of `shpool list --json`.
    fn parse_list_json(json: &str) -> Result<Vec<ManagedTerminal>, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("failed to parse shpool list: {e}"))?;

        let sessions = parsed["sessions"]
            .as_array()
            .ok_or("shpool list: no sessions array")?;

        let mut terminals = Vec::new();
        for session in sessions {
            let name = session["name"]
                .as_str()
                .ok_or("shpool session missing name")?;

            // Only show flotilla-managed sessions (prefixed "flotilla/")
            let Some(rest) = name.strip_prefix("flotilla/") else {
                continue;
            };

            // Parse "checkout/role/index" from the right — checkout may contain
            // slashes (e.g. "feature/foo"), but role and index never do.
            let Some((before_index, index_str)) = rest.rsplit_once('/') else {
                continue;
            };
            let Some((checkout, role)) = before_index.rsplit_once('/') else {
                continue;
            };
            let index: u32 = index_str.parse().unwrap_or(0);

            let status_str = session["status"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase();
            let status = match status_str.as_str() {
                "attached" => TerminalStatus::Running,
                "disconnected" => TerminalStatus::Disconnected,
                _ => TerminalStatus::Disconnected,
            };

            terminals.push(ManagedTerminal {
                id: ManagedTerminalId {
                    checkout: checkout.into(),
                    role: role.into(),
                    index,
                },
                role: role.into(),
                command: String::new(), // shpool doesn't report the original command
                working_directory: PathBuf::new(), // populated separately if needed
                status,
            });
        }

        Ok(terminals)
    }
}

#[async_trait]
impl TerminalPool for ShpoolTerminalPool {
    fn display_name(&self) -> &str {
        "shpool"
    }

    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        let result = run!(
            self.runner,
            "shpool",
            &[
                "--socket",
                &socket_path_str,
                "-c",
                &config_path_str,
                "list",
                "--json"
            ],
            Path::new("/")
        );

        match result {
            Ok(json) => Self::parse_list_json(&json),
            Err(e) => {
                tracing::debug!(err = %e, "shpool list failed (daemon may not be running)");
                Ok(vec![])
            }
        }
    }

    async fn ensure_running(
        &self,
        _id: &ManagedTerminalId,
        _command: &str,
        _cwd: &Path,
    ) -> Result<(), String> {
        // No-op: shpool creates sessions on first `attach`. The actual session
        // creation happens when the workspace manager runs the attach_command.
        Ok(())
    }

    async fn attach_command(
        &self,
        id: &ManagedTerminalId,
        command: &str,
        cwd: &Path,
    ) -> Result<String, String> {
        let session_name = format!("flotilla/{id}");
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        let cwd_str = cwd.display().to_string();
        fn sq(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        // shpool attach creates the session if it doesn't exist (using --cmd/--dir),
        // or reattaches if it does (ignoring --cmd/--dir).
        // --cmd does a direct exec with no shell environment, so we wrap commands
        // in an interactive login shell to get the full user environment (PATH,
        // node, direnv, aliases, etc). Empty commands omit --cmd, letting shpool
        // use the user's default shell.
        let cmd_part = if command.is_empty() {
            String::new()
        } else {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            // shell-words parses: /bin/zsh -lic 'claude'
            // → ["/bin/zsh", "-lic", "claude"]
            // Interactive login shell resolves aliases and has full PATH.
            let escaped_cmd = command.replace('\'', "'\\''");
            format!(" --cmd {}", sq(&format!("{shell} -lic '{escaped_cmd}'")))
        };
        Ok(format!(
            "shpool --socket {} -c {} attach{} --dir {} {}",
            sq(&socket_path_str),
            sq(&config_path_str),
            cmd_part,
            sq(&cwd_str),
            sq(&session_name),
        ))
    }

    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
        let session_name = format!("flotilla/{id}");
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        run!(
            self.runner,
            "shpool",
            &[
                "--socket",
                &socket_path_str,
                "-c",
                &config_path_str,
                "kill",
                &session_name
            ],
            Path::new("/")
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
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
    fn ensure_config_writes_expected_content() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_path = dir.path().join("config.toml");
        ShpoolTerminalPool::ensure_config(&config_path);
        let content =
            std::fs::read_to_string(&config_path).expect("config should have been written");
        assert!(content.contains("prompt_prefix = \"\""));
        assert!(content.contains("TERMINFO"));
        assert!(content.contains("COLORTERM"));
    }

    #[test]
    fn parse_list_json_with_flotilla_sessions() {
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

        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(terminals.len(), 2); // user-manual-session filtered out

        assert_eq!(terminals[0].id.checkout, "my-feature");
        assert_eq!(terminals[0].id.role, "shell");
        assert_eq!(terminals[0].id.index, 0);
        assert_eq!(terminals[0].status, TerminalStatus::Running);

        assert_eq!(terminals[1].id.checkout, "my-feature");
        assert_eq!(terminals[1].id.role, "agent");
        assert_eq!(terminals[1].status, TerminalStatus::Disconnected);
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

        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(terminals.len(), 2);

        assert_eq!(terminals[0].id.checkout, "feature/foo");
        assert_eq!(terminals[0].id.role, "shell");
        assert_eq!(terminals[0].id.index, 0);

        assert_eq!(terminals[1].id.checkout, "feat/deep/nested");
        assert_eq!(terminals[1].id.role, "agent");
        assert_eq!(terminals[1].id.index, 1);
    }

    #[test]
    fn parse_list_json_empty_sessions() {
        let json = r#"{"sessions": []}"#;
        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert!(terminals.is_empty());
    }

    #[test]
    fn parse_list_json_invalid_json() {
        assert!(ShpoolTerminalPool::parse_list_json("not json").is_err());
    }

    #[tokio::test]
    async fn ensure_running_is_noop() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId {
            checkout: "feat".into(),
            role: "shell".into(),
            index: 0,
        };
        assert!(pool
            .ensure_running(&id, "bash", Path::new("/home/dev"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn attach_command_includes_cmd_dir_and_config() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId {
            checkout: "feat".into(),
            role: "shell".into(),
            index: 0,
        };
        let cmd = pool
            .attach_command(&id, "bash", Path::new("/home/dev"))
            .await
            .unwrap();
        assert!(cmd.contains("shpool"));
        assert!(cmd.contains("attach"));
        assert!(cmd.contains("--cmd"));
        assert!(cmd.contains("-lic"));
        assert!(cmd.contains("bash"));
        assert!(cmd.contains("--dir"));
        assert!(cmd.contains("/home/dev"));
        assert!(cmd.contains("flotilla/feat/shell/0"));
        assert!(cmd.contains("-c"), "should pass config file: {cmd}");
        assert!(
            cmd.contains("config.toml"),
            "should reference config.toml: {cmd}"
        );
    }

    #[tokio::test]
    async fn attach_command_empty_cmd_omits_cmd_flag() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId {
            checkout: "feat".into(),
            role: "shell".into(),
            index: 0,
        };
        let cmd = pool
            .attach_command(&id, "", Path::new("/home/dev"))
            .await
            .unwrap();
        assert!(cmd.contains("shpool"));
        assert!(cmd.contains("attach"));
        assert!(!cmd.contains("--cmd"));
        assert!(cmd.contains("--dir"));
        assert!(cmd.contains("-c"));
    }

    #[tokio::test]
    async fn list_terminals_returns_empty_when_daemon_not_running() {
        let (pool, _dir) = test_pool(Arc::new(MockRunner::new(vec![Err(
            "connection refused".into()
        )])));
        let terminals = pool.list_terminals().await.unwrap();
        assert!(terminals.is_empty());
    }

    #[test]
    fn clean_stale_socket_removes_dead_pid_artifacts() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // Create a socket file and a pid file pointing to a dead process
        std::fs::write(&socket_path, b"").expect("create fake socket");
        // PID 99999999 is almost certainly not running
        std::fs::write(&pid_path, "99999999").expect("create fake pid");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");
    }

    #[test]
    fn clean_stale_socket_removes_orphan_socket_without_pid_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Socket exists but no pid file
        std::fs::write(&socket_path, b"").expect("create fake socket");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "orphan socket should be removed");
    }

    #[test]
    fn clean_stale_socket_noop_when_nothing_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Nothing exists — should not panic
        ShpoolTerminalPool::clean_stale_socket(&socket_path);
    }

    /// Create a ShpoolTerminalPool via the async factory method.
    async fn test_pool_async(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir for shpool test");
        let socket_path = dir.path().join("shpool.socket");
        let pool = ShpoolTerminalPool::create(runner, socket_path).await;
        (pool, dir)
    }

    #[tokio::test]
    async fn create_writes_config_and_returns_pool() {
        // No mock responses needed — start_daemon spawns shpool directly
        // (not through MockRunner), and will fail gracefully in test.
        let runner = Arc::new(MockRunner::new(vec![]));
        let (pool, dir) = test_pool_async(runner).await;
        let config_path = dir.path().join("config.toml");
        assert!(config_path.exists(), "config should be written");
        assert_eq!(pool.display_name(), "shpool");
    }
}
