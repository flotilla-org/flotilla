use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    path_context::DaemonHostPath,
    providers::{run, types::*, CommandRunner},
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TmuxState {
    #[serde(default)]
    windows: HashMap<String, WindowState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowState {
    working_directory: String,
    created_at: String,
}

pub struct TmuxWorkspaceManager {
    runner: Arc<dyn CommandRunner>,
}

impl TmuxWorkspaceManager {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Run a tmux command and return stdout, or an error on failure.
    async fn tmux_cmd(&self, args: &[&str]) -> Result<String, String> {
        run!(self.runner, "tmux", args, Path::new(".")).map(|s| s.trim().to_string())
    }

    /// Return the current tmux session name.
    async fn session_name(&self) -> Result<String, String> {
        self.tmux_cmd(&["display-message", "-p", "#{session_name}"]).await
    }

    /// Return the state file path: `~/.config/flotilla/tmux/{session}/state.toml`.
    fn state_path(session: &str) -> Result<DaemonHostPath, String> {
        let config_dir = dirs::config_dir().ok_or_else(|| "could not determine config directory".to_string())?;
        Ok(DaemonHostPath::new(config_dir.join("flotilla").join("tmux").join(session).join("state.toml")))
    }

    /// Load persisted state for the given session. Returns default on any error.
    fn load_state(session: &str) -> TmuxState {
        let path = match Self::state_path(session) {
            Ok(p) => p,
            Err(_) => return TmuxState::default(),
        };
        let contents = match std::fs::read_to_string(path.as_path()) {
            Ok(c) => c,
            Err(_) => return TmuxState::default(),
        };
        match toml::from_str(&contents) {
            Ok(state) => state,
            Err(e) => {
                warn!(err = %e, "corrupt tmux state file, treating as empty");
                TmuxState::default()
            }
        }
    }

    /// Save state for the given session. Silently ignores errors.
    fn save_state(session: &str, state: &TmuxState) {
        let path = match Self::state_path(session) {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Some(parent) = path.as_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = toml::to_string(state) {
            let _ = std::fs::write(path.as_path(), contents);
        }
    }

    /// Map split direction names to tmux flags.
    /// tmux: -h = horizontal split (pane appears to the right)
    ///        -v = vertical split (pane appears below)
    /// Note: tmux doesn't support placing a pane to the left or above directly;
    /// "left" produces the same result as "right" (-h), "up" same as "down" (-v).
    fn split_flag(direction: &str) -> &'static str {
        match direction {
            "left" | "right" => "-h",
            "up" | "down" => "-v",
            _ => "-h",
        }
    }
}

#[async_trait]
impl super::WorkspaceManager for TmuxWorkspaceManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let output = self.tmux_cmd(&["list-windows", "-F", "#{window_name}"]).await?;
        let window_names: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

        // Load state for enrichment, pruning stale entries
        let (session, mut state) = match self.session_name().await {
            Ok(s) => {
                let st = Self::load_state(&s);
                (Some(s), st)
            }
            Err(_) => (None, TmuxState::default()),
        };

        let live_names: HashSet<&str> = window_names.iter().copied().collect();
        let before_len = state.windows.len();
        state.windows.retain(|name, _| live_names.contains(name.as_str()));
        if state.windows.len() != before_len {
            if let Some(ref session) = session {
                Self::save_state(session, &state);
            }
        }

        let workspaces = window_names
            .into_iter()
            .map(|name| {
                let mut directories = Vec::new();
                if let Some(window) = state.windows.get(name) {
                    let path = PathBuf::from(&window.working_directory);
                    directories.push(path);
                }

                (name.to_string(), Workspace { name: name.to_string(), directories, correlation_keys: vec![], attachable_set_id: None })
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "tmux: creating workspace");

        let rendered = super::resolve_template(config);
        let working_dir = config.working_directory.as_path().display().to_string();

        // Create new window
        self.tmux_cmd(&["new-window", "-n", &config.name, "-c", &working_dir]).await?;

        // Small delay to let tmux process window creation
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Track pane count for focus. focus_pane_index captures the tmux pane index
        // of the first surface in the template pane marked with focus=true.
        let mut pane_count: usize = 0;
        let mut focus_pane_index: Option<usize> = None;

        for (i, pane) in rendered.panes.iter().enumerate() {
            // Warn if multiple surfaces — tmux doesn't support tabbed/stacked panes
            if pane.surfaces.len() > 1 {
                warn!(
                    pane = %pane.name,
                    surfaces = pane.surfaces.len(),
                    "tmux: pane has multiple surfaces; tmux does not support tabbed/stacked panes, \
                     extra surfaces will be created as additional splits"
                );
            }

            if pane.focus {
                focus_pane_index = Some(pane_count);
            }

            if i == 0 {
                // First pane is the window's initial pane — send command via send-keys
                if let Some(surface) = pane.surfaces.first() {
                    if !surface.command.is_empty() {
                        self.tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                pane_count += 1;

                // Additional surfaces in first pane become splits
                for surface in pane.surfaces.iter().skip(1) {
                    self.tmux_cmd(&["split-window", "-v", "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        self.tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pane_count += 1;
                }
            } else {
                // Subsequent panes: split from the last pane
                let direction = pane.split.as_deref().unwrap_or("right");
                let flag = Self::split_flag(direction);

                if let Some(surface) = pane.surfaces.first() {
                    self.tmux_cmd(&["split-window", flag, "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        self.tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pane_count += 1;
                }

                // Additional surfaces become splits
                for surface in pane.surfaces.iter().skip(1) {
                    self.tmux_cmd(&["split-window", "-v", "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        self.tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pane_count += 1;
                }
            }
        }

        // Focus the designated pane (use pane index within current window
        // to avoid issues with window names containing special characters)
        if let Some(fi) = focus_pane_index {
            // :.N targets pane N within the current window
            let target = format!(":.{fi}");
            self.tmux_cmd(&["select-pane", "-t", &target]).await.ok();
        }

        // Save state
        if let Ok(session) = self.session_name().await {
            let mut state = Self::load_state(&session);
            let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs().to_string()).unwrap_or_default();
            state.windows.insert(config.name.clone(), WindowState { working_directory: working_dir.clone(), created_at: timestamp });
            Self::save_state(&session, &state);
        }

        let directories = vec![config.working_directory.clone().into_path_buf()];
        info!(workspace = %config.name, "tmux: workspace ready");
        Ok((config.name.clone(), Workspace { name: config.name.clone(), directories, correlation_keys: vec![], attachable_set_id: None }))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!(%ws_ref, "tmux: switching to window");
        self.tmux_cmd(&["select-window", "-t", ws_ref]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_context::ExecutionEnvironmentPath;

    #[test]
    fn split_flag_maps_directions() {
        assert_eq!(TmuxWorkspaceManager::split_flag("left"), "-h");
        assert_eq!(TmuxWorkspaceManager::split_flag("right"), "-h");
        assert_eq!(TmuxWorkspaceManager::split_flag("up"), "-v");
        assert_eq!(TmuxWorkspaceManager::split_flag("down"), "-v");
        assert_eq!(TmuxWorkspaceManager::split_flag("unknown"), "-h");
        assert_eq!(TmuxWorkspaceManager::split_flag(""), "-h");
    }

    #[test]
    fn state_path_contains_session_name() {
        let path = TmuxWorkspaceManager::state_path("my-session").unwrap();
        assert!(path.as_path().ends_with("flotilla/tmux/my-session/state.toml"));
    }

    #[test]
    fn load_state_returns_default_for_missing_file() {
        let state = TmuxWorkspaceManager::load_state("nonexistent-session-xyz");
        assert!(state.windows.is_empty());
    }

    #[test]
    fn toml_serialization_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = "test-session";
        let state_path = dir.path().join("flotilla").join("tmux").join(session).join("state.toml");

        // Create state with a window entry
        let mut state = TmuxState::default();
        state.windows.insert("my-window".to_string(), WindowState {
            working_directory: "/tmp/work".to_string(),
            created_at: "1234567890".to_string(),
        });

        // Save manually (since state_path uses dirs::config_dir)
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let contents = toml::to_string(&state).unwrap();
        std::fs::write(&state_path, &contents).unwrap();

        // Load back and verify
        let loaded: TmuxState = toml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows["my-window"].working_directory, "/tmp/work");
        assert_eq!(loaded.windows["my-window"].created_at, "1234567890");
    }

    #[test]
    fn corrupt_toml_fails_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "this is not valid toml {{{{").unwrap();

        // Direct deserialization should fail
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(toml::from_str::<TmuxState>(&contents).is_err());
    }

    #[test]
    fn state_serialization_format() {
        let mut state = TmuxState::default();
        state.windows.insert("feat-branch".to_string(), WindowState {
            working_directory: "/home/user/project".to_string(),
            created_at: "1000".to_string(),
        });
        let serialized = toml::to_string(&state).unwrap();
        assert!(serialized.contains("[windows.feat-branch]"));
        assert!(serialized.contains("working_directory"));
        assert!(serialized.contains("created_at"));
    }

    #[test]
    fn prune_retains_only_live_windows() {
        let mut state = TmuxState::default();
        state
            .windows
            .insert("live-window".to_string(), WindowState { working_directory: "/tmp/live".to_string(), created_at: "1".to_string() });
        state
            .windows
            .insert("stale-window".to_string(), WindowState { working_directory: "/tmp/stale".to_string(), created_at: "2".to_string() });
        state
            .windows
            .insert("another-stale".to_string(), WindowState { working_directory: "/tmp/stale2".to_string(), created_at: "3".to_string() });

        let live_names: HashSet<&str> = ["live-window"].into_iter().collect();
        state.windows.retain(|name, _| live_names.contains(name.as_str()));

        assert_eq!(state.windows.len(), 1);
        assert!(state.windows.contains_key("live-window"));
    }

    #[test]
    fn prune_empty_state_is_noop() {
        let mut state = TmuxState::default();
        let live_names: HashSet<&str> = ["win1", "win2"].into_iter().collect();
        state.windows.retain(|name, _| live_names.contains(name.as_str()));
        assert!(state.windows.is_empty());
    }

    #[test]
    fn prune_all_live_removes_nothing() {
        let mut state = TmuxState::default();
        state.windows.insert("win1".to_string(), WindowState { working_directory: "/tmp/1".to_string(), created_at: "1".to_string() });
        state.windows.insert("win2".to_string(), WindowState { working_directory: "/tmp/2".to_string(), created_at: "2".to_string() });

        let live_names: HashSet<&str> = ["win1", "win2"].into_iter().collect();
        state.windows.retain(|name, _| live_names.contains(name.as_str()));
        assert_eq!(state.windows.len(), 2);
    }

    use crate::providers::{replay, workspace::WorkspaceManager};

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("workspace", name)
    }

    fn setup_tmux_session() {
        // Create a headless tmux session with two named windows
        let status = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", "flotilla-test-tmux", "-x", "80", "-y", "24"])
            .status()
            .expect("failed to create tmux session");
        assert!(status.success(), "tmux new-session failed");

        let status = std::process::Command::new("tmux")
            .args(["rename-window", "-t", "flotilla-test-tmux:0", "main-work"])
            .status()
            .expect("failed to rename tmux window");
        assert!(status.success(), "tmux rename-window failed");

        let status = std::process::Command::new("tmux")
            .args(["new-window", "-t", "flotilla-test-tmux", "-n", "feature-branch"])
            .status()
            .expect("failed to create tmux window");
        assert!(status.success(), "tmux new-window failed");
    }

    fn teardown_tmux_session() {
        let _ = std::process::Command::new("tmux").args(["kill-session", "-t", "flotilla-test-tmux"]).status();
    }

    fn setup_tmux_ws_session() {
        let status = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", "flotilla-test-tmux-ws", "-x", "80", "-y", "24"])
            .status()
            .expect("failed to create tmux session");
        assert!(status.success(), "tmux new-session failed");
    }

    fn teardown_tmux_ws_session() {
        let _ = std::process::Command::new("tmux").args(["kill-session", "-t", "flotilla-test-tmux-ws"]).status();

        // Clean up state files created by create_workspace
        if let Some(config_dir) = dirs::config_dir() {
            let state_dir = config_dir.join("flotilla").join("tmux").join("flotilla-test-tmux-ws");
            let _ = std::fs::remove_dir_all(&state_dir);
        }
    }

    #[tokio::test]
    async fn record_replay_create_and_switch_workspaces() {
        let live = replay::is_live();

        if live {
            setup_tmux_ws_session();
        }

        let session = replay::test_session(&fixture("tmux_workspaces.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = TmuxWorkspaceManager::new(runner.clone());

        // Create workspace "feat-123"
        let config1 = WorkspaceConfig {
            name: "feat-123".to_string(),
            working_directory: ExecutionEnvironmentPath::new("/tmp"),
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
            working_directory: ExecutionEnvironmentPath::new("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            resolved_commands: None,
        };
        let (name2, ws2) = mgr.create_workspace(&config2).await.unwrap();
        assert_eq!(name2, "fix-456");
        assert_eq!(ws2.name, "fix-456");

        // Verify with external command: list windows through the runner
        let list_output = run!(runner, "tmux", &["list-windows", "-F", "#{window_name}"], Path::new(".")).unwrap();
        assert!(list_output.contains("feat-123"), "expected 'feat-123' in list output: {list_output}");
        assert!(list_output.contains("fix-456"), "expected 'fix-456' in list output: {list_output}");

        // Switch to "feat-123"
        mgr.select_workspace("feat-123").await.unwrap();

        // Verify current window through the runner
        let current = run!(runner, "tmux", &["display-message", "-p", "#{window_name}"], Path::new(".")).unwrap();
        assert!(current.contains("feat-123"), "expected current window 'feat-123', got: {current}");

        // List workspaces via the manager
        let workspaces = mgr.list_workspaces().await.unwrap();
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"feat-123"), "expected 'feat-123' in {names:?}");
        assert!(names.contains(&"fix-456"), "expected 'fix-456' in {names:?}");
        // At least 3: default zsh window + feat-123 + fix-456
        assert!(workspaces.len() >= 3, "expected at least 3 workspaces, got {}", workspaces.len());

        if live {
            teardown_tmux_ws_session();
        }

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_list_workspaces() {
        let live = replay::is_live();

        if live {
            setup_tmux_session();
        }

        let session = replay::test_session(&fixture("tmux_list.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = TmuxWorkspaceManager::new(runner);
        let workspaces = mgr.list_workspaces().await.unwrap();

        assert_eq!(workspaces.len(), 2);
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"main-work"), "expected 'main-work' in {names:?}");
        assert!(names.contains(&"feature-branch"), "expected 'feature-branch' in {names:?}");

        // No state file exists, so directories and correlation_keys should be empty
        for (_key, ws) in &workspaces {
            assert!(ws.directories.is_empty());
            assert!(ws.correlation_keys.is_empty());
        }

        if live {
            teardown_tmux_session();
        }

        session.finish();
    }
}
