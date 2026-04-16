use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::providers::{run, types::*, CommandRunner};

/// Timeout for individual `zellij action` calls.  Combined with the 1-permit
/// semaphore this limits the blast radius when Zellij is unresponsive: at most
/// one child process can be waiting at a time, and callers give up after the
/// timeout.  Note that the timed-out child process itself may linger until the
/// Zellij server recovers or is killed — the runner's `Command::output()` does
/// not set `kill_on_drop`.
const ZELLIJ_ACTION_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ZellijPresentationManager {
    runner: Arc<dyn CommandRunner>,
    /// Optional override for the session name. When `None`, falls back to
    /// the `ZELLIJ_SESSION_NAME` environment variable.
    session_name_override: Option<String>,
    /// Serialise all `zellij action` calls so we don't pile up child processes
    /// when the server is slow or unresponsive.
    action_semaphore: Semaphore,
}

impl ZellijPresentationManager {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner, session_name_override: None, action_semaphore: Semaphore::new(1) }
    }

    /// Create a manager targeting a specific session name, avoiding the need
    /// to read `ZELLIJ_SESSION_NAME` from the process environment.
    pub fn with_session_name(runner: Arc<dyn CommandRunner>, session_name: String) -> Self {
        Self { runner, session_name_override: Some(session_name), action_semaphore: Semaphore::new(1) }
    }

    /// Run `zellij action <args>` and return stdout, or an error on failure.
    ///
    /// Serialised via a semaphore so at most one `zellij action` child is
    /// outstanding at a time, and wrapped in a timeout so callers give up
    /// rather than blocking forever.
    async fn zellij_action(&self, args: &[&str]) -> Result<String, String> {
        let _permit = self.action_semaphore.acquire().await.map_err(|_| "zellij action semaphore closed".to_string())?;

        let mut cmd_args = vec!["action"];
        cmd_args.extend_from_slice(args);

        let action_desc = args.first().copied().unwrap_or("unknown");
        match tokio::time::timeout(ZELLIJ_ACTION_TIMEOUT, async { run!(self.runner, "zellij", &cmd_args, Path::new(".")) }).await {
            Ok(result) => result.map(|s| s.trim().to_string()),
            Err(_) => {
                warn!(action = %action_desc, timeout_secs = ZELLIJ_ACTION_TIMEOUT.as_secs(), "zellij action timed out");
                Err(format!("zellij action '{action_desc}' timed out after {}s", ZELLIJ_ACTION_TIMEOUT.as_secs()))
            }
        }
    }

    /// Check that `zellij --version` reports >= 0.40.
    /// Parses output like "zellij 0.42.2".
    pub async fn check_version(runner: &dyn CommandRunner) -> Result<(), String> {
        let version_str = run!(runner, "zellij", &["--version"], Path::new("."))
            .map_err(|e| format!("failed to run zellij --version: {e}"))?
            .trim()
            .to_string();
        let version_part = version_str.strip_prefix("zellij ").ok_or_else(|| format!("unexpected zellij version output: {version_str}"))?;

        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.len() < 2 {
            return Err(format!("cannot parse zellij version: {version_part}"));
        }

        let major: u32 = parts[0].parse().map_err(|_| format!("invalid major version: {}", parts[0]))?;
        let minor: u32 = parts[1].parse().map_err(|_| format!("invalid minor version: {}", parts[1]))?;

        if major == 0 && minor < 40 {
            return Err(format!("zellij >= 0.40 required, found {version_part}"));
        }

        info!(version = %version_part, "zellij version OK");
        Ok(())
    }

    /// Return the current Zellij session name. The session name must have been
    /// resolved at probe time and passed to `with_session_name()`.
    pub fn session_name(&self) -> Result<String, String> {
        self.session_name_override
            .clone()
            .ok_or_else(|| "zellij session name not resolved at probe time (ZELLIJ_SESSION_NAME was not set)".to_string())
    }

    /// Append `-- sh -c "command"` to args if command is non-empty.
    /// Uses sh -c to avoid quoting issues with complex commands.
    fn append_command_args<'a>(args: &mut Vec<&'a str>, command: &'a str) {
        if !command.is_empty() {
            args.extend(["--", "sh", "-c", command]);
        }
    }
}

#[async_trait]
impl super::PresentationManager for ZellijPresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let output = self.zellij_action(&["list-tabs", "--json"]).await?;
        let tabs: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|e| format!("zellij list-tabs: {e}"))?;

        let session = self.session_name()?;

        let workspaces = tabs
            .iter()
            .filter_map(|tab| {
                let tab_id = tab["tab_id"].as_u64()?;
                let name = tab["name"].as_str()?.to_string();
                let ws_ref = format!("{session}:{tab_id}");
                Some((ws_ref, Workspace { name, correlation_keys: vec![], attachable_set_id: None }))
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "zellij: creating workspace");

        let rendered = super::resolve_template(config);
        let working_dir = config.working_directory.as_path().display().to_string();

        // Create new tab — new-tab returns the tab_id to stdout
        let tab_id_str = self.zellij_action(&["new-tab", "--name", &config.name, "--cwd", &working_dir]).await?;
        let tab_id = tab_id_str.trim();
        let session = self.session_name()?;
        let ws_ref = format!("{session}:{tab_id}");

        // Small delay to let zellij process the tab creation
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Zellij's --cwd on new-pane doesn't reliably set the default shell's
        // working directory — the shell inherits the server's cwd instead. When
        // no command is given, explicitly launch $SHELL so --cwd is honoured.
        const SHELL_FALLBACK: &str = "exec \"${SHELL:-sh}\"";

        for (i, pane) in rendered.panes.iter().enumerate() {
            if i == 0 {
                // First pane is the tab's initial pane — send command via write-chars
                // (--cwd on new-tab already sets working directory, so skip if no command)
                if let Some(surface) = pane.surfaces.first() {
                    if !surface.command.is_empty() {
                        let text = format!("{}\n", surface.command);
                        self.zellij_action(&["write-chars", &text]).await?;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }

                // Additional surfaces in the first pane: stacked panes
                for surface in pane.surfaces.iter().skip(1) {
                    let mut args: Vec<&str> = vec!["new-pane", "--stacked", "--cwd", &working_dir];
                    let cmd = if surface.command.is_empty() { SHELL_FALLBACK } else { &surface.command };
                    Self::append_command_args(&mut args, cmd);
                    self.zellij_action(&args).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } else {
                let direction = pane.split.as_deref().unwrap_or("right");

                if let Some(surface) = pane.surfaces.first() {
                    let mut args: Vec<&str> = vec!["new-pane", "-d", direction, "--cwd", &working_dir];
                    let cmd = if surface.command.is_empty() { SHELL_FALLBACK } else { &surface.command };
                    Self::append_command_args(&mut args, cmd);
                    self.zellij_action(&args).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                // Additional surfaces in this pane: stacked panes
                for surface in pane.surfaces.iter().skip(1) {
                    let mut args: Vec<&str> = vec!["new-pane", "--stacked", "--cwd", &working_dir];
                    let cmd = if surface.command.is_empty() { SHELL_FALLBACK } else { &surface.command };
                    Self::append_command_args(&mut args, cmd);
                    self.zellij_action(&args).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }

        // Focus the designated pane. Use focus-previous-pane which walks panes in
        // reverse creation order regardless of split direction (unlike move-focus which
        // is direction-specific and fails for mixed horizontal/vertical layouts).
        let focus_index = rendered.panes.iter().position(|p| p.focus);
        let total_panes: usize = rendered.panes.iter().map(|p| p.surfaces.len().max(1)).sum();
        if let Some(fi) = focus_index {
            let panes_before: usize = rendered.panes.iter().take(fi).map(|p| p.surfaces.len().max(1)).sum();
            let moves_back = total_panes.saturating_sub(1).saturating_sub(panes_before);
            for _ in 0..moves_back {
                self.zellij_action(&["focus-previous-pane"]).await.ok();
            }
        }

        info!(workspace = %config.name, "zellij: workspace ready");
        Ok((ws_ref, Workspace { name: config.name.clone(), correlation_keys: vec![], attachable_set_id: None }))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let tab_id = ws_ref.rsplit_once(':').map(|(_, id)| id).ok_or_else(|| format!("invalid zellij ws_ref: {ws_ref}"))?;
        info!(%ws_ref, %tab_id, "zellij: switching to tab by id");
        self.zellij_action(&["go-to-tab-by-id", tab_id]).await?;
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        match self.session_name() {
            Ok(session) => format!("{session}:"),
            Err(_) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::providers::{presentation::PresentationManager, replay};

    #[test]
    fn append_command_args_with_command() {
        let mut args: Vec<&str> = vec!["new-pane"];
        let cmd = "echo hello";
        ZellijPresentationManager::append_command_args(&mut args, cmd);
        assert_eq!(args, vec!["new-pane", "--", "sh", "-c", "echo hello"]);
    }

    #[test]
    fn append_command_args_empty_is_noop() {
        let mut args: Vec<&str> = vec!["new-pane"];
        ZellijPresentationManager::append_command_args(&mut args, "");
        assert_eq!(args, vec!["new-pane"]);
    }

    #[test]
    fn shell_fallback_produces_explicit_shell_launch() {
        // Simulates the call-site logic: empty command -> SHELL_FALLBACK
        const SHELL_FALLBACK: &str = "exec \"${SHELL:-sh}\"";
        let command = "";
        let cmd = if command.is_empty() { SHELL_FALLBACK } else { command };
        let mut args: Vec<&str> = vec!["new-pane", "--cwd", "/tmp/repo"];
        ZellijPresentationManager::append_command_args(&mut args, cmd);
        assert_eq!(args, vec!["new-pane", "--cwd", "/tmp/repo", "--", "sh", "-c", "exec \"${SHELL:-sh}\""]);
    }

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("workspace", name)
    }

    fn setup_zellij_ws_session() {
        // Create a tmux session to host zellij
        let status = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", "zellij-host-ws", "-x", "80", "-y", "24"])
            .status()
            .expect("failed to create tmux host session");
        assert!(status.success(), "tmux new-session for zellij host failed");

        // Start zellij inside the tmux session
        std::process::Command::new("tmux")
            .args(["send-keys", "-t", "zellij-host-ws", "zellij --session flotilla-test-zj-ws", "Enter"])
            .status()
            .expect("failed to send zellij start command");

        // Wait for zellij to start up
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    fn teardown_zellij_ws_session() {
        // Quit zellij gracefully
        let _ = std::process::Command::new("zellij").args(["action", "quit"]).env("ZELLIJ_SESSION_NAME", "flotilla-test-zj-ws").status();

        std::thread::sleep(std::time::Duration::from_millis(500));

        // Kill the tmux host session
        let _ = std::process::Command::new("tmux").args(["kill-session", "-t", "zellij-host-ws"]).status();

        std::thread::sleep(std::time::Duration::from_millis(500));

        // Force-delete the zellij session
        let _ = std::process::Command::new("zellij").args(["delete-session", "flotilla-test-zj-ws", "--force"]).status();
    }

    #[tokio::test]
    async fn record_replay_create_and_switch_workspaces() {
        let live = replay::is_live();

        if live {
            setup_zellij_ws_session();
        }

        let session = replay::test_session(&fixture("zellij_workspaces.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = ZellijPresentationManager::with_session_name(runner.clone(), "flotilla-test-zj-ws".to_string());

        // Create workspace "feat-123"
        let config1 = WorkspaceAttachRequest {
            name: "feat-123".to_string(),
            working_directory: crate::path_context::ExecutionEnvironmentPath::new("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            attach_commands: vec![],
        };
        let (ws_ref1, ws1) = mgr.create_workspace(&config1).await.unwrap();
        assert_eq!(ws1.name, "feat-123");
        // ws_ref should now be session:tab_id format
        assert!(ws_ref1.starts_with("flotilla-test-zj-ws:"), "ws_ref should start with session name: {ws_ref1}");

        // Create workspace "fix-456"
        let config2 = WorkspaceAttachRequest {
            name: "fix-456".to_string(),
            working_directory: crate::path_context::ExecutionEnvironmentPath::new("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            attach_commands: vec![],
        };
        let (ws_ref2, ws2) = mgr.create_workspace(&config2).await.unwrap();
        assert_eq!(ws2.name, "fix-456");
        assert!(ws_ref2.starts_with("flotilla-test-zj-ws:"), "ws_ref should start with session name: {ws_ref2}");

        // Switch to first workspace
        mgr.select_workspace(&ws_ref1).await.unwrap();

        // List workspaces via the manager
        let workspaces = mgr.list_workspaces().await.unwrap();
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"feat-123"), "expected 'feat-123' in {names:?}");
        assert!(names.contains(&"fix-456"), "expected 'fix-456' in {names:?}");

        if live {
            teardown_zellij_ws_session();
        }

        session.finish();
    }

    fn setup_zellij_session() {
        // Create a tmux session to host zellij
        let status = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", "zellij-host", "-x", "80", "-y", "24"])
            .status()
            .expect("failed to create tmux host session");
        assert!(status.success(), "tmux new-session for zellij host failed");

        // Start zellij inside the tmux session
        std::process::Command::new("tmux")
            .args(["send-keys", "-t", "zellij-host", "zellij --session flotilla-test-zj", "Enter"])
            .status()
            .expect("failed to send zellij start command");

        // Wait for zellij to start up
        std::thread::sleep(std::time::Duration::from_secs(3));

        // Create a second tab named "feature-tab"
        let status = std::process::Command::new("zellij")
            .args(["action", "new-tab", "--name", "feature-tab"])
            .env("ZELLIJ_SESSION_NAME", "flotilla-test-zj")
            .status()
            .expect("failed to create zellij tab");
        assert!(status.success(), "zellij action new-tab failed");

        // Small delay for tab creation
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    fn teardown_zellij_session() {
        // Kill the tmux host session (this also kills zellij running inside it)
        let _ = std::process::Command::new("tmux").args(["kill-session", "-t", "zellij-host"]).status();

        std::thread::sleep(std::time::Duration::from_millis(500));

        // Force-delete the zellij session
        let _ = std::process::Command::new("zellij").args(["delete-session", "flotilla-test-zj", "--force"]).status();
    }

    #[tokio::test]
    async fn record_replay_list_workspaces() {
        let live = replay::is_live();

        if live {
            setup_zellij_session();
        }

        let session = replay::test_session(&fixture("zellij_list.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = ZellijPresentationManager::with_session_name(runner, "flotilla-test-zj".to_string());
        let workspaces = mgr.list_workspaces().await.unwrap();

        assert_eq!(workspaces.len(), 2);
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"Tab #1"), "expected 'Tab #1' in {names:?}");
        assert!(names.contains(&"feature-tab"), "expected 'feature-tab' in {names:?}");

        // correlation_keys should be empty
        for (_key, ws) in &workspaces {
            assert!(ws.correlation_keys.is_empty());
        }

        if live {
            teardown_zellij_session();
        }

        session.finish();
    }
}
