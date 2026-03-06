use std::path::{Path, PathBuf};
use async_trait::async_trait;
use crate::providers::types::*;

pub struct GitVcs;

use crate::providers::run_cmd;

impl Default for GitVcs {
    fn default() -> Self {
        Self::new()
    }
}

impl GitVcs {
    pub fn new() -> Self {
        Self
    }
}

use super::TRUNK_NAMES;

#[async_trait]
impl super::Vcs for GitVcs {
    fn display_name(&self) -> &str {
        "Git"
    }

    fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf> {
        // git-common-dir points to the shared .git dir (same as .git for
        // non-worktree repos, the main repo's .git for worktrees).
        // --path-format=absolute requires git >= 2.31 (Feb 2021).
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let git_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

        // For bare repos, git-common-dir IS the repo directory itself (e.g.
        // foo.git), so calling parent() would give the containing directory
        // rather than the repo root.  Detect bare repos and return git_dir
        // directly in that case.
        let bare_output = std::process::Command::new("git")
            .args(["rev-parse", "--is-bare-repository"])
            .current_dir(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let is_bare = bare_output.status.success()
            && String::from_utf8_lossy(&bare_output.stdout).trim() == "true";

        if is_bare {
            Some(git_dir)
        } else {
            // The repo root is the parent of the .git directory.
            git_dir.parent().map(|p| p.to_path_buf())
        }
    }

    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>, String> {
        let output = run_cmd(
                "git",
                &["branch", "--list", "--format=%(refname:short)"],
                repo_root,
            )
            .await?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|name| {
                let name = name.trim().to_string();
                let is_trunk = TRUNK_NAMES.contains(&name.as_str());
                BranchInfo { name, is_trunk }
            })
            .collect())
    }

    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>, String> {
        // Check if any remote exists; return empty if not (local-only repo).
        let remotes = run_cmd("git", &["remote"], repo_root).await.unwrap_or_default();
        if remotes.trim().is_empty() {
            return Ok(vec![]);
        }
        let remote = remotes.lines().next().unwrap_or("origin");
        let output = run_cmd("git", &["ls-remote", "--heads", remote], repo_root)
            .await?;
        // Output format: "<sha>\trefs/heads/<branch>"
        Ok(output
            .lines()
            .filter_map(|line| {
                line.split('\t')
                    .nth(1)
                    .and_then(|r| r.strip_prefix("refs/heads/"))
                    .map(|s| s.to_string())
            })
            .collect())
    }

    async fn commit_log(
        &self,
        repo_root: &Path,
        branch: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, String> {
        let limit_arg = format!("-{}", limit);
        let output = run_cmd(
                "git",
                &["log", branch, "--oneline", &limit_arg],
                repo_root,
            )
            .await?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut parts = line.splitn(2, ' ');
                let short_sha = parts.next().unwrap_or("").to_string();
                let message = parts.next().unwrap_or("").to_string();
                CommitInfo { short_sha, message }
            })
            .collect())
    }

    async fn ahead_behind(
        &self,
        repo_root: &Path,
        branch: &str,
        reference: &str,
    ) -> Result<AheadBehind, String> {
        let range = format!("{}...{}", branch, reference);
        let output = run_cmd(
                "git",
                &["rev-list", "--count", "--left-right", &range],
                repo_root,
            )
            .await?;
        let trimmed = output.trim();
        let mut parts = trimmed.split('\t');
        let ahead: i64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let behind: i64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(AheadBehind { ahead, behind })
    }

    async fn working_tree_status(
        &self,
        _repo_root: &Path,
        checkout_path: &Path,
    ) -> Result<WorkingTreeStatus, String> {
        let output = run_cmd("git", &["status", "--porcelain"], checkout_path)
            .await?;
        let mut status = WorkingTreeStatus::default();
        for line in output.lines() {
            if line.len() < 2 {
                continue;
            }
            let index = line.as_bytes()[0];
            let worktree = line.as_bytes()[1];
            if index == b'?' && worktree == b'?' {
                status.untracked += 1;
            } else {
                if index != b' ' && index != b'?' {
                    status.staged += 1;
                }
                if worktree != b' ' && worktree != b'?' {
                    status.modified += 1;
                }
            }
        }
        Ok(status)
    }
}
