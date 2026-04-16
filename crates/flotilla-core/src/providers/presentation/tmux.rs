use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use tracing::{info, warn};

use crate::providers::{run, types::*, CommandRunner};

pub struct TmuxPresentationManager {
    runner: Arc<dyn CommandRunner>,
}

impl TmuxPresentationManager {
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
impl super::PresentationManager for TmuxPresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let session = self.session_name().await?;
        let start_time = self.tmux_cmd(&["display-message", "-p", "#{start_time}"]).await?;
        let output = self.tmux_cmd(&["list-windows", "-F", "#{window_id}\t#{window_name}"]).await?;

        let workspaces = output
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let (window_id, name) = line.split_once('\t')?;
                let ws_ref = format!("{start_time}:{session}:{window_id}");
                Some((ws_ref, Workspace { name: name.to_string(), correlation_keys: vec![], attachable_set_id: None }))
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "tmux: creating workspace");

        let rendered = super::resolve_template(config);
        let working_dir = config.working_directory.as_path().display().to_string();

        // Create new window, capturing its window ID
        let window_id = self.tmux_cmd(&["new-window", "-n", &config.name, "-c", &working_dir, "-P", "-F", "#{window_id}"]).await?;
        let session = self.session_name().await?;
        let start_time = self.tmux_cmd(&["display-message", "-p", "#{start_time}"]).await?;
        let ws_ref = format!("{start_time}:{session}:{window_id}");

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

        info!(workspace = %config.name, "tmux: workspace ready");
        Ok((ws_ref, Workspace { name: config.name.clone(), correlation_keys: vec![], attachable_set_id: None }))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let window_id = ws_ref.rsplit_once(':').map(|(_, id)| id).ok_or_else(|| format!("invalid tmux ws_ref: {ws_ref}"))?;
        info!(%ws_ref, %window_id, "tmux: switching to window by id");
        self.tmux_cmd(&["select-window", "-t", window_id]).await?;
        Ok(())
    }

    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let window_id = ws_ref.rsplit_once(':').map(|(_, id)| id).ok_or_else(|| format!("invalid tmux ws_ref: {ws_ref}"))?;
        info!(%ws_ref, %window_id, "tmux: killing window by id");
        self.tmux_cmd(&["kill-window", "-t", window_id]).await?;
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::{presentation::PresentationManager, replay},
    };

    #[test]
    fn split_flag_maps_directions() {
        assert_eq!(TmuxPresentationManager::split_flag("left"), "-h");
        assert_eq!(TmuxPresentationManager::split_flag("right"), "-h");
        assert_eq!(TmuxPresentationManager::split_flag("up"), "-v");
        assert_eq!(TmuxPresentationManager::split_flag("down"), "-v");
        assert_eq!(TmuxPresentationManager::split_flag("unknown"), "-h");
        assert_eq!(TmuxPresentationManager::split_flag(""), "-h");
    }

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("presentation", name)
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
    }

    #[tokio::test]
    async fn record_replay_create_and_switch_workspaces() {
        let live = replay::is_live();

        if live {
            setup_tmux_ws_session();
        }

        let session = replay::test_session(&fixture("tmux_workspaces.yaml"), replay::Masks::new());
        let runner = replay::test_runner(&session);

        let mgr = TmuxPresentationManager::new(runner.clone());

        // Create workspace "feat-123"
        let config1 = WorkspaceAttachRequest {
            name: "feat-123".to_string(),
            working_directory: ExecutionEnvironmentPath::new("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            attach_commands: vec![],
        };
        let (name1, ws1) = mgr.create_workspace(&config1).await.unwrap();
        assert_eq!(ws1.name, "feat-123");
        // ws_ref is now start_time:session:@window_id, not the name
        assert!(name1.contains(':'), "ws_ref should contain colons: {name1}");

        // Create workspace "fix-456"
        let config2 = WorkspaceAttachRequest {
            name: "fix-456".to_string(),
            working_directory: ExecutionEnvironmentPath::new("/tmp"),
            template_yaml: None,
            template_vars: HashMap::new(),
            attach_commands: vec![],
        };
        let (name2, ws2) = mgr.create_workspace(&config2).await.unwrap();
        assert_eq!(ws2.name, "fix-456");
        assert!(name2.contains(':'), "ws_ref should contain colons: {name2}");

        // Verify with external command: list windows through the runner
        let list_output = run!(runner, "tmux", &["list-windows", "-F", "#{window_name}"], Path::new(".")).unwrap();
        assert!(list_output.contains("feat-123"), "expected 'feat-123' in list output: {list_output}");
        assert!(list_output.contains("fix-456"), "expected 'fix-456' in list output: {list_output}");

        // Switch to first workspace by ws_ref
        mgr.select_workspace(&name1).await.unwrap();

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

        let mgr = TmuxPresentationManager::new(runner);
        let workspaces = mgr.list_workspaces().await.unwrap();

        assert_eq!(workspaces.len(), 2);
        let names: Vec<&str> = workspaces.iter().map(|w| w.1.name.as_str()).collect();
        assert!(names.contains(&"main-work"), "expected 'main-work' in {names:?}");
        assert!(names.contains(&"feature-branch"), "expected 'feature-branch' in {names:?}");

        // correlation_keys should be empty
        for (_key, ws) in &workspaces {
            assert!(ws.correlation_keys.is_empty());
        }

        if live {
            teardown_tmux_session();
        }

        session.finish();
    }

    #[tokio::test]
    async fn delete_workspace_kills_tmux_window() {
        let runner = Arc::new(crate::providers::testing::MockRunner::new(vec![Ok("".to_string())]));
        let mgr = TmuxPresentationManager::new(runner);

        mgr.delete_workspace("1712345678:flotilla:@42").await.expect("delete should succeed");
    }
}
