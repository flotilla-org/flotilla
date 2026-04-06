use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use tracing::info;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{run, types::*, CommandRunner, TaskId},
};

/// Resolve `.` and `..` components in-place without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub struct GitCheckoutManager {
    checkout_path: String,
    env: minijinja::Environment<'static>,
    runner: Arc<dyn CommandRunner>,
}

impl GitCheckoutManager {
    pub fn new(checkout_path: String, runner: Arc<dyn CommandRunner>) -> Self {
        let mut env = minijinja::Environment::new();
        env.add_filter("sanitize", |value: String| -> String { value.replace(['/', '\\'], "-") });
        Self { checkout_path, env, runner }
    }

    /// Render the worktree path template for a given repo and branch.
    ///
    /// The result is normalized to resolve `.` and `..` components so that the
    /// returned path matches what `git worktree list` reports (which always
    /// returns canonical absolute paths).
    fn render_worktree_path(&self, repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<ExecutionEnvironmentPath, String> {
        let root = repo_root.as_path();
        let repo_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".to_string());

        let rendered = self
            .env
            .render_str(&self.checkout_path, minijinja::context! {
                repo_path => root.to_string_lossy(),
                repo => repo_name,
                branch => branch,
            })
            .map_err(|e| format!("failed to render worktree path: {e}"))?;

        let path = PathBuf::from(rendered.trim());
        let absolute = if path.is_absolute() { path } else { root.join(&path) };
        Ok(ExecutionEnvironmentPath::new(normalize_path(&absolute)))
    }

    /// Parse `git worktree list --porcelain` output into (path, branch) tuples.
    /// Entries without a branch (detached HEAD, bare) use a synthetic label.
    fn parse_porcelain(output: &str) -> Vec<(PathBuf, String)> {
        let mut results = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut head_sha: Option<String> = None;

        let flush = |results: &mut Vec<(PathBuf, String)>, path: Option<PathBuf>, branch: Option<String>, head_sha: Option<String>| {
            if let Some(p) = path {
                let label = branch.unwrap_or_else(|| {
                    head_sha.map(|sha| format!("(detached: {})", &sha[..sha.len().min(7)])).unwrap_or_else(|| "(unknown)".to_string())
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
                // Skip bare worktrees — not useful as checkouts.
                // Clear all state so flush() is a no-op.
                path = None;
                branch = None;
                head_sha = None;
            }
        }
        flush(&mut results, path, branch, head_sha);
        results
    }

    /// Detect the default branch for trunk detection.
    async fn default_branch(&self, repo_root: &ExecutionEnvironmentPath) -> String {
        let root = repo_root.as_path();
        if let Ok(out) = run!(self.runner, "git", &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"], root) {
            let trimmed = out.trim();
            if let Some(branch) = trimmed.strip_prefix("origin/") {
                return branch.to_string();
            }
        }
        // Fallback: check which common trunk names exist locally
        for name in super::TRUNK_NAMES {
            if run!(self.runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}"),], root,).is_ok() {
                return name.to_string();
            }
        }
        "main".to_string()
    }

    /// Gather detailed info for a single worktree checkout.
    async fn enrich_checkout(
        &self,
        path: &ExecutionEnvironmentPath,
        branch: &str,
        is_main: bool,
        default_branch: &str,
    ) -> (ExecutionEnvironmentPath, Checkout) {
        let host_path = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), path.as_path());
        let correlation_keys = vec![CorrelationKey::Branch(branch.to_string()), CorrelationKey::CheckoutPath(host_path.into())];

        let trunk_ref = format!("HEAD...{default_branch}");
        let remote_ref = format!("HEAD...origin/{branch}");
        let raw = path.as_path();

        let (trunk_ab, remote_ab, wt_status, commit, issue_links) = tokio::join!(
            async {
                if !is_main {
                    run!(self.runner, "git", &["rev-list", "--left-right", "--count", &trunk_ref], raw, TaskId("trunk-ab"))
                        .ok()
                        .and_then(|out| super::parse_ahead_behind(&out))
                } else {
                    None
                }
            },
            async {
                run!(self.runner, "git", &["rev-list", "--left-right", "--count", &remote_ref], raw, TaskId("remote-ab"))
                    .ok()
                    .and_then(|out| super::parse_ahead_behind(&out))
            },
            async { run!(self.runner, "git", &["status", "--porcelain"], raw).ok().map(|out| super::parse_porcelain_status(&out)) },
            async {
                run!(self.runner, "git", &["log", "-1", "--format=%h\t%s"], raw).ok().and_then(|out| {
                    let trimmed = out.trim();
                    let (sha, msg) = trimmed.split_once('\t')?;
                    Some(CommitInfo { short_sha: sha.to_string(), message: msg.to_string() })
                })
            },
            async { super::read_branch_issue_links(raw, branch, &*self.runner).await },
        );

        (path.clone(), Checkout {
            branch: branch.to_string(),
            is_main,
            trunk_ahead_behind: trunk_ab,
            remote_ahead_behind: remote_ab,
            working_tree: wt_status,
            last_commit: commit,
            correlation_keys,
            association_keys: issue_links,
            environment_id: None,
        })
    }
}

#[async_trait]
impl super::CheckoutManager for GitCheckoutManager {
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
        let output = run!(self.runner, "git", &["worktree", "list", "--porcelain"], root)?;
        let entries = Self::parse_porcelain(&output);
        let default_branch = self.default_branch(repo_root).await;

        let ee_entries: Vec<(ExecutionEnvironmentPath, String)> =
            entries.into_iter().map(|(path, branch)| (ExecutionEnvironmentPath::new(path), branch)).collect();

        let futures: Vec<_> = ee_entries
            .iter()
            .enumerate()
            .map(|(i, (path, branch))| {
                // The first worktree in porcelain output is always the main worktree
                let is_main = i == 0;
                self.enrich_checkout(path, branch, is_main, &default_branch)
            })
            .collect();
        Ok(futures::future::join_all(futures).await)
    }

    async fn create_checkout(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        _create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        let root = repo_root.as_path();
        let wt_path = self.render_worktree_path(repo_root, branch)?;
        info!(
            %branch, path = %wt_path, "git: creating worktree"
        );

        let wt_str = wt_path.as_path().to_str().ok_or_else(|| format!("worktree path is not valid UTF-8: {wt_path}"))?;

        let branch_exists = run!(self.runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}"),], root,).is_ok();

        let default_branch = self.default_branch(repo_root).await;

        if branch_exists {
            run!(self.runner, "git", &["worktree", "add", wt_str, branch], root)?;
        } else {
            // Check if a remote-tracking branch exists on origin.
            let remote_exists =
                run!(self.runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}"),], root,).is_ok();

            if remote_exists {
                // Fetch latest and create worktree tracking the remote branch.
                // If fetch fails (offline, etc), the local remote-tracking ref
                // is still usable — just potentially stale.
                if run!(self.runner, "git", &["fetch", "origin", branch], root).is_err() {
                    tracing::warn!(
                        %branch, "fetch from origin failed, using existing remote-tracking ref"
                    );
                }
                run!(self.runner, "git", &["worktree", "add", "-b", branch, wt_str, &format!("origin/{branch}")], root,)?;
            } else {
                // Brand new branch: fetch latest default branch from origin so we
                // branch from current remote state. Fall back to local default
                // branch if fetch fails (offline, no remote, etc).
                let fetch_ok = run!(self.runner, "git", &["fetch", "origin", &default_branch], root).is_ok();

                let start_point = if fetch_ok {
                    format!("origin/{default_branch}")
                } else {
                    tracing::warn!(
                        %default_branch, "fetch from origin failed, branching from local"
                    );
                    default_branch.clone()
                };

                run!(self.runner, "git", &["worktree", "add", "-b", branch, wt_str, &start_point], root,)?;
            }
        }
        // A newly created worktree is never the main worktree
        Ok(self.enrich_checkout(&wt_path, branch, false, &default_branch).await)
    }

    async fn remove_checkout(&self, repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        let root = repo_root.as_path();
        info!(%branch, "git: removing worktree");

        let output = run!(self.runner, "git", &["worktree", "list", "--porcelain"], root)?;
        let entries = Self::parse_porcelain(&output);
        let wt_path = entries
            .iter()
            .find(|(_, b)| b == branch)
            .map(|(p, _)| p.clone())
            .ok_or_else(|| format!("no worktree found for branch {branch}"))?;

        let wt_str = wt_path.to_str().ok_or_else(|| format!("worktree path is not valid UTF-8: {}", wt_path.display()))?;

        run!(self.runner, "git", &["worktree", "remove", "--force", wt_str], root)?;

        // Force-delete branch (-D) since feature branches are typically
        // unmerged locally. Skip trunk to prevent catastrophic deletion.
        let default_branch = self.default_branch(repo_root).await;
        if branch != default_branch {
            if let Err(e) = run!(self.runner, "git", &["branch", "-D", branch], root) {
                tracing::warn!(%branch, err = %e, "failed to delete branch");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::providers::{testing::MockRunner, vcs::parse_porcelain_status, CommandRunner};

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
    fn parse_working_tree_mixed() {
        let output = "M  src/main.rs\n?? new_file.rs\nA  added.rs\n M modified.rs\n";
        let wt = parse_porcelain_status(output);
        assert_eq!(wt.staged, 2); // M and A in index column
        assert_eq!(wt.modified, 1); // M in worktree column
        assert_eq!(wt.untracked, 1); // ??
    }

    #[test]
    fn parse_working_tree_empty() {
        let wt = parse_porcelain_status("");
        assert_eq!(wt.staged, 0);
        assert_eq!(wt.modified, 0);
        assert_eq!(wt.untracked, 0);
    }

    #[test]
    fn render_worktree_path_default_template() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![]));
        let checkout_path = crate::config::default_checkout_path();
        let mgr = GitCheckoutManager::new(checkout_path, runner);
        let repo = ExecutionEnvironmentPath::new("/home/user/myrepo");

        let path = mgr.render_worktree_path(&repo, "feature/my-branch").unwrap();
        assert_eq!(path.as_path(), Path::new("/home/user/myrepo.feature-my-branch"));
    }

    #[test]
    fn render_worktree_path_absolute_template() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![]));
        let mgr = GitCheckoutManager::new("/tmp/worktrees/{{ repo }}.{{ branch | sanitize }}".to_string(), runner);
        let repo = ExecutionEnvironmentPath::new("/home/user/myrepo");

        let path = mgr.render_worktree_path(&repo, "fix\\backslash").unwrap();
        assert_eq!(path.as_path(), Path::new("/tmp/worktrees/myrepo.fix-backslash"));
    }

    #[test]
    fn render_worktree_path_relative_template() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockRunner::new(vec![]));
        let mgr = GitCheckoutManager::new("worktrees/{{ branch | sanitize }}".to_string(), runner);
        let repo = ExecutionEnvironmentPath::new("/home/user/myrepo");

        let path = mgr.render_worktree_path(&repo, "dev/thing").unwrap();
        assert_eq!(path.as_path(), Path::new("/home/user/myrepo/worktrees/dev-thing"));
    }

    // ── Record/replay tests ──

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("vcs", name)
    }

    #[tokio::test]
    async fn record_replay_create_checkout_tracks_remote_branch() {
        use crate::providers::{replay, vcs::checkout_test_support};

        let live = replay::is_live();
        let temp = if live { Some(checkout_test_support::setup_remote_only_branch()) } else { None };
        let repo_path = temp.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));
        let repo_path = ExecutionEnvironmentPath::new(repo_path);

        let mut masks = replay::Masks::new();
        masks.add(repo_path.as_path().to_str().expect("repo path is valid UTF-8"), "{repo}");
        let session = replay::test_session(&fixture("git_create_remote_branch.yaml"), masks);
        let runner = replay::test_runner(&session);

        let checkout_path = crate::config::default_checkout_path();
        let mgr = GitCheckoutManager::new(checkout_path, runner.clone());

        checkout_test_support::assert_checkout_tracks_remote_branch(&mgr, &runner, &repo_path).await;

        session.finish();
    }
}
