//! Git-related detectors: binary availability and repo structure.

use std::path::Path;

use async_trait::async_trait;

use crate::providers::{
    discovery::{
        detectors::generic::{parse_first_dotted_version, CommandDetector},
        EnvVars, EnvironmentAssertion, RepoDetector, VcsKind,
    },
    run, CommandRunner,
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;

pub fn git_binary_detector() -> CommandDetector {
    CommandDetector::new("git", &["--version"], parse_first_dotted_version)
}

/// Bootstrap probes used before checkout-scoped provider construction.
pub async fn reference_clone_available(runner: &dyn CommandRunner) -> bool {
    run!(runner, "git", &["--git-dir", "/ref/repo", "rev-parse", "--git-dir"], Path::new("/")).is_ok()
}

pub async fn origin_url(runner: &dyn CommandRunner, checkout: &Path) -> Result<String, String> {
    run!(runner, "git", &["remote", "get-url", "origin"], checkout)
}

// ---------------------------------------------------------------------------
// VcsRepoDetector (RepoDetector)
// ---------------------------------------------------------------------------

/// Detects whether the repo root contains a `.git` directory or file.
pub struct VcsRepoDetector;

#[async_trait]
impl RepoDetector for VcsRepoDetector {
    async fn detect(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        runner: &dyn CommandRunner,
        _env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        let inside = match run!(runner, "git", &["rev-parse", "--is-inside-work-tree"], repo_root.as_path()) {
            Ok(output) if output.trim() == "true" => output,
            _ => return vec![],
        };
        let _ = inside;
        let git_dir = run!(runner, "git", &["rev-parse", "--path-format=absolute", "--git-dir"], repo_root.as_path()).ok();
        let git_common_dir = run!(runner, "git", &["rev-parse", "--path-format=absolute", "--git-common-dir"], repo_root.as_path()).ok();
        let is_main_checkout =
            matches!((git_dir, git_common_dir), (Some(git_dir), Some(common_dir)) if git_dir.trim() == common_dir.trim());
        vec![EnvironmentAssertion::vcs_checkout(repo_root.as_path(), VcsKind::Git, is_main_checkout)]
    }
}

/// Extract the remote host, including SSH aliases.
fn detect_host_from_url(url: &str) -> Option<String> {
    let canonical = flotilla_resources::canonicalize_repo_url(url).ok()?;
    let (_, rest) = canonical.split_once("://")?;
    Some(rest.split_once('/')?.0.to_ascii_lowercase())
}

/// Extract "owner" and "repo" from a git remote URL.
///
/// Handles SSH (`git@github.com:owner/repo.git`) and
/// HTTPS (`https://github.com/owner/repo.git`).
fn extract_owner_repo(url: &str) -> Option<(String, String)> {
    let canonical = flotilla_resources::canonicalize_repo_url(url).ok()?;
    let (_, rest) = canonical.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    let (owner, repo) = path.rsplit_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

pub(crate) fn remote_assertion(url: &str, remote_name: &str) -> Option<EnvironmentAssertion> {
    let host = detect_host_from_url(url)?;
    let (owner, repo) = extract_owner_repo(url)?;
    Some(EnvironmentAssertion::remote_host(host, owner, repo, remote_name))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use crate::testkits::discovery::TestEnvVars;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    // -- VcsRepoDetector --

    #[tokio::test]
    async fn vcs_repo_detector_git_dir() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--is-inside-work-tree"], Ok("true\n".into()))
            .on_run("git", &["rev-parse", "--path-format=absolute", "--git-dir"], Ok("/tmp/repo/.git\n".into()))
            .on_run("git", &["rev-parse", "--path-format=absolute", "--git-common-dir"], Ok("/tmp/repo/.git\n".into()))
            .build();
        let assertions = VcsRepoDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::VcsCheckoutDetected { root, kind, is_main_checkout } => {
                assert_eq!(root.as_path(), repo_root.as_path());
                assert_eq!(*kind, VcsKind::Git);
                assert!(*is_main_checkout);
            }
            other => panic!("expected VcsCheckoutDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vcs_repo_detector_git_file_is_worktree() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--is-inside-work-tree"], Ok("true\n".into()))
            .on_run("git", &["rev-parse", "--path-format=absolute", "--git-dir"], Ok("/tmp/repo/.git/worktrees/feature\n".into()))
            .on_run("git", &["rev-parse", "--path-format=absolute", "--git-common-dir"], Ok("/tmp/repo/.git\n".into()))
            .build();
        let assertions = VcsRepoDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::VcsCheckoutDetected { root, kind, is_main_checkout } => {
                assert_eq!(root.as_path(), repo_root.as_path());
                assert_eq!(*kind, VcsKind::Git);
                assert!(!*is_main_checkout);
            }
            other => panic!("expected VcsCheckoutDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vcs_repo_detector_no_git() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--is-inside-work-tree"], Err("fatal: not a git repository".into()))
            .build();
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let assertions = VcsRepoDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert!(assertions.is_empty());
    }

    // -- URL parsing unit tests --

    #[test]
    fn detect_host_from_url_github() {
        assert_eq!(detect_host_from_url("git@github.com:owner/repo.git"), Some("github.com".into()));
        assert_eq!(detect_host_from_url("https://GitHub.com/owner/repo"), Some("github.com".into()));
    }

    #[test]
    fn detect_host_from_url_gitlab() {
        assert_eq!(detect_host_from_url("https://gitlab.mycompany.com/org/project"), Some("gitlab.mycompany.com".into()));
    }

    #[test]
    fn detect_host_from_url_unknown() {
        assert_eq!(detect_host_from_url("https://bitbucket.org/owner/repo"), Some("bitbucket.org".into()));
        assert_eq!(detect_host_from_url(""), None);
    }

    #[test]
    fn extract_owner_repo_ssh() {
        assert_eq!(extract_owner_repo("git@github.com:owner/repo.git"), Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn extract_owner_repo_https() {
        assert_eq!(extract_owner_repo("https://github.com/owner/repo.git"), Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn extract_owner_repo_no_git_suffix() {
        assert_eq!(extract_owner_repo("git@github.com:owner/repo"), Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn extract_owner_repo_trailing_slash() {
        assert_eq!(extract_owner_repo("https://github.com/owner/repo/"), Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn extract_owner_repo_no_repo() {
        assert_eq!(extract_owner_repo("git@github.com:repo"), None);
        assert_eq!(extract_owner_repo("https://github.com/owner"), None);
    }

    #[test]
    fn extract_owner_repo_deep_path() {
        // Keep nested owner paths intact.
        assert_eq!(extract_owner_repo("https://github.com/org/sub/repo.git"), Some(("org/sub".into(), "repo".into())));
    }
}
