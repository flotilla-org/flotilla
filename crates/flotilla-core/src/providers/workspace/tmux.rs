use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{info, warn};

use crate::providers::types::*;
use crate::template::WorkspaceTemplate;

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

pub struct TmuxWorkspaceManager;

impl Default for TmuxWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxWorkspaceManager {
    pub fn new() -> Self {
        Self
    }

    /// Run a tmux command and return stdout, or an error on failure.
    async fn tmux_cmd(args: &[&str]) -> Result<String, String> {
        let output = Command::new("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("failed to run tmux: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(format!(
                "tmux {} failed: {}",
                args.first().unwrap_or(&""),
                if stderr.is_empty() { &stdout } else { &stderr }
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Return the current tmux session name.
    async fn session_name() -> Result<String, String> {
        Self::tmux_cmd(&["display-message", "-p", "#{session_name}"]).await
    }

    /// Return the state file path: `~/.config/flotilla/tmux/{session}/state.toml`.
    fn state_path(session: &str) -> Result<PathBuf, String> {
        let config_dir =
            dirs::config_dir().ok_or_else(|| "could not determine config directory".to_string())?;
        Ok(config_dir
            .join("flotilla")
            .join("tmux")
            .join(session)
            .join("state.toml"))
    }

    /// Load persisted state for the given session. Returns default on any error.
    fn load_state(session: &str) -> TmuxState {
        let path = match Self::state_path(session) {
            Ok(p) => p,
            Err(_) => return TmuxState::default(),
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return TmuxState::default(),
        };
        match toml::from_str(&contents) {
            Ok(state) => state,
            Err(e) => {
                warn!("corrupt tmux state file, treating as empty: {e}");
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
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = toml::to_string(state) {
            let _ = std::fs::write(&path, contents);
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
    fn display_name(&self) -> &str {
        "tmux Workspaces"
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, String> {
        let output = Self::tmux_cmd(&["list-windows", "-F", "#{window_name}"]).await?;
        let window_names: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

        // Load state for enrichment, pruning stale entries
        let (session, mut state) = match Self::session_name().await {
            Ok(s) => {
                let st = Self::load_state(&s);
                (Some(s), st)
            }
            Err(_) => (None, TmuxState::default()),
        };

        let live_names: HashSet<&str> = window_names.iter().copied().collect();
        let before_len = state.windows.len();
        state
            .windows
            .retain(|name, _| live_names.contains(name.as_str()));
        if state.windows.len() != before_len {
            if let Some(ref session) = session {
                Self::save_state(session, &state);
            }
        }

        let workspaces = window_names
            .into_iter()
            .map(|name| {
                let mut directories = Vec::new();
                let mut correlation_keys = Vec::new();

                if let Some(window) = state.windows.get(name) {
                    let path = PathBuf::from(&window.working_directory);
                    correlation_keys.push(CorrelationKey::CheckoutPath(path.clone()));
                    directories.push(path);
                }

                Workspace {
                    ws_ref: name.to_string(),
                    name: name.to_string(),
                    directories,
                    correlation_keys,
                }
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<Workspace, String> {
        info!("tmux: creating workspace '{}'", config.name);

        let template = if let Some(ref yaml) = config.template_yaml {
            serde_yaml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
                warn!("tmux: failed to parse workspace template, using default: {e}");
                WorkspaceTemplate::load_default()
            })
        } else {
            WorkspaceTemplate::load_default()
        };

        let rendered = template.render(&config.template_vars);
        let working_dir = config.working_directory.display().to_string();

        // Create new window
        Self::tmux_cmd(&["new-window", "-n", &config.name, "-c", &working_dir]).await?;

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
                    "tmux: pane '{}' has {} surfaces; tmux does not support tabbed/stacked panes, \
                     extra surfaces will be created as additional splits",
                    pane.name,
                    pane.surfaces.len()
                );
            }

            if pane.focus {
                focus_pane_index = Some(pane_count);
            }

            if i == 0 {
                // First pane is the window's initial pane — send command via send-keys
                if let Some(surface) = pane.surfaces.first() {
                    if !surface.command.is_empty() {
                        Self::tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                pane_count += 1;

                // Additional surfaces in first pane become splits
                for surface in pane.surfaces.iter().skip(1) {
                    Self::tmux_cmd(&["split-window", "-v", "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        Self::tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pane_count += 1;
                }
            } else {
                // Subsequent panes: split from the last pane
                let direction = pane.split.as_deref().unwrap_or("right");
                let flag = Self::split_flag(direction);

                if let Some(surface) = pane.surfaces.first() {
                    Self::tmux_cmd(&["split-window", flag, "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        Self::tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pane_count += 1;
                }

                // Additional surfaces become splits
                for surface in pane.surfaces.iter().skip(1) {
                    Self::tmux_cmd(&["split-window", "-v", "-c", &working_dir]).await?;
                    if !surface.command.is_empty() {
                        Self::tmux_cmd(&["send-keys", &surface.command, "Enter"]).await?;
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
            Self::tmux_cmd(&["select-pane", "-t", &target]).await.ok();
        }

        // Save state
        if let Ok(session) = Self::session_name().await {
            let mut state = Self::load_state(&session);
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default();
            state.windows.insert(
                config.name.clone(),
                WindowState {
                    working_directory: working_dir.clone(),
                    created_at: timestamp,
                },
            );
            Self::save_state(&session, &state);
        }

        let directories = vec![config.working_directory.clone()];
        let correlation_keys = directories
            .iter()
            .map(|d| CorrelationKey::CheckoutPath(d.clone()))
            .collect();

        info!("tmux: workspace '{}' ready", config.name);
        Ok(Workspace {
            ws_ref: config.name.clone(),
            name: config.name.clone(),
            directories,
            correlation_keys,
        })
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!("tmux: switching to window '{ws_ref}'");
        Self::tmux_cmd(&["select-window", "-t", ws_ref]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(path.ends_with("flotilla/tmux/my-session/state.toml"));
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
        let state_path = dir
            .path()
            .join("flotilla")
            .join("tmux")
            .join(session)
            .join("state.toml");

        // Create state with a window entry
        let mut state = TmuxState::default();
        state.windows.insert(
            "my-window".to_string(),
            WindowState {
                working_directory: "/tmp/work".to_string(),
                created_at: "1234567890".to_string(),
            },
        );

        // Save manually (since state_path uses dirs::config_dir)
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let contents = toml::to_string(&state).unwrap();
        std::fs::write(&state_path, &contents).unwrap();

        // Load back and verify
        let loaded: TmuxState =
            toml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
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
        state.windows.insert(
            "feat-branch".to_string(),
            WindowState {
                working_directory: "/home/user/project".to_string(),
                created_at: "1000".to_string(),
            },
        );
        let serialized = toml::to_string(&state).unwrap();
        assert!(serialized.contains("[windows.feat-branch]"));
        assert!(serialized.contains("working_directory"));
        assert!(serialized.contains("created_at"));
    }

    #[test]
    fn prune_retains_only_live_windows() {
        let mut state = TmuxState::default();
        state.windows.insert(
            "live-window".to_string(),
            WindowState {
                working_directory: "/tmp/live".to_string(),
                created_at: "1".to_string(),
            },
        );
        state.windows.insert(
            "stale-window".to_string(),
            WindowState {
                working_directory: "/tmp/stale".to_string(),
                created_at: "2".to_string(),
            },
        );
        state.windows.insert(
            "another-stale".to_string(),
            WindowState {
                working_directory: "/tmp/stale2".to_string(),
                created_at: "3".to_string(),
            },
        );

        let live_names: HashSet<&str> = ["live-window"].into_iter().collect();
        state
            .windows
            .retain(|name, _| live_names.contains(name.as_str()));

        assert_eq!(state.windows.len(), 1);
        assert!(state.windows.contains_key("live-window"));
    }

    #[test]
    fn prune_empty_state_is_noop() {
        let mut state = TmuxState::default();
        let live_names: HashSet<&str> = ["win1", "win2"].into_iter().collect();
        state
            .windows
            .retain(|name, _| live_names.contains(name.as_str()));
        assert!(state.windows.is_empty());
    }

    #[test]
    fn prune_all_live_removes_nothing() {
        let mut state = TmuxState::default();
        state.windows.insert(
            "win1".to_string(),
            WindowState {
                working_directory: "/tmp/1".to_string(),
                created_at: "1".to_string(),
            },
        );
        state.windows.insert(
            "win2".to_string(),
            WindowState {
                working_directory: "/tmp/2".to_string(),
                created_at: "2".to_string(),
            },
        );

        let live_names: HashSet<&str> = ["win1", "win2"].into_iter().collect();
        state
            .windows
            .retain(|name, _| live_names.contains(name.as_str()));
        assert_eq!(state.windows.len(), 2);
    }
}
