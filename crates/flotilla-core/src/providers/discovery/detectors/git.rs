//! Git-related detectors: binary availability and repo structure.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{
    EnvironmentAssertion, HostDetector, HostPlatform, RepoDetector, VcsKind,
};
use crate::providers::{run, CommandRunner};

// ---------------------------------------------------------------------------
// GitBinaryDetector (HostDetector)
// ---------------------------------------------------------------------------

/// Detects whether `git` is available on the host.
pub struct GitBinaryDetector;

#[async_trait]
impl HostDetector for GitBinaryDetector {
    fn name(&self) -> &str {
        "git-binary"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        // Single call: proves binary exists and captures version
        let Ok(output) = run!(runner, "git", &["--version"], Path::new(".")) else {
            return vec![];
        };
        let version = output
            .trim()
            .strip_prefix("git version ")
            .map(|v| v.to_string());
        vec![EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: PathBuf::from("git"),
            version,
        }]
    }
}

// ---------------------------------------------------------------------------
// VcsRepoDetector (RepoDetector)
// ---------------------------------------------------------------------------

/// Detects whether the repo root contains a `.git` directory or file.
pub struct VcsRepoDetector;

#[async_trait]
impl RepoDetector for VcsRepoDetector {
    fn name(&self) -> &str {
        "vcs-repo"
    }

    async fn detect(
        &self,
        repo_root: &Path,
        _runner: &dyn CommandRunner,
    ) -> Vec<EnvironmentAssertion> {
        let git_path = repo_root.join(".git");
        if git_path.is_dir() {
            vec![EnvironmentAssertion::VcsCheckoutDetected {
                root: repo_root.to_path_buf(),
                kind: VcsKind::Git,
                is_main_checkout: true,
            }]
        } else if git_path.is_file() {
            // .git file indicates a worktree
            vec![EnvironmentAssertion::VcsCheckoutDetected {
                root: repo_root.to_path_buf(),
                kind: VcsKind::Git,
                is_main_checkout: false,
            }]
        } else {
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// RemoteHostDetector (RepoDetector)
// ---------------------------------------------------------------------------

/// Detects the remote host platform by parsing git remote URLs.
///
/// Preference order for selecting the remote:
/// 1. The remote tracked by the current branch
/// 2. `origin` if it exists
/// 3. First remote with a valid URL
pub struct RemoteHostDetector;

/// Get the URL of the remote for the current tracking branch.
async fn tracking_remote_url(
    repo_root: &Path,
    runner: &dyn CommandRunner,
) -> Option<(String, String)> {
    let upstream = run!(
        runner,
        "git",
        &["rev-parse", "--abbrev-ref", "@{upstream}"],
        repo_root,
    )
    .ok()?;
    let upstream = upstream.trim();
    let remotes_output = run!(runner, "git", &["remote"], repo_root).ok()?;
    let remotes: Vec<&str> = remotes_output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    // upstream looks like "origin/main" or "team/origin/main". Match it against
    // configured remotes and prefer the longest prefix to handle remotes containing '/'.
    let remote_name = remotes
        .into_iter()
        .filter(|remote| {
            upstream == *remote
                || upstream
                    .strip_prefix(remote)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|remote| remote.len())
        .or_else(|| upstream.split('/').next())?;
    if remote_name.is_empty() {
        return None;
    }
    let url = run!(
        runner,
        "git",
        &["remote", "get-url", remote_name],
        repo_root
    )
    .ok()?;
    let url = url.trim().to_string();
    if url.is_empty() {
        return None;
    }
    Some((remote_name.to_string(), url))
}

/// Find the preferred remote URL and its name.
async fn preferred_remote(
    repo_root: &Path,
    runner: &dyn CommandRunner,
) -> Option<(String, String)> {
    // 1. Try the tracking remote for the current branch
    if let Some(result) = tracking_remote_url(repo_root, runner).await {
        return Some(result);
    }

    // Get the list of remotes for steps 2 and 3
    let remotes_output = run!(runner, "git", &["remote"], repo_root).ok()?;
    let remotes: Vec<&str> = remotes_output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    // 2. Prefer "origin" if it exists
    if remotes.contains(&"origin") {
        if let Ok(url) = run!(runner, "git", &["remote", "get-url", "origin"], repo_root) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                return Some(("origin".to_string(), url));
            }
        }
    }

    // 3. Fall back to first remote with a valid URL
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

/// Detect the host platform from a remote URL.
fn detect_host_from_url(url: &str) -> Option<HostPlatform> {
    let url_lower = url.to_lowercase();
    if url_lower.contains("github.com") {
        Some(HostPlatform::GitHub)
    } else if url_lower.contains("gitlab") {
        Some(HostPlatform::GitLab)
    } else {
        None
    }
}

/// Extract "owner" and "repo" from a git remote URL.
///
/// Handles SSH (`git@github.com:owner/repo.git`) and
/// HTTPS (`https://github.com/owner/repo.git`).
fn extract_owner_repo(url: &str) -> Option<(String, String)> {
    let path = if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:owner/repo.git
        rest.split_once(':').map(|(_, p)| p)
    } else {
        // https://github.com/owner/repo.git or similar
        url.strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .and_then(|u| u.split_once('/').map(|(_, p)| p))
    }?;
    let slug = path.trim_end_matches(".git").trim_matches('/');
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // Take only the first segment after owner as repo (ignore deeper paths)
    let repo = repo.split('/').next().unwrap_or(repo);
    Some((owner.to_string(), repo.to_string()))
}

#[async_trait]
impl RepoDetector for RemoteHostDetector {
    fn name(&self) -> &str {
        "remote-host"
    }

    async fn detect(
        &self,
        repo_root: &Path,
        runner: &dyn CommandRunner,
    ) -> Vec<EnvironmentAssertion> {
        let (remote_name, url) = match preferred_remote(repo_root, runner).await {
            Some(r) => r,
            None => return vec![],
        };
        let platform = match detect_host_from_url(&url) {
            Some(p) => p,
            None => return vec![],
        };
        let (owner, repo) = match extract_owner_repo(&url) {
            Some(r) => r,
            None => return vec![],
        };
        vec![EnvironmentAssertion::RemoteHost {
            platform,
            owner,
            repo,
            remote_name,
        }]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    // -- GitBinaryDetector --

    #[tokio::test]
    async fn git_binary_detector_found() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["--version"], Ok("git version 2.43.0\n".into()))
            .build();
        let assertions = GitBinaryDetector.detect(&runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::BinaryAvailable {
                name,
                path,
                version,
            } => {
                assert_eq!(name, "git");
                assert_eq!(path, &PathBuf::from("git"));
                assert_eq!(version.as_deref(), Some("2.43.0"));
            }
            other => panic!("expected BinaryAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_binary_detector_not_found() {
        // No on_run configured → run! returns Err → empty assertions
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = GitBinaryDetector.detect(&runner).await;
        assert!(assertions.is_empty());
    }

    // -- VcsRepoDetector --

    #[tokio::test]
    async fn vcs_repo_detector_git_dir() {
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("create .git dir");
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = VcsRepoDetector.detect(dir.path(), &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::VcsCheckoutDetected {
                root,
                kind,
                is_main_checkout,
            } => {
                assert_eq!(root, dir.path());
                assert_eq!(*kind, VcsKind::Git);
                assert!(*is_main_checkout);
            }
            other => panic!("expected VcsCheckoutDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vcs_repo_detector_git_file_is_worktree() {
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(dir.path().join(".git"), "gitdir: /some/path\n").expect("write .git file");
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = VcsRepoDetector.detect(dir.path(), &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::VcsCheckoutDetected {
                root,
                kind,
                is_main_checkout,
            } => {
                assert_eq!(root, dir.path());
                assert_eq!(*kind, VcsKind::Git);
                assert!(!*is_main_checkout);
            }
            other => panic!("expected VcsCheckoutDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vcs_repo_detector_no_git() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = VcsRepoDetector.detect(dir.path(), &runner).await;
        assert!(assertions.is_empty());
    }

    // -- RemoteHostDetector --

    #[tokio::test]
    async fn remote_host_detector_github_ssh() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".into()),
            )
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("git@github.com:owner/repo.git\n".into()),
            )
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost {
                platform,
                owner,
                repo,
                remote_name,
            } => {
                assert_eq!(*platform, HostPlatform::GitHub);
                assert_eq!(owner, "owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_prefers_tracking_remote() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Ok("upstream/main\n".into()),
            )
            .on_run("git", &["remote"], Ok("origin\nupstream\n".into()))
            .on_run(
                "git",
                &["remote", "get-url", "upstream"],
                Ok("https://github.com/upstream-owner/repo.git\n".into()),
            )
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost {
                platform,
                owner,
                repo,
                remote_name,
            } => {
                assert_eq!(*platform, HostPlatform::GitHub);
                assert_eq!(owner, "upstream-owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "upstream");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_https_url() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".into()),
            )
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("https://github.com/owner/repo.git\n".into()),
            )
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost {
                platform,
                owner,
                repo,
                remote_name,
            } => {
                assert_eq!(*platform, HostPlatform::GitHub);
                assert_eq!(owner, "owner");
                assert_eq!(repo, "repo");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_no_remotes() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".into()),
            )
            .on_run("git", &["remote"], Ok(String::new()))
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert!(assertions.is_empty());
    }

    #[tokio::test]
    async fn remote_host_detector_gitlab() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".into()),
            )
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("https://gitlab.example.com/org/project.git\n".into()),
            )
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::RemoteHost {
                platform,
                owner,
                repo,
                remote_name,
            } => {
                assert_eq!(*platform, HostPlatform::GitLab);
                assert_eq!(owner, "org");
                assert_eq!(repo, "project");
                assert_eq!(remote_name, "origin");
            }
            other => panic!("expected RemoteHost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remote_host_detector_unknown_host_returns_empty() {
        let repo_root = Path::new("/tmp/repo");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".into()),
            )
            .on_run("git", &["remote"], Ok("origin\n".into()))
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("https://bitbucket.org/owner/repo.git\n".into()),
            )
            .build();
        let assertions = RemoteHostDetector.detect(repo_root, &runner).await;
        assert!(assertions.is_empty());
    }

    // -- URL parsing unit tests --

    #[test]
    fn detect_host_from_url_github() {
        assert_eq!(
            detect_host_from_url("git@github.com:owner/repo.git"),
            Some(HostPlatform::GitHub)
        );
        assert_eq!(
            detect_host_from_url("https://GitHub.com/owner/repo"),
            Some(HostPlatform::GitHub)
        );
    }

    #[test]
    fn detect_host_from_url_gitlab() {
        assert_eq!(
            detect_host_from_url("https://gitlab.mycompany.com/org/project"),
            Some(HostPlatform::GitLab)
        );
    }

    #[test]
    fn detect_host_from_url_unknown() {
        assert_eq!(
            detect_host_from_url("https://bitbucket.org/owner/repo"),
            None
        );
        assert_eq!(detect_host_from_url(""), None);
    }

    #[test]
    fn extract_owner_repo_ssh() {
        assert_eq!(
            extract_owner_repo("git@github.com:owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn extract_owner_repo_https() {
        assert_eq!(
            extract_owner_repo("https://github.com/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn extract_owner_repo_no_git_suffix() {
        assert_eq!(
            extract_owner_repo("git@github.com:owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn extract_owner_repo_trailing_slash() {
        assert_eq!(
            extract_owner_repo("https://github.com/owner/repo/"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn extract_owner_repo_no_repo() {
        assert_eq!(extract_owner_repo("git@github.com:repo"), None);
        assert_eq!(extract_owner_repo("https://github.com/owner"), None);
    }

    #[test]
    fn extract_owner_repo_deep_path() {
        // For deep paths like org/sub/repo, we take owner=org, repo=sub
        assert_eq!(
            extract_owner_repo("https://github.com/org/sub/repo.git"),
            Some(("org".into(), "sub".into()))
        );
    }
}
