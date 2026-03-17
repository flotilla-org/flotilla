use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ZellijState {
    #[serde(default)]
    tabs: HashMap<String, TabState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TabState {
    working_directory: String,
    created_at: String,
}

pub struct ZellijWorkspaceManager {
    runner: Arc<dyn CommandRunner>,
    /// Optional override for the session name. When `None`, falls back to
    /// the `ZELLIJ_SESSION_NAME` environment variable.
    session_name_override: Option<String>,
    /// Serialise all `zellij action` calls so we don't pile up child processes
    /// when the server is slow or unresponsive.
    action_semaphore: Semaphore,
}

impl ZellijWorkspaceManager {
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

    /// Return the current Zellij session name, preferring the override if set,
    /// otherwise falling back to the `ZELLIJ_SESSION_NAME` environment variable.
    pub fn session_name(&self) -> Result<String, String> {
        if let Some(ref name) = self.session_name_override {
            return Ok(name.clone());
        }
        std::env::var("ZELLIJ_SESSION_NAME").map_err(|_| "not running inside a zellij session (ZELLIJ_SESSION_NAME not set)".to_string())
    }

    /// Return the state file path: `~/.config/flotilla/zellij/{session}/state.toml`.
    pub fn state_path(session: &str) -> Result<PathBuf, String> {
        let config_dir = dirs::config_dir().ok_or_else(|| "could not determine config directory".to_string())?;
        Ok(config_dir.join("flotilla").join("zellij").join(session).join("state.toml"))
    }

    /// Load persisted state for the given session. Returns default on any error.
    fn load_state(session: &str) -> ZellijState {
        let path = match Self::state_path(session) {
            Ok(p) => p,
            Err(_) => return ZellijState::default(),
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return ZellijState::default(),
        };
        match toml::from_str(&contents) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(err = %e, "corrupt zellij state file, treating as empty");
                ZellijState::default()
            }
        }
    }

    /// Save state for the given session. Silently ignores errors.
    fn save_state(session: &str, state: &ZellijState) {
        let path = match Self::state_path(session) {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = toml::to_string(state) {
            let _ = std::fs::write(&path, contents);
        }
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
impl super::WorkspaceManager for ZellijWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let output = self.zellij_action(&["query-tab-names"]).await?;
        let tab_names: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

        // Load state for enrichment, pruning stale entries
        let (session, mut state) = match self.session_name() {
            Ok(s) => {
                let st = Self::load_state(&s);
                (Some(s), st)
            }
            Err(_) => (None, ZellijState::default()),
        };

        let live_names: HashSet<&str> = tab_names.iter().copied().collect();
        let before_len = state.tabs.len();
        state.tabs.retain(|name, _| live_names.contains(name.as_str()));
        if state.tabs.len() != before_len {
            if let Some(ref session) = session {
                Self::save_state(session, &state);
            }
        }

        let workspaces = tab_names
            .into_iter()
            .map(|name| {
                let mut directories = Vec::new();
                if let Some(tab) = state.tabs.get(name) {
                    let path = PathBuf::from(&tab.working_directory);
                    directories.push(path);
                }

                (name.to_string(), Workspace { name: name.to_string(), directories, correlation_keys: vec![], attachable_set_id: None })
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "zellij: creating workspace");

        let rendered = super::resolve_template(config);
        let working_dir = config.working_directory.display().to_string();

        // Create new tab
        self.zellij_action(&["new-tab", "--name", &config.name, "--cwd", &working_dir]).await?;

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

        // Save state
        if let Ok(session) = self.session_name() {
            let mut state = Self::load_state(&session);
            let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs().to_string()).unwrap_or_default();
            state.tabs.insert(config.name.clone(), TabState { working_directory: working_dir.clone(), created_at: timestamp });
            Self::save_state(&session, &state);
        }

        let directories = vec![config.working_directory.clone()];
        info!(workspace = %config.name, "zellij: workspace ready");
        Ok((config.name.clone(), Workspace { name: config.name.clone(), directories, correlation_keys: vec![], attachable_set_id: None }))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!(%ws_ref, "zellij: switching to tab");
        self.zellij_action(&["go-to-tab-name", ws_ref]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_path_contains_session_name() {
        let path = ZellijWorkspaceManager::state_path("my-session").unwrap();
        assert!(path.ends_with("flotilla/zellij/my-session/state.toml"));
    }

    #[test]
    fn load_state_returns_default_for_missing_file() {
        let state = ZellijWorkspaceManager::load_state("nonexistent-session-xyz");
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn toml_serialization_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = "test-session";
        let state_path = dir.path().join("flotilla").join("zellij").join(session).join("state.toml");

        // Create state with a tab entry
        let mut state = ZellijState::default();
        state
            .tabs
            .insert("my-tab".to_string(), TabState { working_directory: "/tmp/work".to_string(), created_at: "1234567890".to_string() });

        // Save manually
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let contents = toml::to_string(&state).unwrap();
        std::fs::write(&state_path, &contents).unwrap();

        // Load back and verify
        let loaded: ZellijState = toml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs["my-tab"].working_directory, "/tmp/work");
        assert_eq!(loaded.tabs["my-tab"].created_at, "1234567890");
    }

    #[test]
    fn corrupt_toml_fails_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "not valid toml {{{{").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(toml::from_str::<ZellijState>(&contents).is_err());
    }

    #[test]
    fn state_serialization_format() {
        let mut state = ZellijState::default();
        state.tabs.insert("feat-branch".to_string(), TabState {
            working_directory: "/home/user/project".to_string(),
            created_at: "1000".to_string(),
        });
        let serialized = toml::to_string(&state).unwrap();
        assert!(serialized.contains("[tabs.feat-branch]"));
        assert!(serialized.contains("working_directory"));
        assert!(serialized.contains("created_at"));
    }

    #[test]
    fn append_command_args_with_command() {
        let mut args: Vec<&str> = vec!["new-pane"];
        let cmd = "echo hello";
        ZellijWorkspaceManager::append_command_args(&mut args, cmd);
        assert_eq!(args, vec!["new-pane", "--", "sh", "-c", "echo hello"]);
    }

    #[test]
    fn append_command_args_empty_is_noop() {
        let mut args: Vec<&str> = vec!["new-pane"];
        ZellijWorkspaceManager::append_command_args(&mut args, "");
        assert_eq!(args, vec!["new-pane"]);
    }

    #[test]
    fn shell_fallback_produces_explicit_shell_launch() {
        // Simulates the call-site logic: empty command → SHELL_FALLBACK
        const SHELL_FALLBACK: &str = "exec \"${SHELL:-sh}\"";
        let command = "";
        let cmd = if command.is_empty() { SHELL_FALLBACK } else { command };
        let mut args: Vec<&str> = vec!["new-pane", "--cwd", "/tmp/repo"];
        ZellijWorkspaceManager::append_command_args(&mut args, cmd);
        assert_eq!(args, vec!["new-pane", "--cwd", "/tmp/repo", "--", "sh", "-c", "exec \"${SHELL:-sh}\""]);
    }

    #[test]
    fn prune_retains_only_live_tabs() {
        let mut state = ZellijState::default();
        state.tabs.insert("live-tab".to_string(), TabState { working_directory: "/tmp/live".to_string(), created_at: "1".to_string() });
        state.tabs.insert("stale-tab".to_string(), TabState { working_directory: "/tmp/stale".to_string(), created_at: "2".to_string() });
        state
            .tabs
            .insert("another-stale".to_string(), TabState { working_directory: "/tmp/stale2".to_string(), created_at: "3".to_string() });

        let live_names: HashSet<&str> = ["live-tab"].into_iter().collect();
        state.tabs.retain(|name, _| live_names.contains(name.as_str()));

        assert_eq!(state.tabs.len(), 1);
        assert!(state.tabs.contains_key("live-tab"));
    }

    #[test]
    fn prune_empty_state_is_noop() {
        let mut state = ZellijState::default();
        let live_names: HashSet<&str> = ["tab1", "tab2"].into_iter().collect();
        state.tabs.retain(|name, _| live_names.contains(name.as_str()));
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn prune_all_live_removes_nothing() {
        let mut state = ZellijState::default();
        state.tabs.insert("tab1".to_string(), TabState { working_directory: "/tmp/1".to_string(), created_at: "1".to_string() });
        state.tabs.insert("tab2".to_string(), TabState { working_directory: "/tmp/2".to_string(), created_at: "2".to_string() });

        let live_names: HashSet<&str> = ["tab1", "tab2"].into_iter().collect();
        state.tabs.retain(|name, _| live_names.contains(name.as_str()));
        assert_eq!(state.tabs.len(), 2);
    }

    use crate::providers::{replay, workspace::WorkspaceManager};

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

        // Clean up state files created by create_workspace
        if let Some(config_dir) = dirs::config_dir() {
            let state_dir = config_dir.join("flotilla").join("zellij").join("flotilla-test-zj-ws");
            let _ = std::fs::remove_dir_all(&state_dir);
        }
    }

    #[tokio::test]
    async fn record_replay_create_and_switch_workspaces() {
        let recording = replay::is_recording();

        if recording {
            setup_zellij_ws_session();
        }

        let session = replay::test_session(&fixture("zellij_workspaces.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = ZellijWorkspaceManager::with_session_name(runner.clone(), "flotilla-test-zj-ws".to_string());

        // Create workspace "feat-123"
        let config1 = WorkspaceConfig {
            name: "feat-123".to_string(),
            working_directory: PathBuf::from("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            resolved_commands: None,
        };
        let (name1, ws1) = mgr.create_workspace(&config1).await.unwrap();
        assert_eq!(name1, "feat-123");
        assert_eq!(ws1.name, "feat-123");

        // Create workspace "fix-456"
        let config2 = WorkspaceConfig {
            name: "fix-456".to_string(),
            working_directory: PathBuf::from("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            resolved_commands: None,
        };
        let (name2, ws2) = mgr.create_workspace(&config2).await.unwrap();
        assert_eq!(name2, "fix-456");
        assert_eq!(ws2.name, "fix-456");

        // Verify with external command: query tab names through the runner
        let list_output = run!(runner, "zellij", &["action", "query-tab-names"], Path::new(".")).unwrap();
        assert!(list_output.contains("feat-123"), "expected 'feat-123' in tab list: {list_output}");
        assert!(list_output.contains("fix-456"), "expected 'fix-456' in tab list: {list_output}");

        // Switch to "feat-123"
        mgr.select_workspace("feat-123").await.unwrap();

        // List workspaces via the manager — assert names only (not directories,
        // since state enrichment differs between recording and replay modes)
        let workspaces = mgr.list_workspaces().await.unwrap();
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"feat-123"), "expected 'feat-123' in {names:?}");
        assert!(names.contains(&"fix-456"), "expected 'fix-456' in {names:?}");

        if recording {
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
        let recording = replay::is_recording();

        if recording {
            setup_zellij_session();
        }

        let session = replay::test_session(&fixture("zellij_list.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = ZellijWorkspaceManager::with_session_name(runner, "flotilla-test-zj".to_string());
        let workspaces = mgr.list_workspaces().await.unwrap();

        assert_eq!(workspaces.len(), 2);
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"Tab #1"), "expected 'Tab #1' in {names:?}");
        assert!(names.contains(&"feature-tab"), "expected 'feature-tab' in {names:?}");

        // No state file exists, so directories and correlation_keys should be empty
        for (_key, ws) in &workspaces {
            assert!(ws.directories.is_empty());
            assert!(ws.correlation_keys.is_empty());
        }

        if recording {
            teardown_zellij_session();
        }

        session.finish();
    }
}
