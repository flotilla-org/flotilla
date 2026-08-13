use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{run, types::*, CommandRunner},
};

pub struct WtCheckoutManager {
    runner: Arc<dyn CommandRunner>,
}

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
    fn into_checkout(self) -> (ExecutionEnvironmentPath, Checkout) {
        let ee_path = ExecutionEnvironmentPath::new(self.path);
        (ee_path, Checkout {
            branch: self.branch,
            is_main: self.is_main,
            trunk_ahead_behind: self.main.map(|m| AheadBehind { ahead: m.ahead, behind: m.behind }),
            remote_ahead_behind: self.remote.map(|r| AheadBehind { ahead: r.ahead, behind: r.behind }),
            working_tree: self.working_tree.map(|w| WorkingTreeStatus {
                staged: if w.staged { 1 } else { 0 },
                modified: if w.modified { 1 } else { 0 },
                untracked: if w.untracked { 1 } else { 0 },
            }),
            last_commit: self
                .commit
                .map(|c| CommitInfo { short_sha: c.short_sha.unwrap_or_default(), message: c.message.unwrap_or_default() }),
            host_name: None,
            environment_id: None,
        })
    }
}

impl WtCheckoutManager {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Strip ANSI escape codes that `wt` may append after JSON output.
    fn strip_to_json(output: &str) -> &str {
        let end = output.rfind(']').map(|i| i + 1).unwrap_or(output.len());
        &output[..end]
    }
}

#[async_trait]
impl super::CheckoutManager for WtCheckoutManager {
    async fn validate_target(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        super::validate_checkout_target_in_repo(repo_root.as_path(), branch, intent, &*self.runner).await
    }

    async fn list_checkouts(&self, repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        let root = repo_root.as_path();
        let output = run!(self.runner, "wt", &["list", "--format=json"], root)?;
        let json = Self::strip_to_json(&output);
        let worktrees: Vec<WtWorktree> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(worktrees.into_iter().map(WtWorktree::into_checkout).collect())
    }

    async fn create_checkout(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        let root = repo_root.as_path();
        info!(%branch, %create_branch, "wt: creating worktree");

        // Check if a remote-tracking branch exists. If so, use `wt switch`
        // (without --create) so wt tracks the remote branch instead of
        // creating a brand new one from the default branch.
        let remote_exists = if create_branch {
            run!(self.runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}"),], root,).is_ok()
        } else {
            false
        };

        if create_branch && !remote_exists {
            run!(self.runner, "wt", &["switch", "--create", branch, "--no-cd"], root)?;
        } else {
            run!(self.runner, "wt", &["switch", branch, "--no-cd", "--yes"], root)?;
        }

        // Look up the path of the newly created worktree
        let list_output = run!(self.runner, "wt", &["list", "--format=json"], root)?;
        let json = Self::strip_to_json(&list_output);
        let worktrees: Vec<WtWorktree> = serde_json::from_str(json).map_err(|e| e.to_string())?;

        for wt in worktrees {
            if wt.branch == branch || wt.branch.ends_with(&format!("/{branch}")) {
                info!(%branch, path = %wt.path.display(), "wt: created worktree");
                return Ok(wt.into_checkout());
            }
        }

        Err("Could not find worktree path after creation".to_string())
    }

    async fn remove_checkout(&self, repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        let root = repo_root.as_path();
        info!(%branch, "wt: removing worktree");
        run!(self.runner, "wt", &["remove", branch], root)?;
        Ok(())
    }
}
