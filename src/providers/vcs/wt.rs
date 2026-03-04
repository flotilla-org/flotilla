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
    #[serde(default)]
    #[allow(dead_code)]
    is_current: bool,
    #[serde(default)]
    main: Option<WtAheadBehind>,
    #[serde(default)]
    remote: Option<WtRemote>,
    #[serde(default)]
    working_tree: Option<WtWorkingTree>,
    #[serde(default)]
    commit: Option<WtCommit>,
}

#[derive(Debug, Deserialize)]
struct WtAheadBehind {
    ahead: i64,
    behind: i64,
}

#[derive(Debug, Deserialize)]
struct WtRemote {
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    branch: Option<String>,
    ahead: i64,
    behind: i64,
}

#[derive(Debug, Deserialize)]
struct WtWorkingTree {
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    modified: bool,
    #[serde(default)]
    untracked: bool,
}

#[derive(Debug, Deserialize)]
struct WtCommit {
    short_sha: Option<String>,
    message: Option<String>,
}

impl WtWorktree {
    fn into_checkout(self) -> Checkout {
        let correlation_keys = vec![
            CorrelationKey::Branch(self.branch.clone()),
            CorrelationKey::RepoPath(self.path.clone()),
        ];
        Checkout {
            branch: self.branch,
            path: self.path,
            is_trunk: self.is_main,
            trunk_ahead_behind: self.main.map(|m| AheadBehind { ahead: m.ahead, behind: m.behind }),
            remote_ahead_behind: self.remote.map(|r| AheadBehind { ahead: r.ahead, behind: r.behind }),
            working_tree: self.working_tree.map(|w| WorkingTreeStatus {
                staged: if w.staged { 1 } else { 0 },
                modified: if w.modified { 1 } else { 0 },
                untracked: if w.untracked { 1 } else { 0 },
            }),
            last_commit: self.commit.map(|c| CommitInfo {
                short_sha: c.short_sha.unwrap_or_default(),
                message: c.message.unwrap_or_default(),
            }),
            correlation_keys,
        }
    }
}

use crate::providers::run_cmd;

impl WtCheckoutManager {
    pub fn new() -> Self {
        Self
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
        let output = run_cmd("wt", &["list", "--format=json"], repo_root)
            .await?;
        let json = Self::strip_to_json(&output);
        let worktrees: Vec<WtWorktree> =
            serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(worktrees
            .into_iter()
            .map(|wt| wt.into_checkout())
            .collect())
    }

    async fn create_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<Checkout, String> {
        // Create the worktree via `wt switch --create <branch> --no-cd`
        run_cmd(
            "wt",
            &["switch", "--create", branch, "--no-cd"],
            repo_root,
        )
        .await?;

        // Look up the path of the newly created worktree
        let list_output = run_cmd("wt", &["list", "--format=json"], repo_root)
            .await?;
        let json = Self::strip_to_json(&list_output);
        let worktrees: Vec<WtWorktree> =
            serde_json::from_str(json).map_err(|e| e.to_string())?;

        for wt in worktrees {
            if wt.branch == branch || wt.branch.ends_with(branch) {
                return Ok(wt.into_checkout());
            }
        }

        Err("Could not find worktree path after creation".to_string())
    }

    async fn remove_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<(), String> {
        run_cmd("wt", &["remove", branch], repo_root).await?;
        Ok(())
    }
}
