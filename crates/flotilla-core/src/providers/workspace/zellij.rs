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
struct ZellijState {
    #[serde(default)]
    tabs: HashMap<String, TabState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TabState {
    working_directory: String,
    created_at: String,
}

pub struct ZellijWorkspaceManager;

impl Default for ZellijWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ZellijWorkspaceManager {
    pub fn new() -> Self {
        Self
    }

    /// Run `zellij action <args>` and return stdout, or an error on failure.
    pub async fn zellij_action(args: &[&str]) -> Result<String, String> {
        let mut cmd_args = vec!["action"];
        cmd_args.extend_from_slice(args);

        let output = Command::new("zellij")
            .args(&cmd_args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("failed to run zellij action: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(format!(
                "zellij action {} failed: {}",
                args.first().unwrap_or(&""),
                if stderr.is_empty() { &stdout } else { &stderr }
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check that `zellij --version` reports >= 0.40.
    /// Parses output like "zellij 0.42.2".
    pub async fn check_version() -> Result<(), String> {
        let output = Command::new("zellij")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("failed to run zellij --version: {e}"))?;

        let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let version_part = version_str
            .strip_prefix("zellij ")
            .ok_or_else(|| format!("unexpected zellij version output: {version_str}"))?;

        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.len() < 2 {
            return Err(format!("cannot parse zellij version: {version_part}"));
        }

        let major: u32 = parts[0]
            .parse()
            .map_err(|_| format!("invalid major version: {}", parts[0]))?;
        let minor: u32 = parts[1]
            .parse()
            .map_err(|_| format!("invalid minor version: {}", parts[1]))?;

        if major == 0 && minor < 40 {
            return Err(format!("zellij >= 0.40 required, found {version_part}"));
        }

        info!("zellij version {version_part} OK");
        Ok(())
    }

    /// Return the current Zellij session name from the environment.
    pub fn session_name() -> Result<String, String> {
        std::env::var("ZELLIJ_SESSION_NAME").map_err(|_| {
            "not running inside a zellij session (ZELLIJ_SESSION_NAME not set)".to_string()
        })
    }

    /// Return the state file path: `~/.config/flotilla/zellij/{session}/state.toml`.
    pub fn state_path(session: &str) -> Result<PathBuf, String> {
        let config_dir =
            dirs::config_dir().ok_or_else(|| "could not determine config directory".to_string())?;
        Ok(config_dir
            .join("flotilla")
            .join("zellij")
            .join(session)
            .join("state.toml"))
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
                tracing::warn!("corrupt zellij state file, treating as empty: {e}");
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
    fn display_name(&self) -> &str {
        "zellij Workspaces"
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, String> {
        let output = Self::zellij_action(&["query-tab-names"]).await?;
        let tab_names: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

        // Load state for enrichment, pruning stale entries
        let (session, mut state) = match Self::session_name() {
            Ok(s) => {
                let st = Self::load_state(&s);
                (Some(s), st)
            }
            Err(_) => (None, ZellijState::default()),
        };

        let live_names: HashSet<&str> = tab_names.iter().copied().collect();
        let before_len = state.tabs.len();
        state
            .tabs
            .retain(|name, _| live_names.contains(name.as_str()));
        if state.tabs.len() != before_len {
            if let Some(ref session) = session {
                Self::save_state(session, &state);
            }
        }

        let workspaces = tab_names
            .into_iter()
            .map(|name| {
                let mut directories = Vec::new();
                let mut correlation_keys = Vec::new();

                if let Some(tab) = state.tabs.get(name) {
                    let path = PathBuf::from(&tab.working_directory);
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
        info!("zellij: creating workspace '{}'", config.name);

        // Parse template from YAML if provided, otherwise use default
        let template = if let Some(ref yaml) = config.template_yaml {
            serde_yaml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
                warn!("zellij: failed to parse workspace template, using default: {e}");
                WorkspaceTemplate::load_default()
            })
        } else {
            WorkspaceTemplate::load_default()
        };

        let rendered = template.render(&config.template_vars);
        let working_dir = config.working_directory.display().to_string();

        // Create new tab
        Self::zellij_action(&["new-tab", "--name", &config.name, "--cwd", &working_dir]).await?;

        // Small delay to let zellij process the tab creation
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        for (i, pane) in rendered.panes.iter().enumerate() {
            if i == 0 {
                // First pane is the tab's initial pane — send command via write-chars
                // (--cwd on new-tab already sets working directory, so skip if no command)
                if let Some(surface) = pane.surfaces.first() {
                    if !surface.command.is_empty() {
                        let text = format!("{}\n", surface.command);
                        Self::zellij_action(&["write-chars", &text]).await?;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }

                // Additional surfaces in the first pane: stacked panes
                for surface in pane.surfaces.iter().skip(1) {
                    let mut args: Vec<&str> = vec!["new-pane", "--stacked", "--cwd", &working_dir];
                    Self::append_command_args(&mut args, &surface.command);
                    Self::zellij_action(&args).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } else {
                // Subsequent panes: create via new-pane with direction
                let direction = pane.split.as_deref().unwrap_or("right");

                if let Some(surface) = pane.surfaces.first() {
                    let mut args: Vec<&str> =
                        vec!["new-pane", "-d", direction, "--cwd", &working_dir];
                    Self::append_command_args(&mut args, &surface.command);
                    Self::zellij_action(&args).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                // Additional surfaces in this pane: stacked panes
                for surface in pane.surfaces.iter().skip(1) {
                    let mut args: Vec<&str> = vec!["new-pane", "--stacked", "--cwd", &working_dir];
                    Self::append_command_args(&mut args, &surface.command);
                    Self::zellij_action(&args).await?;
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
            let panes_before: usize = rendered
                .panes
                .iter()
                .take(fi)
                .map(|p| p.surfaces.len().max(1))
                .sum();
            let moves_back = total_panes.saturating_sub(1).saturating_sub(panes_before);
            for _ in 0..moves_back {
                Self::zellij_action(&["focus-previous-pane"]).await.ok();
            }
        }

        // Save state
        if let Ok(session) = Self::session_name() {
            let mut state = Self::load_state(&session);
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default();
            state.tabs.insert(
                config.name.clone(),
                TabState {
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

        info!("zellij: workspace '{}' ready", config.name);
        Ok(Workspace {
            ws_ref: config.name.clone(),
            name: config.name.clone(),
            directories,
            correlation_keys,
        })
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!("zellij: switching to tab '{ws_ref}'");
        Self::zellij_action(&["go-to-tab-name", ws_ref]).await?;
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
        let state_path = dir
            .path()
            .join("flotilla")
            .join("zellij")
            .join(session)
            .join("state.toml");

        // Create state with a tab entry
        let mut state = ZellijState::default();
        state.tabs.insert(
            "my-tab".to_string(),
            TabState {
                working_directory: "/tmp/work".to_string(),
                created_at: "1234567890".to_string(),
            },
        );

        // Save manually
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let contents = toml::to_string(&state).unwrap();
        std::fs::write(&state_path, &contents).unwrap();

        // Load back and verify
        let loaded: ZellijState =
            toml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
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
        state.tabs.insert(
            "feat-branch".to_string(),
            TabState {
                working_directory: "/home/user/project".to_string(),
                created_at: "1000".to_string(),
            },
        );
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
    fn prune_retains_only_live_tabs() {
        let mut state = ZellijState::default();
        state.tabs.insert(
            "live-tab".to_string(),
            TabState {
                working_directory: "/tmp/live".to_string(),
                created_at: "1".to_string(),
            },
        );
        state.tabs.insert(
            "stale-tab".to_string(),
            TabState {
                working_directory: "/tmp/stale".to_string(),
                created_at: "2".to_string(),
            },
        );
        state.tabs.insert(
            "another-stale".to_string(),
            TabState {
                working_directory: "/tmp/stale2".to_string(),
                created_at: "3".to_string(),
            },
        );

        let live_names: HashSet<&str> = ["live-tab"].into_iter().collect();
        state
            .tabs
            .retain(|name, _| live_names.contains(name.as_str()));

        assert_eq!(state.tabs.len(), 1);
        assert!(state.tabs.contains_key("live-tab"));
    }

    #[test]
    fn prune_empty_state_is_noop() {
        let mut state = ZellijState::default();
        let live_names: HashSet<&str> = ["tab1", "tab2"].into_iter().collect();
        state
            .tabs
            .retain(|name, _| live_names.contains(name.as_str()));
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn prune_all_live_removes_nothing() {
        let mut state = ZellijState::default();
        state.tabs.insert(
            "tab1".to_string(),
            TabState {
                working_directory: "/tmp/1".to_string(),
                created_at: "1".to_string(),
            },
        );
        state.tabs.insert(
            "tab2".to_string(),
            TabState {
                working_directory: "/tmp/2".to_string(),
                created_at: "2".to_string(),
            },
        );

        let live_names: HashSet<&str> = ["tab1", "tab2"].into_iter().collect();
        state
            .tabs
            .retain(|name, _| live_names.contains(name.as_str()));
        assert_eq!(state.tabs.len(), 2);
    }
}
