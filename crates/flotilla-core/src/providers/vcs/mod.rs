pub mod git;
pub mod git_worktree;
pub mod wt;

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::{run, types::*, CommandRunner};

pub const TRUNK_NAMES: &[&str] = &["main", "master", "trunk"];

#[allow(dead_code)]
#[async_trait]
pub trait Vcs: Send + Sync {
    /// Given any path (possibly inside a worktree/checkout), resolve to the
    /// main repository root. Returns None if the path is not inside a repo.
    fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf>;
    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>, String>;
    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>, String>;
    async fn commit_log(&self, repo_root: &Path, branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String>;
    async fn ahead_behind(&self, repo_root: &Path, branch: &str, reference: &str) -> Result<AheadBehind, String>;
    async fn working_tree_status(&self, repo_root: &Path, checkout_path: &Path) -> Result<WorkingTreeStatus, String>;
}

#[async_trait]
pub trait CheckoutManager: Send + Sync {
    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String>;
    async fn create_checkout(&self, repo_root: &Path, branch: &str, create_branch: bool) -> Result<(PathBuf, Checkout), String>;
    async fn remove_checkout(&self, repo_root: &Path, branch: &str) -> Result<(), String>;
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

/// Parse the output of `git config --get-regexp 'branch\.<branch>\.flotilla\.issues\.'`
/// into association keys. Each line has the format:
/// `branch.<name>.flotilla.issues.<provider> id1,id2,...`
pub fn parse_issue_config_output(output: &str) -> Vec<AssociationKey> {
    let mut keys = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((config_key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(provider) = config_key.rsplit_once(".issues.").map(|(_, p)| p) else {
            continue;
        };
        for id in value.split(',') {
            let id = id.trim();
            if !id.is_empty() {
                keys.push(AssociationKey::IssueRef(provider.to_string(), id.to_string()));
            }
        }
    }
    keys
}

/// Read issue links from git config for a specific branch.
/// Returns empty vec if no links or on error (non-fatal).
pub async fn read_branch_issue_links(repo_root: &Path, branch: &str, runner: &dyn CommandRunner) -> Vec<AssociationKey> {
    let pattern = format!("branch\\.{}\\.flotilla\\.issues\\.", regex_escape_branch(branch));
    let result = run!(runner, "git", &["config", "--get-regexp", &pattern], repo_root);
    match result {
        Ok(output) => parse_issue_config_output(&output),
        Err(_) => Vec::new(),
    }
}

/// Escape special regex characters in branch names for git config --get-regexp.
fn regex_escape_branch(branch: &str) -> String {
    let mut escaped = String::with_capacity(branch.len());
    for c in branch.chars() {
        match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_issue_links_single_provider() {
        let git_output = "branch.feat-x.flotilla.issues.github 123,456\n";
        let keys = parse_issue_config_output(git_output);
        assert_eq!(keys, vec![
            AssociationKey::IssueRef("github".into(), "123".into()),
            AssociationKey::IssueRef("github".into(), "456".into()),
        ]);
    }

    #[test]
    fn parse_issue_links_multiple_providers() {
        let git_output = "branch.feat-x.flotilla.issues.github 42\nbranch.feat-x.flotilla.issues.linear ABC-123\n";
        let keys = parse_issue_config_output(git_output);
        assert_eq!(keys, vec![
            AssociationKey::IssueRef("github".into(), "42".into()),
            AssociationKey::IssueRef("linear".into(), "ABC-123".into()),
        ]);
    }

    #[test]
    fn parse_issue_links_empty() {
        let keys = parse_issue_config_output("");
        assert!(keys.is_empty());
    }

    #[test]
    fn regex_escape_branch_with_dots() {
        assert_eq!(regex_escape_branch("feat.x"), "feat\\.x");
        assert_eq!(regex_escape_branch("simple"), "simple");
    }

    #[test]
    fn parse_ahead_behind_normal() {
        let ab = parse_ahead_behind("3\t5\n").expect("should parse");
        assert_eq!(ab.ahead, 3);
        assert_eq!(ab.behind, 5);
    }

    #[test]
    fn parse_ahead_behind_zeros() {
        let ab = parse_ahead_behind("0\t0\n").expect("should parse");
        assert_eq!(ab.ahead, 0);
        assert_eq!(ab.behind, 0);
    }

    #[test]
    fn parse_ahead_behind_empty() {
        assert!(parse_ahead_behind("").is_none());
    }

    #[test]
    fn parse_ahead_behind_malformed() {
        assert!(parse_ahead_behind("notanumber\t5").is_none());
    }
}

/// Shared test utilities for checkout manager implementations.
#[cfg(test)]
pub(crate) mod checkout_test_support {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::providers::{vcs::CheckoutManager, ChannelLabel, CommandRunner};

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
    pub async fn assert_checkout_tracks_remote_branch(mgr: &dyn CheckoutManager, runner: &Arc<dyn CommandRunner>, repo_path: &Path) {
        let (wt_path, checkout) =
            mgr.create_checkout(repo_path, "feature/remote-only", true).await.expect("create_checkout should succeed");

        assert_eq!(checkout.branch, "feature/remote-only");
        assert!(!checkout.is_main);

        let commit = checkout.last_commit.as_ref().expect("should have commit info");
        assert_eq!(commit.message, "remote-only work", "checkout should be on the remote branch's commit, not main");

        // Verify via direct git command through the runner
        let label = ChannelLabel::Command("verify-commit".into());
        let log_output = runner.run("git", &["log", "-1", "--format=%s"], &wt_path, &label).await.expect("git log should succeed");
        assert_eq!(log_output.trim(), "remote-only work", "worktree HEAD should be the remote branch's tip");
    }
}
