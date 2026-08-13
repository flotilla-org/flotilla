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
                Some((ws_ref, Workspace { name: name.to_string(), attachable_set_id: None }))
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
        Ok((ws_ref, Workspace { name: config.name.clone(), attachable_set_id: None }))
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
