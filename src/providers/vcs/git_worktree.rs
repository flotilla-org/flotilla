use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::info;

use crate::config::CheckoutsConfig;
use crate::providers::run_cmd;
use crate::providers::types::*;

pub struct GitCheckoutManager {
    config: CheckoutsConfig,
}

impl GitCheckoutManager {
    pub fn new(config: CheckoutsConfig) -> Self {
        Self { config }
    }

    /// Render the worktree path template for a given repo and branch.
    fn render_worktree_path(&self, repo_root: &Path, branch: &str) -> Result<PathBuf, String> {
        let repo_name = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());

        let mut env = minijinja::Environment::new();
        env.add_filter("sanitize", |value: String| -> String {
            value.replace(['/', '\\'], "-")
        });
        env.add_template("path", &self.config.path)
            .map_err(|e| format!("invalid worktree path template: {e}"))?;
        let tmpl = env.get_template("path").map_err(|e| e.to_string())?;
        let rendered = tmpl
            .render(minijinja::context! {
                repo_path => repo_root.to_string_lossy(),
                repo => repo_name,
                branch => branch,
            })
            .map_err(|e| format!("failed to render worktree path: {e}"))?;

        let path = PathBuf::from(rendered.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            repo_root.join(&path)
        })
    }

    /// Parse `git worktree list --porcelain` output into (path, branch, is_bare) tuples.
    fn parse_porcelain(output: &str) -> Vec<(PathBuf, String, bool)> {
        let mut results = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut bare = false;

        for line in output.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if let (Some(p), Some(b)) = (path.take(), branch.take()) {
                    results.push((p, b, bare));
                }
                path = Some(PathBuf::from(p));
                branch = None;
                bare = false;
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line == "bare" {
                bare = true;
            }
        }
        if let (Some(p), Some(b)) = (path, branch) {
            results.push((p, b, bare));
        }
        results
    }

    /// Detect the default branch (main or master) for trunk detection.
    async fn default_branch(repo_root: &Path) -> String {
        if let Ok(out) = run_cmd(
            "git",
            &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
            repo_root,
        )
        .await
        {
            let trimmed = out.trim();
            if let Some(branch) = trimmed.strip_prefix("origin/") {
                return branch.to_string();
            }
        }
        "main".to_string()
    }

    /// Gather detailed info for a single worktree checkout.
    async fn enrich_checkout(
        _repo_root: &Path,
        path: &Path,
        branch: &str,
        is_trunk: bool,
        default_branch: &str,
    ) -> Checkout {
        let correlation_keys = vec![
            CorrelationKey::Branch(branch.to_string()),
            CorrelationKey::CheckoutPath(path.to_path_buf()),
        ];

        let trunk_ahead_behind = if !is_trunk {
            run_cmd(
                "git",
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("HEAD...{default_branch}"),
                ],
                path,
            )
            .await
            .ok()
            .and_then(|out| parse_ahead_behind(&out))
        } else {
            None
        };

        let remote_ahead_behind = run_cmd(
            "git",
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("HEAD...origin/{branch}"),
            ],
            path,
        )
        .await
        .ok()
        .and_then(|out| parse_ahead_behind(&out));

        let working_tree = run_cmd("git", &["status", "--porcelain"], path)
            .await
            .ok()
            .map(|out| parse_working_tree(&out));

        let last_commit = run_cmd(
            "git",
            &["log", "-1", "--format=%h\t%s"],
            path,
        )
        .await
        .ok()
        .and_then(|out| {
            let trimmed = out.trim();
            let (sha, msg) = trimmed.split_once('\t')?;
            Some(CommitInfo {
                short_sha: sha.to_string(),
                message: msg.to_string(),
            })
        });

        Checkout {
            branch: branch.to_string(),
            path: path.to_path_buf(),
            is_trunk,
            trunk_ahead_behind,
            remote_ahead_behind,
            working_tree,
            last_commit,
            correlation_keys,
        }
    }
}

fn parse_ahead_behind(output: &str) -> Option<AheadBehind> {
    let parts: Vec<&str> = output.trim().split_whitespace().collect();
    if parts.len() == 2 {
        Some(AheadBehind {
            ahead: parts[0].parse().ok()?,
            behind: parts[1].parse().ok()?,
        })
    } else {
        None
    }
}

fn parse_working_tree(output: &str) -> WorkingTreeStatus {
    let mut staged = 0usize;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    for line in output.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        let x = bytes[0];
        let y = bytes[1];
        if x == b'?' {
            untracked += 1;
        } else {
            if x != b' ' && x != b'?' {
                staged += 1;
            }
            if y != b' ' && y != b'?' {
                modified += 1;
            }
        }
    }
    WorkingTreeStatus {
        staged,
        modified,
        untracked,
    }
}

#[async_trait]
impl super::CheckoutManager for GitCheckoutManager {
    fn display_name(&self) -> &str {
        "git"
    }

    fn section_label(&self) -> &str {
        "Worktrees"
    }
    fn item_noun(&self) -> &str {
        "worktree"
    }
    fn abbreviation(&self) -> &str {
        "WT"
    }

    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<Checkout>, String> {
        let output = run_cmd("git", &["worktree", "list", "--porcelain"], repo_root).await?;
        let entries = Self::parse_porcelain(&output);
        let default_branch = Self::default_branch(repo_root).await;

        let mut checkouts = Vec::new();
        for (path, branch, _bare) in &entries {
            let is_trunk = *branch == default_branch;
            let checkout = Self::enrich_checkout(
                repo_root,
                path,
                branch,
                is_trunk,
                &default_branch,
            )
            .await;
            checkouts.push(checkout);
        }
        Ok(checkouts)
    }

    async fn create_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<Checkout, String> {
        let wt_path = self.render_worktree_path(repo_root, branch)?;
        info!("git: creating worktree for {branch} at {}", wt_path.display());

        let branch_exists = run_cmd(
            "git",
            &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")],
            repo_root,
        )
        .await
        .is_ok();

        if branch_exists {
            run_cmd(
                "git",
                &["worktree", "add", wt_path.to_str().unwrap_or(""), branch],
                repo_root,
            )
            .await?;
        } else {
            run_cmd(
                "git",
                &["worktree", "add", "-b", branch, wt_path.to_str().unwrap_or("")],
                repo_root,
            )
            .await?;
        }

        let default_branch = Self::default_branch(repo_root).await;
        let is_trunk = branch == default_branch;
        Ok(Self::enrich_checkout(repo_root, &wt_path, branch, is_trunk, &default_branch).await)
    }

    async fn remove_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<(), String> {
        info!("git: removing worktree for {branch}");

        let output = run_cmd("git", &["worktree", "list", "--porcelain"], repo_root).await?;
        let entries = Self::parse_porcelain(&output);
        let wt_path = entries
            .iter()
            .find(|(_, b, _)| b == branch)
            .map(|(p, _, _)| p.clone())
            .ok_or_else(|| format!("no worktree found for branch {branch}"))?;

        run_cmd(
            "git",
            &["worktree", "remove", wt_path.to_str().unwrap_or("")],
            repo_root,
        )
        .await?;

        let _ = run_cmd("git", &["branch", "-D", branch], repo_root).await;

        Ok(())
    }
}
