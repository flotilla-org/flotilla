//! Git-related detectors: binary availability and repo structure.

use std::path::Path;

use async_trait::async_trait;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvVars, EnvironmentAssertion, RepoDetector, VcsKind},
        run, CommandRunner,
    },
};

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

// ---------------------------------------------------------------------------
// RemoteHostDetector (RepoDetector)
// ---------------------------------------------------------------------------

/// Detects the origin remote without assuming a hosting platform.
///
/// Preference order for selecting the remote:
/// 1. `origin` if it exists
/// 2. The remote tracked by the current branch
/// 3. First remote with a valid URL
pub struct RemoteHostDetector;

/// Get the URL of the remote for the current tracking branch.
async fn tracking_remote_url(repo_root: &Path, runner: &dyn CommandRunner) -> Option<(String, String)> {
    let upstream = run!(runner, "git", &["rev-parse", "--abbrev-ref", "@{upstream}"], repo_root,).ok()?;
    let upstream = upstream.trim();
    let remotes_output = run!(runner, "git", &["remote"], repo_root).ok()?;
    let remotes: Vec<&str> = remotes_output.lines().map(|line| line.trim()).filter(|line| !line.is_empty()).collect();
    // upstream looks like "origin/main" or "team/origin/main". Match it against
    // configured remotes and prefer the longest prefix to handle remotes containing '/'.
    let remote_name = remotes
        .into_iter()
        .filter(|remote| upstream == *remote || upstream.strip_prefix(remote).is_some_and(|suffix| suffix.starts_with('/')))
        .max_by_key(|remote| remote.len())
        .or_else(|| upstream.split('/').next())?;
    if remote_name.is_empty() {
        return None;
    }
    let url = run!(runner, "git", &["remote", "get-url", remote_name], repo_root).ok()?;
    let url = url.trim().to_string();
    if url.is_empty() {
        return None;
    }
    Some((remote_name.to_string(), url))
}

/// Find the preferred remote URL and its name.
async fn preferred_remote(repo_root: &Path, runner: &dyn CommandRunner) -> Option<(String, String)> {
    // Origin is the remote used for issues, change requests, and pushes.
    let remotes_output = run!(runner, "git", &["remote"], repo_root).ok()?;
    let remotes: Vec<&str> = remotes_output.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();

    // 1. Prefer "origin" if it exists
    if remotes.contains(&"origin") {
        if let Ok(url) = run!(runner, "git", &["remote", "get-url", "origin"], repo_root) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                return Some(("origin".to_string(), url));
            }
        }
    }

    // 2. Try the tracking remote if origin is unavailable.
    if let Some(result) = tracking_remote_url(repo_root, runner).await {
        return Some(result);
    }

    // 3. Fall back to first remote with a valid URL.
    for remote in &remotes {
        if let Ok(url) = run!(runner, "git", &["remote", "get-url", remote], repo_root) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                return Some((remote.to_string(), url));
            }
        }
    }
    None
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

#[async_trait]
impl RepoDetector for RemoteHostDetector {
    async fn detect(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        runner: &dyn CommandRunner,
        _env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        let (remote_name, url) = match preferred_remote(repo_root.as_path(), runner).await {
            Some(r) => r,
            None => return vec![],
        };
        remote_assertion(&url, &remote_name).into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars},
    };

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

    // -- RemoteHostDetector --

    #[tokio::test]
    async fn remote_host_detector_github_ssh() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Err("fatal: no upstream".into()))
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("git@github.com:owner/repo.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_prefers_tracking_remote() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Ok("upstream/main\n".into()))
            .on_run("git", &["remote"], Ok("origin\nupstream\n".into()))
            .on_run("git", &["remote", "get-url", "upstream"], Ok("https://github.com/upstream-owner/repo.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "upstream-owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "upstream");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_uses_origin_with_cross_forge_upstream() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["remote"], Ok("origin\nupstream\n".into()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("git@forgejo.lab.flotilla.work:lab/flotilla.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name }
                if host == "forgejo.lab.flotilla.work" && owner == "lab" && repo == "flotilla" && remote_name == "origin"
        ));
    }

    #[tokio::test]
    async fn remote_host_detector_https_url() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Err("fatal: no upstream".into()))
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://github.com/owner/repo.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_no_remotes() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Err("fatal: no upstream".into()))
            .on_run("git", &["remote"], Ok(String::new()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert!(assertions.is_empty());
    }

    #[tokio::test]
    async fn remote_host_detector_gitlab() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Err("fatal: no upstream".into()))
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://gitlab.example.com/org/project.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name } => {
                assert_eq!(host, "gitlab.example.com");
                assert_eq!(owner, "org");
                assert_eq!(repo, "project");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_accepts_any_host() {
        let repo_root = ExecutionEnvironmentPath::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--abbrev-ref", "@{upstream}"], Err("fatal: no upstream".into()))
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://bitbucket.org/owner/repo.git\n".into()))
            .build();
        let assertions = RemoteHostDetector.detect(&repo_root, &runner, &TestEnvVars::default()).await;
        assert!(matches!(&assertions[0], EnvironmentAssertion::RemoteHost { host, .. } if host == "bitbucket.org"));
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
