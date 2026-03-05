use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::info;

use crate::config::CheckoutsConfig;
use crate::providers::run_cmd;
use crate::providers::types::*;

pub struct GitCheckoutManager {
    config: CheckoutsConfig,
    env: minijinja::Environment<'static>,
}

impl GitCheckoutManager {
    pub fn new(config: CheckoutsConfig) -> Self {
        let mut env = minijinja::Environment::new();
        env.add_filter("sanitize", |value: String| -> String {
            value.replace(['/', '\\'], "-")
        });
        Self { config, env }
    }

    /// Render the worktree path template for a given repo and branch.
    fn render_worktree_path(&self, repo_root: &Path, branch: &str) -> Result<PathBuf, String> {
        let repo_name = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());

        let rendered = self.env
            .render_str(
                &self.config.path,
                minijinja::context! {
                    repo_path => repo_root.to_string_lossy(),
                    repo => repo_name,
                    branch => branch,
                },
            )
            .map_err(|e| format!("failed to render worktree path: {e}"))?;

        let path = PathBuf::from(rendered.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            repo_root.join(&path)
        })
    }

    /// Parse `git worktree list --porcelain` output into (path, branch) tuples.
    /// Entries without a branch (detached HEAD, bare) use a synthetic label.
    fn parse_porcelain(output: &str) -> Vec<(PathBuf, String)> {
        let mut results = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut head_sha: Option<String> = None;

        let flush = |results: &mut Vec<(PathBuf, String)>,
                     path: Option<PathBuf>,
                     branch: Option<String>,
                     head_sha: Option<String>| {
            if let Some(p) = path {
                let label = branch.unwrap_or_else(|| {
                    head_sha
                        .map(|sha| format!("(detached: {})", &sha[..sha.len().min(7)]))
                        .unwrap_or_else(|| "(unknown)".to_string())
                });
                results.push((p, label));
            }
        };

        for line in output.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                flush(&mut results, path.take(), branch.take(), head_sha.take());
                path = Some(PathBuf::from(p));
                branch = None;
                head_sha = None;
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if let Some(sha) = line.strip_prefix("HEAD ") {
                head_sha = Some(sha.to_string());
            } else if line == "bare" {
                // Skip bare worktrees — not useful as checkouts
                path = None;
            }
        }
        flush(&mut results, path, branch, head_sha);
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
        path: &Path,
        branch: &str,
        is_trunk: bool,
        default_branch: &str,
    ) -> Checkout {
        let correlation_keys = vec![
            CorrelationKey::Branch(branch.to_string()),
            CorrelationKey::CheckoutPath(path.to_path_buf()),
        ];

        let trunk_ref = format!("HEAD...{default_branch}");
        let remote_ref = format!("HEAD...origin/{branch}");

        let (trunk_ab, remote_ab, wt_status, commit) = tokio::join!(
            async {
                if !is_trunk {
                    run_cmd(
                        "git",
                        &["rev-list", "--left-right", "--count", &trunk_ref],
                        path,
                    )
                    .await
                    .ok()
                    .and_then(|out| parse_ahead_behind(&out))
                } else {
                    None
                }
            },
            async {
                run_cmd(
                    "git",
                    &["rev-list", "--left-right", "--count", &remote_ref],
                    path,
                )
                .await
                .ok()
                .and_then(|out| parse_ahead_behind(&out))
            },
            async {
                run_cmd("git", &["status", "--porcelain"], path)
                    .await
                    .ok()
                    .map(|out| parse_working_tree(&out))
            },
            async {
                run_cmd("git", &["log", "-1", "--format=%h\t%s"], path)
                    .await
                    .ok()
                    .and_then(|out| {
                        let trimmed = out.trim();
                        let (sha, msg) = trimmed.split_once('\t')?;
                        Some(CommitInfo {
                            short_sha: sha.to_string(),
                            message: msg.to_string(),
                        })
                    })
            },
        );

        Checkout {
            branch: branch.to_string(),
            path: path.to_path_buf(),
            is_trunk,
            trunk_ahead_behind: trunk_ab,
            remote_ahead_behind: remote_ab,
            working_tree: wt_status,
            last_commit: commit,
            correlation_keys,
        }
    }
}

fn parse_ahead_behind(output: &str) -> Option<AheadBehind> {
    let parts: Vec<&str> = output.split_whitespace().collect();
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

        let futures: Vec<_> = entries
            .iter()
            .map(|(path, branch)| {
                let is_trunk = *branch == default_branch;
                Self::enrich_checkout(path, branch, is_trunk, &default_branch)
            })
            .collect();
        Ok(futures::future::join_all(futures).await)
    }

    async fn create_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<Checkout, String> {
        let wt_path = self.render_worktree_path(repo_root, branch)?;
        info!("git: creating worktree for {branch} at {}", wt_path.display());

        let wt_str = wt_path.to_str()
            .ok_or_else(|| format!("worktree path is not valid UTF-8: {}", wt_path.display()))?;

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
                &["worktree", "add", wt_str, branch],
                repo_root,
            )
            .await?;
        } else {
            run_cmd(
                "git",
                &["worktree", "add", "-b", branch, wt_str],
                repo_root,
            )
            .await?;
        }

        let default_branch = Self::default_branch(repo_root).await;
        let is_trunk = branch == default_branch;
        Ok(Self::enrich_checkout(&wt_path, branch, is_trunk, &default_branch).await)
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
            .find(|(_, b)| b == branch)
            .map(|(p, _)| p.clone())
            .ok_or_else(|| format!("no worktree found for branch {branch}"))?;

        let wt_str = wt_path.to_str()
            .ok_or_else(|| format!("worktree path is not valid UTF-8: {}", wt_path.display()))?;

        run_cmd(
            "git",
            &["worktree", "remove", wt_str],
            repo_root,
        )
        .await?;

        // Force-delete branch (-D) since feature branches are typically
        // unmerged locally. Skip trunk to prevent catastrophic deletion.
        let default_branch = Self::default_branch(repo_root).await;
        if branch != default_branch {
            let _ = run_cmd("git", &["branch", "-D", branch], repo_root).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_normal_worktrees() {
        let output = "\
worktree /home/user/repo
HEAD abc1234567890
branch refs/heads/main

worktree /home/user/repo.feature-x
HEAD def4567890123
branch refs/heads/feature/x
";
        let entries = GitCheckoutManager::parse_porcelain(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, PathBuf::from("/home/user/repo"));
        assert_eq!(entries[0].1, "main");
        assert_eq!(entries[1].0, PathBuf::from("/home/user/repo.feature-x"));
        assert_eq!(entries[1].1, "feature/x");
    }

    #[test]
    fn parse_porcelain_detached_head() {
        let output = "\
worktree /home/user/repo
HEAD abc1234567890
branch refs/heads/main

worktree /home/user/repo.detached
HEAD def4567890123
detached
";
        let entries = GitCheckoutManager::parse_porcelain(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, "main");
        assert_eq!(entries[1].1, "(detached: def4567)");
    }

    #[test]
    fn parse_porcelain_bare_repo_skipped() {
        let output = "\
worktree /home/user/repo.git
HEAD abc1234567890
bare

worktree /home/user/repo.feature
HEAD def4567890123
branch refs/heads/feature
";
        let entries = GitCheckoutManager::parse_porcelain(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "feature");
    }

    #[test]
    fn parse_ahead_behind_valid() {
        assert!(parse_ahead_behind("3\t5\n").is_some());
        let ab = parse_ahead_behind("3\t5\n").unwrap();
        assert_eq!(ab.ahead, 3);
        assert_eq!(ab.behind, 5);
    }

    #[test]
    fn parse_ahead_behind_empty() {
        assert!(parse_ahead_behind("").is_none());
        assert!(parse_ahead_behind("just-one").is_none());
    }

    #[test]
    fn parse_working_tree_mixed() {
        let output = "M  src/main.rs\n?? new_file.rs\nA  added.rs\n M modified.rs\n";
        let wt = parse_working_tree(output);
        assert_eq!(wt.staged, 2); // M and A in index column
        assert_eq!(wt.modified, 1); // M in worktree column
        assert_eq!(wt.untracked, 1); // ??
    }

    #[test]
    fn parse_working_tree_empty() {
        let wt = parse_working_tree("");
        assert_eq!(wt.staged, 0);
        assert_eq!(wt.modified, 0);
        assert_eq!(wt.untracked, 0);
    }
}
