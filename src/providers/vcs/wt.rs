use std::path::{Path, PathBuf};
use async_trait::async_trait;
use serde::Deserialize;
use crate::providers::types::*;

pub struct WtCheckoutManager;

#[derive(Debug, Deserialize)]
struct WtWorktree {
    branch: String,
    path: PathBuf,
    #[serde(default)]
    is_main: bool,
}

impl WtCheckoutManager {
    pub fn new() -> Self {
        Self
    }

    async fn run_cmd(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<String, String> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    /// Strip ANSI escape codes that `wt` may append after JSON output.
    fn strip_to_json(output: &str) -> &str {
        let end = output.rfind(']').map(|i| i + 1).unwrap_or(output.len());
        &output[..end]
    }
}

#[async_trait]
impl super::CheckoutManager for WtCheckoutManager {
    fn display_name(&self) -> &str {
        "wt"
    }

    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<Checkout>, String> {
        let output = self
            .run_cmd("wt", &["list", "--format=json"], repo_root)
            .await?;
        let json = Self::strip_to_json(&output);
        let worktrees: Vec<WtWorktree> =
            serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(worktrees
            .into_iter()
            .map(|wt| {
                let correlation_keys = vec![
                    CorrelationKey::Branch(wt.branch.clone()),
                    CorrelationKey::RepoPath(wt.path.clone()),
                ];
                Checkout {
                    branch: wt.branch,
                    path: wt.path,
                    is_trunk: wt.is_main,
                    correlation_keys,
                }
            })
            .collect())
    }

    async fn create_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<Checkout, String> {
        // Create the worktree via `wt switch --create <branch> --no-cd`
        self.run_cmd(
            "wt",
            &["switch", "--create", branch, "--no-cd"],
            repo_root,
        )
        .await?;

        // Look up the path of the newly created worktree
        let list_output = self
            .run_cmd("wt", &["list", "--format=json"], repo_root)
            .await?;
        let json = Self::strip_to_json(&list_output);
        let worktrees: Vec<WtWorktree> =
            serde_json::from_str(json).map_err(|e| e.to_string())?;

        for wt in &worktrees {
            if wt.branch == branch || wt.branch.ends_with(branch) {
                let correlation_keys = vec![
                    CorrelationKey::Branch(wt.branch.clone()),
                    CorrelationKey::RepoPath(wt.path.clone()),
                ];
                return Ok(Checkout {
                    branch: wt.branch.clone(),
                    path: wt.path.clone(),
                    is_trunk: wt.is_main,
                    correlation_keys,
                });
            }
        }

        Err("Could not find worktree path after creation".to_string())
    }

    async fn remove_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<(), String> {
        self.run_cmd("wt", &["remove", branch], repo_root).await?;
        Ok(())
    }
}
