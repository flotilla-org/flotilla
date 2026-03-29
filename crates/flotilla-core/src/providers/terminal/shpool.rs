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
    /// Terminal env defaults (TERM, COLORTERM) from discovery, injected into
    /// sessions at creation time. Empty when the daemon environment already
    /// has them (local case); populated for remote daemons started without a TTY.
    terminal_env_defaults: TerminalEnvVars,
}

/// Shpool config content managed by flotilla.
/// Disables prompt prefix (flotilla manages its own UI) and forwards
/// terminal environment variables that would otherwise be lost when
/// the shpool daemon spawns shells outside the terminal emulator.
/// Note: `forward_env` only takes effect when creating new sessions,
/// not when reattaching to existing ones (shpool limitation).
const FLOTILLA_SHPOOL_CONFIG: &str = include_str!("shpool_config.toml");

impl ShpoolTerminalPool {
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath, terminal_env_defaults: TerminalEnvVars) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        if let Err(e) = runner.ensure_file(config_path.as_path(), FLOTILLA_SHPOOL_CONFIG).await {
            tracing::warn!(err = %e, "failed to write shpool config");
        }
        Self { runner, socket_path, config_path, terminal_env_defaults }
    }

    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: DaemonHostPath) -> Self {
        Self::new_with_env(runner, socket_path, vec![])
    }

    #[cfg(test)]
    pub(crate) fn new_with_env(
        runner: Arc<dyn CommandRunner>,
        socket_path: DaemonHostPath,
        terminal_env_defaults: TerminalEnvVars,
    ) -> Self {
        let config_path = DaemonHostPath::new(socket_path.as_path().parent().unwrap_or(Path::new(".")).join("config.toml"));
        Self { runner, socket_path, config_path, terminal_env_defaults }
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
        // terminal_env_defaults (from discovery) provide TERM/COLORTERM
        // fallbacks for daemons started without a TTY (e.g. remote SSH).
        let mut cmd_parts: Vec<String> = Vec::new();
        let has_env = !env_vars.is_empty() || !self.terminal_env_defaults.is_empty();
        if has_env {
            cmd_parts.push("env".to_string());
            for (k, v) in &self.terminal_env_defaults {
                cmd_parts.push(format!("{k}={}", flotilla_protocol::arg::shell_quote(v)));
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
            // Session names are UUIDs (attachable IDs) — always shell-safe, no quoting needed.
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
