pub mod clone;
pub mod git;
pub mod git_worktree;
pub mod provisioning;
pub mod wt;

use std::path::Path;

use async_trait::async_trait;
use flotilla_protocol::CheckoutIntent;
pub use provisioning::{CloneInspection, CloneProvisioner, GitCloneProvisioner};
use tracing::warn;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{run, types::*, ChannelLabel, CommandRunner},
};

pub const TRUNK_NAMES: &[&str] = &["main", "master", "trunk"];

#[allow(dead_code)]
#[async_trait]
pub trait Vcs: Send + Sync {
    /// Given any path (possibly inside a worktree/checkout), resolve to the
    /// main repository root. Returns None if the path is not inside a repo.
    async fn resolve_repo_root(&self, path: &ExecutionEnvironmentPath) -> Option<ExecutionEnvironmentPath>;
    async fn list_local_branches(&self, repo_root: &ExecutionEnvironmentPath) -> Result<Vec<BranchInfo>, String>;
    async fn list_remote_branches(&self, repo_root: &ExecutionEnvironmentPath) -> Result<Vec<String>, String>;
    async fn commit_log(&self, repo_root: &ExecutionEnvironmentPath, branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String>;
    async fn ahead_behind(&self, repo_root: &ExecutionEnvironmentPath, branch: &str, reference: &str) -> Result<AheadBehind, String>;
    async fn working_tree_status(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        checkout_path: &ExecutionEnvironmentPath,
    ) -> Result<WorkingTreeStatus, String>;
}

#[async_trait]
pub trait CheckoutManager: Send + Sync {
    /// Validate whether this checkout manager can satisfy the requested branch intent.
    ///
    /// For ambient checkout flows the executor calls this before `create_checkout`.
    /// Managers used in constructed environments may need to call it from
    /// `create_checkout` themselves when bootstrap/discovery bypasses that outer preflight.
    async fn validate_target(&self, repo_root: &ExecutionEnvironmentPath, branch: &str, intent: CheckoutIntent) -> Result<(), String>;
    async fn list_checkouts(&self, repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String>;
    async fn create_checkout(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String>;
    async fn remove_checkout(&self, repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String>;
}

#[allow(dead_code)]
pub struct VcsBundle {
    pub vcs: Box<dyn Vcs>,
    pub checkout_manager: Box<dyn CheckoutManager>,
}

/// Parse `git status --porcelain` output into a `WorkingTreeStatus`.
///
/// Each line has a two-character status prefix: X Y, where X is the index
/// (staging area) status and Y is the working-tree status.  `??` means
/// untracked.  This is the single canonical implementation used by both
/// the `Vcs` and `CheckoutManager` providers.
pub(crate) fn parse_porcelain_status(output: &str) -> WorkingTreeStatus {
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
            if x != b' ' {
                staged += 1;
            }
            if y != b' ' && y != b'?' {
                modified += 1;
            }
        }
    }
    WorkingTreeStatus { staged, modified, untracked }
}

/// Parse the output of `git rev-list --count --left-right` into an `AheadBehind`.
///
/// Output format is `<ahead>\t<behind>\n`.
pub(crate) fn parse_ahead_behind(output: &str) -> Option<AheadBehind> {
    let trimmed = output.trim();
    let mut parts = trimmed.split('\t');
    let ahead: i64 = parts.next()?.parse().ok()?;
    let behind: i64 = parts.next()?.parse().ok()?;
    Some(AheadBehind { ahead, behind })
}

/// Write issue links to git config for a specific branch.
/// Errors are logged and ignored because issue linking is best-effort metadata.
pub(crate) async fn write_branch_issue_links(repo_root: &Path, branch: &str, issue_ids: &[(String, String)], runner: &dyn CommandRunner) {
    use std::collections::HashMap;

    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(err) = run!(runner, "git", &["config", &key, &value], repo_root) {
            warn!(err = %err, "failed to write issue link");
        }
    }
}

async fn validate_checkout_target_with_prefix(
    cwd: &Path,
    prefix: &[&str],
    branch: &str,
    intent: CheckoutIntent,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let local_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/remotes/origin/{branch}");

    let mut local_args = prefix.to_vec();
    local_args.extend(["show-ref", "--verify", "--quiet", local_ref.as_str()]);
    let local_exists = runner.run("git", &local_args, cwd, &ChannelLabel::Default).await.is_ok();

    let mut remote_args = prefix.to_vec();
    remote_args.extend(["show-ref", "--verify", "--quiet", remote_ref.as_str()]);
    let remote_exists = runner.run("git", &remote_args, cwd, &ChannelLabel::Default).await.is_ok();

    match intent {
        CheckoutIntent::ExistingBranch if local_exists || remote_exists => Ok(()),
        CheckoutIntent::ExistingBranch => Err(format!("branch not found: {branch}")),
        CheckoutIntent::FreshBranch if local_exists || remote_exists => Err(format!("branch already exists: {branch}")),
        CheckoutIntent::FreshBranch => Ok(()),
    }
}

pub(crate) async fn validate_checkout_target_in_repo(
    repo_root: &Path,
    branch: &str,
    intent: CheckoutIntent,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    validate_checkout_target_with_prefix(repo_root, &[], branch, intent, runner).await
}

pub(crate) async fn validate_checkout_target_in_git_dir(
    git_dir: &Path,
    cwd: &Path,
    branch: &str,
    intent: CheckoutIntent,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let git_dir = git_dir.to_str().ok_or_else(|| "git dir path is not valid UTF-8".to_string())?;
    validate_checkout_target_with_prefix(cwd, &["--git-dir", git_dir], branch, intent, runner).await
}

/// Shared test utilities for checkout manager implementations.
#[cfg(test)]
pub(crate) mod checkout_test_support {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::{vcs::CheckoutManager, ChannelLabel, CommandRunner},
    };

    /// Run a git command, panicking on failure.
    pub fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("failed to spawn git");
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    /// Create a repo where `feature/remote-only` exists on the remote but not locally.
    /// The remote branch has a commit "remote-only work" ahead of main.
    pub fn setup_remote_only_branch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path().canonicalize().expect("failed to canonicalize tempdir");
        let remote = base.join("remote.git");
        let repo = base.join("repo");

        git(&base, &["init", "--bare", remote.to_str().expect("non-UTF-8 path")]);
        git(&base, &["clone", remote.to_str().expect("non-UTF-8 path"), repo.to_str().expect("non-UTF-8 path")]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);

        // Initial commit on main
        std::fs::write(repo.join("README.md"), "# Test\n").expect("failed to write README");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "Initial commit"]);
        git(&repo, &["push", "origin", "main"]);

        // Create feature branch, commit, push, then delete local
        git(&repo, &["checkout", "-b", "feature/remote-only"]);
        std::fs::write(repo.join("remote-work.txt"), "work\n").expect("failed to write test file");
        git(&repo, &["add", "remote-work.txt"]);
        git(&repo, &["commit", "-m", "remote-only work"]);
        git(&repo, &["push", "origin", "feature/remote-only"]);

        // Back to main, delete local branch
        git(&repo, &["checkout", "main"]);
        git(&repo, &["branch", "-D", "feature/remote-only"]);

        (dir, repo)
    }

    /// Assert that create_checkout correctly tracks a remote-only branch.
    ///
    /// The worktree should end up on the remote branch's commit ("remote-only work"),
    /// not on main's HEAD ("Initial commit").
    pub async fn assert_checkout_tracks_remote_branch(
        mgr: &dyn CheckoutManager,
        runner: &Arc<dyn CommandRunner>,
        repo_path: &ExecutionEnvironmentPath,
    ) {
        let (wt_path, checkout) =
            mgr.create_checkout(repo_path, "feature/remote-only", true).await.expect("create_checkout should succeed");

        assert_eq!(checkout.branch, "feature/remote-only");
        assert!(!checkout.is_main);

        let commit = checkout.last_commit.as_ref().expect("should have commit info");
        assert_eq!(commit.message, "remote-only work", "checkout should be on the remote branch's commit, not main");

        // Verify via direct git command through the runner
        let label = ChannelLabel::Command("verify-commit".into());
        let log_output = runner.run("git", &["log", "-1", "--format=%s"], wt_path.as_path(), &label).await.expect("git log should succeed");
        assert_eq!(log_output.trim(), "remote-only work", "worktree HEAD should be the remote branch's tip");
    }
}
