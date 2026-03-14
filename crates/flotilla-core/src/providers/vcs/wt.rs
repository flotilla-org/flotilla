use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use crate::providers::{run, types::*, CommandRunner};

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
    fn into_checkout(self) -> (PathBuf, Checkout) {
        let path = self.path;
        let host_path = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), path.clone());
        let correlation_keys = vec![CorrelationKey::Branch(self.branch.clone()), CorrelationKey::CheckoutPath(host_path)];
        (path, Checkout {
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
            correlation_keys,
            association_keys: Vec::new(),
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
    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
        let output = run!(self.runner, "wt", &["list", "--format=json"], repo_root)?;
        let json = Self::strip_to_json(&output);
        let worktrees: Vec<WtWorktree> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut checkouts: Vec<(PathBuf, Checkout)> = worktrees.into_iter().map(|wt| wt.into_checkout()).collect();

        // Enrich with issue links from git config
        let futures: Vec<_> =
            checkouts.iter().map(|(_, co)| super::read_branch_issue_links(repo_root, &co.branch, &*self.runner)).collect();
        let all_links = futures::future::join_all(futures).await;
        for ((_, co), links) in checkouts.iter_mut().zip(all_links) {
            co.association_keys = links;
        }

        Ok(checkouts)
    }

    async fn create_checkout(&self, repo_root: &Path, branch: &str, create_branch: bool) -> Result<(PathBuf, Checkout), String> {
        info!(%branch, %create_branch, "wt: creating worktree");

        // Check if a remote-tracking branch exists. If so, use `wt switch`
        // (without --create) so wt tracks the remote branch instead of
        // creating a brand new one from the default branch.
        let remote_exists = if create_branch {
            run!(self.runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}"),], repo_root,).is_ok()
        } else {
            false
        };

        if create_branch && !remote_exists {
            run!(self.runner, "wt", &["switch", "--create", branch, "--no-cd"], repo_root)?;
        } else {
            run!(self.runner, "wt", &["switch", branch, "--no-cd", "--yes"], repo_root)?;
        }

        // Look up the path of the newly created worktree
        let list_output = run!(self.runner, "wt", &["list", "--format=json"], repo_root)?;
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

    async fn remove_checkout(&self, repo_root: &Path, branch: &str) -> Result<(), String> {
        info!(%branch, "wt: removing worktree");
        run!(self.runner, "wt", &["remove", branch], repo_root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        replay,
        vcs::{checkout_test_support::git, CheckoutManager},
    };

    // ── Setup helpers (only called in record mode) ──

    /// Run a wt command in `repo`, panicking on failure.
    fn wt(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("wt").args(args).current_dir(repo).stdin(std::process::Stdio::null()).output().unwrap();
        assert!(out.status.success(), "wt {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    /// Create a temp git repo cloned from a bare remote, with a feature branch
    /// worktree. Returns (tempdir_guard, canonical_repo_path).
    ///
    /// Layout after setup:
    /// - `<tmp>/remote.git` — bare remote
    /// - `<tmp>/repo`       — main worktree (the clone)
    /// - `<tmp>/repo.feature-foo` — feature worktree created by `wt`
    ///
    /// The feature worktree has staged, modified, and untracked changes.
    fn setup_list() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote.git");
        let repo = base.join("repo");

        // Create bare remote and clone it
        git(&base, &["init", "--bare", remote.to_str().unwrap()]);
        git(&base, &["clone", remote.to_str().unwrap(), repo.to_str().unwrap()]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);

        // Initial commit on main
        std::fs::write(repo.join("README.md"), "# Test repo\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "Initial commit"]);
        git(&repo, &["push", "origin", "main"]);

        // Create feature worktree via wt
        wt(&repo, &["switch", "--create", "feature/foo", "--no-cd"]);

        // Make some changes in the feature worktree for working_tree status
        let feature_path = base.join("repo.feature-foo");
        // Staged change
        std::fs::write(feature_path.join("staged.txt"), "staged content").unwrap();
        git(&feature_path, &["add", "staged.txt"]);
        // Modified tracked file
        std::fs::write(feature_path.join("README.md"), "modified\n").unwrap();
        // Untracked file
        std::fs::write(feature_path.join("untracked.txt"), "untracked").unwrap();

        (dir, repo)
    }

    /// Create a temp git repo cloned from a bare remote. No extra worktrees.
    /// Used for create_checkout and remove_checkout tests.
    fn setup_base_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote.git");
        let repo = base.join("repo");

        git(&base, &["init", "--bare", remote.to_str().unwrap()]);
        git(&base, &["clone", remote.to_str().unwrap(), repo.to_str().unwrap()]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "# Test\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "Initial commit"]);
        git(&repo, &["push", "origin", "main"]);

        (dir, repo)
    }

    /// Create a base repo with a feature worktree ready to be removed.
    fn setup_remove() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = setup_base_repo();
        wt(&repo, &["switch", "--create", "feat-remove", "--no-cd"]);
        (dir, repo)
    }

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("vcs", name)
    }

    // ── Record/replay tests ──

    #[tokio::test]
    async fn record_replay_list_checkouts() {
        let recording = replay::is_recording();
        let temp = if recording { Some(setup_list()) } else { None };
        let repo_path = temp.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("wt_list.yaml"), masks);
        let runner = replay::test_runner(&session);

        let mgr = WtCheckoutManager::new(runner);
        let checkouts = mgr.list_checkouts(&repo_path).await.unwrap();

        assert_eq!(checkouts.len(), 2);

        // Find main worktree
        let (path_main, co_main) = checkouts.iter().find(|(_, co)| co.branch == "main").expect("should have main worktree");
        assert_eq!(path_main, &repo_path);
        assert!(co_main.is_main);
        // main has remote tracking since we cloned
        assert!(co_main.remote_ahead_behind.is_some());
        let commit_main = co_main.last_commit.as_ref().expect("main should have commit");
        assert!(!commit_main.short_sha.is_empty());
        assert_eq!(commit_main.message, "Initial commit");
        assert!(co_main.association_keys.is_empty());

        // Find feature worktree
        let (path_feat, co_feat) = checkouts.iter().find(|(_, co)| co.branch == "feature/foo").expect("should have feature/foo worktree");
        assert!(!co_feat.is_main);
        // The worktree path should be a sibling of the repo
        assert!(
            path_feat.to_str().unwrap().contains("feature-foo"),
            "feature worktree path should contain feature-foo: {}",
            path_feat.display()
        );
        // trunk ahead/behind should be present
        let trunk_ab = co_feat.trunk_ahead_behind.as_ref().expect("feature should have trunk ahead/behind");
        assert_eq!(trunk_ab.ahead, 0);
        assert_eq!(trunk_ab.behind, 0);
        // working tree status: staged, modified, untracked
        let wt_status = co_feat.working_tree.as_ref().expect("feature should have working tree status");
        assert_eq!(wt_status.staged, 1);
        assert_eq!(wt_status.modified, 1);
        assert_eq!(wt_status.untracked, 1);
        let commit_feat = co_feat.last_commit.as_ref().expect("feature should have commit");
        assert!(!commit_feat.short_sha.is_empty());
        assert!(co_feat.association_keys.is_empty());

        // Correlation keys
        assert!(co_feat.correlation_keys.contains(&CorrelationKey::Branch("feature/foo".to_string())));
        assert!(co_feat.correlation_keys.contains(&CorrelationKey::CheckoutPath(flotilla_protocol::HostPath::new(
            flotilla_protocol::HostName::local(),
            path_feat.clone()
        ),)));

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_create_checkout() {
        let recording = replay::is_recording();
        let temp = if recording { Some(setup_base_repo()) } else { None };
        let repo_path = temp.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("wt_create.yaml"), masks);
        let runner = replay::test_runner(&session);

        let mgr = WtCheckoutManager::new(runner);
        let (path, checkout) = mgr.create_checkout(&repo_path, "new-feature", true).await.unwrap();

        assert!(path.to_str().unwrap().contains("new-feature"), "created worktree path should contain new-feature: {}", path.display());
        assert_eq!(checkout.branch, "new-feature");
        assert!(!checkout.is_main);
        let trunk_ab = checkout.trunk_ahead_behind.as_ref().expect("new branch should have trunk ahead/behind");
        assert_eq!(trunk_ab.ahead, 0);
        assert_eq!(trunk_ab.behind, 0);
        let wt_status = checkout.working_tree.as_ref().expect("new branch should have working tree status");
        assert_eq!(wt_status.staged, 0);
        assert_eq!(wt_status.modified, 0);
        assert_eq!(wt_status.untracked, 0);
        assert!(checkout.association_keys.is_empty());

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_remove_checkout() {
        let recording = replay::is_recording();
        let temp = if recording { Some(setup_remove()) } else { None };
        let repo_path = temp.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("wt_remove.yaml"), masks);
        let runner = replay::test_runner(&session);

        let mgr = WtCheckoutManager::new(runner);
        mgr.remove_checkout(&repo_path, "feat-remove").await.unwrap();

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_create_checkout_tracks_remote_branch() {
        use crate::providers::vcs::checkout_test_support;

        let recording = replay::is_recording();
        let temp = if recording { Some(checkout_test_support::setup_remote_only_branch()) } else { None };
        let repo_path = temp.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().expect("repo path is valid UTF-8"), "{repo}");
        let session = replay::test_session(&fixture("wt_create_remote_branch.yaml"), masks);
        let runner = replay::test_runner(&session);

        let mgr = WtCheckoutManager::new(runner.clone());

        checkout_test_support::assert_checkout_tracks_remote_branch(&mgr, &runner, &repo_path).await;

        session.finish();
    }
}
