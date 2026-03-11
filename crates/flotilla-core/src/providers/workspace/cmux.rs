use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

use crate::providers::types::*;
use crate::providers::{run, CommandRunner};

const CMUX_BIN: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

pub struct CmuxWorkspaceManager {
    runner: Arc<dyn CommandRunner>,
}

impl CmuxWorkspaceManager {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn cmux_cmd(&self, args: &[&str]) -> Result<String, String> {
        run!(self.runner, CMUX_BIN, args, Path::new(".")).map(|s| s.trim().to_string())
    }

    /// Shell-quote a string with single quotes, escaping embedded single quotes.
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    /// Parse "OK surface:N workspace:M" -> "surface:N"
    fn parse_ok_ref(output: &str) -> String {
        output
            .strip_prefix("OK ")
            .unwrap_or(output)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    }
}

#[async_trait]
impl super::WorkspaceManager for CmuxWorkspaceManager {
    fn display_name(&self) -> &str {
        "cmux Workspaces"
    }

    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let output = self.cmux_cmd(&["--json", "list-workspaces"]).await?;
        let parsed: serde_json::Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
        let workspaces = parsed["workspaces"]
            .as_array()
            .ok_or("cmux list-workspaces: response missing 'workspaces' array")?;
        Ok(workspaces
            .iter()
            .filter_map(|ws| {
                let ws_ref = ws["ref"].as_str()?.to_string();
                let name = ws["title"].as_str().unwrap_or("").to_string();
                let directories: Vec<PathBuf> = ws["directories"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(PathBuf::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let correlation_keys: Vec<CorrelationKey> = directories
                    .iter()
                    .map(|d| CorrelationKey::CheckoutPath(d.clone()))
                    .collect();

                Some((
                    ws_ref,
                    Workspace {
                        name,
                        directories,
                        correlation_keys,
                    },
                ))
            })
            .collect())
    }

    async fn create_workspace(
        &self,
        config: &WorkspaceConfig,
    ) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "cmux: creating workspace");

        let rendered = super::resolve_template(config);

        // Create workspace — output is "OK workspace:N"
        let ws_output = self
            .cmux_cmd(&["new-workspace", "--name", &config.name])
            .await?;
        let ws_ref = Self::parse_ok_ref(&ws_output);
        if ws_ref.is_empty() {
            return Err("cmux new-workspace returned no workspace ref".to_string());
        }

        // Get initial surface + pane from the new workspace
        let panels_json = self
            .cmux_cmd(&["--json", "list-panels", "--workspace", &ws_ref])
            .await?;
        let panels: serde_json::Value =
            serde_json::from_str(&panels_json).map_err(|e| e.to_string())?;
        let first = panels["surfaces"]
            .as_array()
            .and_then(|s| s.first())
            .ok_or("cmux list-panels: no surfaces in new workspace")?;
        let initial_surface = first["ref"]
            .as_str()
            .ok_or("cmux list-panels: initial surface missing 'ref'")?
            .to_string();
        let initial_pane = first["pane_ref"]
            .as_str()
            .ok_or("cmux list-panels: initial surface missing 'pane_ref'")?
            .to_string();

        // Track pane name -> (surface_ref for split targeting, pane_ref for tab creation)
        let mut pane_info: HashMap<String, (String, String)> = HashMap::new();
        let mut surface_cmds: Vec<(String, String)> = Vec::new();
        let mut active_surfaces: Vec<(String, String, usize)> = Vec::new();
        let mut focus_pane: Option<String> = None;

        let working_dir = &config.working_directory;

        for (i, pane) in rendered.panes.iter().enumerate() {
            let (split_surface_ref, pane_ref) = if i == 0 {
                (initial_surface.clone(), initial_pane.clone())
            } else {
                let direction = pane.split.as_deref().unwrap_or("right");
                let mut args = vec!["new-split", direction, "--workspace", &ws_ref];
                if let Some(parent) = &pane.parent {
                    if let Some((parent_surface, _)) = pane_info.get(parent) {
                        args.extend(["--surface", parent_surface.as_str()]);
                    }
                }
                let split_output = self.cmux_cmd(&args).await?;
                let new_surface = Self::parse_ok_ref(&split_output);

                // Look up pane_ref for this new surface
                let panels_json = self
                    .cmux_cmd(&["--json", "list-panels", "--workspace", &ws_ref])
                    .await?;
                let panels: serde_json::Value =
                    serde_json::from_str(&panels_json).map_err(|e| e.to_string())?;
                let pane_ref = panels["surfaces"]
                    .as_array()
                    .and_then(|surfs| {
                        surfs
                            .iter()
                            .find(|s| s["ref"].as_str() == Some(&new_surface))
                    })
                    .and_then(|s| s["pane_ref"].as_str())
                    .ok_or(format!("no pane_ref for {}", new_surface))?
                    .to_string();

                (new_surface, pane_ref)
            };

            pane_info.insert(
                pane.name.clone(),
                (split_surface_ref.clone(), pane_ref.clone()),
            );

            // Process surfaces (tabs) for this pane
            for (j, surface) in pane.surfaces.iter().enumerate() {
                let surface_ref = if j == 0 {
                    split_surface_ref.clone()
                } else {
                    let output = self
                        .cmux_cmd(&[
                            "new-surface",
                            "--type",
                            "terminal",
                            "--pane",
                            &pane_ref,
                            "--workspace",
                            &ws_ref,
                        ])
                        .await?;
                    Self::parse_ok_ref(&output)
                };

                let quoted_dir = Self::shell_quote(&working_dir.display().to_string());
                let cmd = if surface.command.is_empty() {
                    format!("cd {}", quoted_dir)
                } else {
                    format!("cd {} && {}", quoted_dir, surface.command)
                };
                surface_cmds.push((surface_ref.clone(), cmd));

                if surface.active {
                    active_surfaces.push((surface_ref, pane_ref.clone(), j));
                }
            }

            if pane.focus {
                focus_pane = Some(pane_ref.clone());
            }
        }

        // Send commands to each surface
        for (surface_ref, cmd) in &surface_cmds {
            self.cmux_cmd(&[
                "send",
                "--workspace",
                &ws_ref,
                "--surface",
                surface_ref,
                &format!("{cmd}\n"),
            ])
            .await?;
        }

        // Select active surfaces within their panes, then restore tab order
        for (surface_ref, pane_ref, tab_index) in &active_surfaces {
            self.cmux_cmd(&[
                "move-surface",
                "--surface",
                surface_ref,
                "--pane",
                pane_ref,
                "--focus",
                "true",
                "--workspace",
                &ws_ref,
            ])
            .await?;
            self.cmux_cmd(&[
                "reorder-surface",
                "--surface",
                surface_ref,
                "--index",
                &tab_index.to_string(),
                "--workspace",
                &ws_ref,
            ])
            .await?;
        }

        // Focus the designated pane last (for keyboard focus)
        if let Some(pane_ref) = &focus_pane {
            self.cmux_cmd(&["focus-pane", "--pane", pane_ref, "--workspace", &ws_ref])
                .await?;
        }

        let directories = vec![config.working_directory.clone()];
        let correlation_keys = directories
            .iter()
            .map(|d| CorrelationKey::CheckoutPath(d.clone()))
            .collect();

        info!(workspace = %config.name, %ws_ref, "cmux: workspace ready");
        Ok((
            ws_ref,
            Workspace {
                name: config.name.clone(),
                directories,
                correlation_keys,
            },
        ))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!(%ws_ref, "cmux: switching to workspace");
        self.cmux_cmd(&["select-workspace", "--workspace", ws_ref])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::providers::testing::MockRunner;
    use crate::providers::workspace::WorkspaceManager;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(
            CmuxWorkspaceManager::shell_quote("a'b c"),
            "'a'\\''b c'".to_string()
        );
    }

    #[test]
    fn parse_ok_ref_extracts_first_token() {
        assert_eq!(
            CmuxWorkspaceManager::parse_ok_ref("OK workspace:42"),
            "workspace:42"
        );
        // Defensive fallback for unexpected output without the "OK " prefix.
        assert_eq!(CmuxWorkspaceManager::parse_ok_ref("surface:7"), "surface:7");
        assert_eq!(CmuxWorkspaceManager::parse_ok_ref(""), "");
    }

    #[tokio::test]
    async fn list_workspaces_parses_json_response() {
        let manager = CmuxWorkspaceManager::new(Arc::new(MockRunner::new(vec![Ok(
            r#"{"workspaces":[{"ref":"workspace:10","title":"Main","directories":["/tmp/repo","/tmp/repo2"]}]}"#.to_string(),
        )])));

        let workspaces = manager.list_workspaces().await.expect("list workspaces");
        assert_eq!(workspaces.len(), 1);
        let (ws_ref, ws) = &workspaces[0];
        assert_eq!(ws_ref, "workspace:10");
        assert_eq!(ws.name, "Main");
        assert_eq!(
            ws.directories,
            vec![PathBuf::from("/tmp/repo"), PathBuf::from("/tmp/repo2")]
        );
        assert_eq!(ws.correlation_keys.len(), 2);
    }

    #[tokio::test]
    async fn create_workspace_returns_error_when_ref_missing() {
        let manager =
            CmuxWorkspaceManager::new(Arc::new(MockRunner::new(vec![Ok("".to_string())])));
        let config = WorkspaceConfig {
            name: "demo".into(),
            working_directory: PathBuf::from("/tmp/repo"),
            template_vars: std::collections::HashMap::new(),
            template_yaml: None,
            resolved_commands: None,
        };

        let err = manager
            .create_workspace(&config)
            .await
            .expect_err("should fail when ref is missing");
        assert!(err.contains("returned no workspace ref"));
    }
}
