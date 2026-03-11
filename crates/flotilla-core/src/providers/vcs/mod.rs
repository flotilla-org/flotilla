pub mod git;
pub mod git_worktree;
pub mod wt;

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::types::*;
use crate::providers::{run, CommandRunner};

pub const TRUNK_NAMES: &[&str] = &["main", "master", "trunk"];

#[allow(dead_code)]
#[async_trait]
pub trait Vcs: Send + Sync {
    fn display_name(&self) -> &str;
    /// Given any path (possibly inside a worktree/checkout), resolve to the
    /// main repository root. Returns None if the path is not inside a repo.
    fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf>;
    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>, String>;
    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>, String>;
    async fn commit_log(
        &self,
        repo_root: &Path,
        branch: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, String>;
    async fn ahead_behind(
        &self,
        repo_root: &Path,
        branch: &str,
        reference: &str,
    ) -> Result<AheadBehind, String>;
    async fn working_tree_status(
        &self,
        repo_root: &Path,
        checkout_path: &Path,
    ) -> Result<WorkingTreeStatus, String>;
}

#[async_trait]
pub trait CheckoutManager: Send + Sync {
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str {
        "Checkouts"
    }
    fn item_noun(&self) -> &str {
        "checkout"
    }
    fn abbreviation(&self) -> &str {
        "CO"
    }
    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String>;
    async fn create_checkout(
        &self,
        repo_root: &Path,
        branch: &str,
        create_branch: bool,
    ) -> Result<(PathBuf, Checkout), String>;
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
    WorkingTreeStatus {
        staged,
        modified,
        untracked,
    }
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
                keys.push(AssociationKey::IssueRef(
                    provider.to_string(),
                    id.to_string(),
                ));
            }
        }
    }
    keys
}

/// Read issue links from git config for a specific branch.
/// Returns empty vec if no links or on error (non-fatal).
pub async fn read_branch_issue_links(
    repo_root: &Path,
    branch: &str,
    runner: &dyn CommandRunner,
) -> Vec<AssociationKey> {
    let pattern = format!(
        "branch\\.{}\\.flotilla\\.issues\\.",
        regex_escape_branch(branch)
    );
    let result = run!(
        runner,
        "git",
        &["config", "--get-regexp", &pattern],
        repo_root
    );
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
        assert_eq!(
            keys,
            vec![
                AssociationKey::IssueRef("github".into(), "123".into()),
                AssociationKey::IssueRef("github".into(), "456".into()),
            ]
        );
    }

    #[test]
    fn parse_issue_links_multiple_providers() {
        let git_output = "branch.feat-x.flotilla.issues.github 42\nbranch.feat-x.flotilla.issues.linear ABC-123\n";
        let keys = parse_issue_config_output(git_output);
        assert_eq!(
            keys,
            vec![
                AssociationKey::IssueRef("github".into(), "42".into()),
                AssociationKey::IssueRef("linear".into(), "ABC-123".into()),
            ]
        );
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
}
