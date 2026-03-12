use std::path::Path;
use std::sync::Arc;

use super::{resolve_claude_path, run, CommandRunner};
use crate::config::ConfigStore;
use crate::providers::ai_utility::claude::ClaudeAiUtility;
use crate::providers::code_review::github::GitHubCodeReview;
use crate::providers::coding_agent::claude::ClaudeCodingAgent;
use crate::providers::coding_agent::codex::CodexCodingAgent;
use crate::providers::coding_agent::cursor::CursorCodingAgent;
use crate::providers::github_api::GhApiClient;
use crate::providers::issue_tracker::github::GitHubIssueTracker;
use crate::providers::registry::ProviderRegistry;
use crate::providers::vcs::git::GitVcs;
use crate::providers::vcs::git_worktree::GitCheckoutManager;
use crate::providers::vcs::wt::WtCheckoutManager;
use crate::providers::workspace::cmux::CmuxWorkspaceManager;
use crate::providers::workspace::tmux::TmuxWorkspaceManager;
use crate::providers::workspace::zellij::ZellijWorkspaceManager;
use tracing::{debug, info, warn};

/// Get the URL of the remote for the current tracking branch.
///
/// Runs `git rev-parse --abbrev-ref @{upstream}` to find the tracking ref
/// (e.g. `origin/main`), then splits on `/` to extract the remote name.
async fn tracking_remote_url(repo_root: &Path, runner: &dyn CommandRunner) -> Option<String> {
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
    debug!(%remote_name, %url, "using tracking remote");
    Some(url)
}

/// Extract the preferred git remote URL for this repo.
///
/// Preference order:
/// 1. The remote tracked by the current branch
/// 2. `origin` if it exists
/// 3. First remote with a valid URL as fallback
pub async fn first_remote_url(repo_root: &Path, runner: &dyn CommandRunner) -> Option<String> {
    // 1. Try the tracking remote for the current branch
    if let Some(url) = tracking_remote_url(repo_root, runner).await {
        return Some(url);
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
                debug!(%url, "using origin remote");
                return Some(url);
            }
        }
    }

    // 3. Fall back to first remote with a valid URL
    for remote in &remotes {
        if let Ok(url) = run!(runner, "git", &["remote", "get-url", remote], repo_root) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                debug!(%remote, "using first available remote as fallback");
                return Some(url);
            }
        }
    }
    None
}

/// Check a remote URL for known hosts.
/// Returns "github" or "gitlab" if matched, None otherwise.
fn detect_host_from_url(url: &str) -> Option<String> {
    let url_lower = url.to_lowercase();
    if url_lower.contains("github.com") {
        Some("github".to_string())
    } else if url_lower.contains("gitlab") {
        Some("gitlab".to_string())
    } else {
        None
    }
}

/// Extract a RepoIdentity from a git remote URL.
/// Delegates to `RepoIdentity::from_remote_url`.
pub fn extract_repo_identity(url: &str) -> Option<flotilla_protocol::RepoIdentity> {
    flotilla_protocol::RepoIdentity::from_remote_url(url)
}

/// Extract "owner/repo" from a git remote URL.
/// Handles SSH (git@github.com:owner/repo.git) and HTTPS (https://github.com/owner/repo.git).
pub fn extract_repo_slug(url: &str) -> Option<String> {
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
    if slug.contains('/') {
        Some(slug.to_string())
    } else {
        None
    }
}

/// Detect available providers for a given repository.
///
/// Detection pipeline:
/// 1. VCS: check for .git (directory or file — worktrees use a file)
/// 2. Checkout manager: check for `wt` CLI
/// 3. Remote host: parse git remote URL -> GitHub/GitLab, check for `gh` CLI
/// 4. Cloud agents: check for Cursor `agent` and `claude` CLI
/// 5. AI utility: check for `claude` CLI
/// 6. Workspace manager: check for cmux binary
///
/// When `follower` is true, only local providers (VCS, checkout manager,
/// workspace manager, terminal pool) are registered. External providers
/// (code review, issue tracker, cloud agents, AI utilities) are skipped
/// because the follower receives that data from the leader via PeerData.
pub async fn detect_providers(
    repo_root: &Path,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
) -> (ProviderRegistry, Option<String>) {
    detect_providers_inner(repo_root, config, runner, false).await
}

/// Like [`detect_providers`] but accepts a `follower` flag.
///
/// When `follower` is true, external providers are stripped after discovery
/// so the daemon only reports local state.
pub async fn detect_providers_with_mode(
    repo_root: &Path,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
    follower: bool,
) -> (ProviderRegistry, Option<String>) {
    detect_providers_inner(repo_root, config, runner, follower).await
}

async fn detect_providers_inner(
    repo_root: &Path,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
    follower: bool,
) -> (ProviderRegistry, Option<String>) {
    let mut registry = ProviderRegistry::new();
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo_root.to_string_lossy().to_string());

    // 1. VCS: .git can be a directory (normal repo) or a file (worktree)
    if repo_root.join(".git").exists() {
        registry.vcs.insert(
            "git".to_string(),
            Arc::new(GitVcs::new(Arc::clone(&runner))),
        );
        info!(%repo_name, "VCS → git");
    }

    // 2. Checkout manager: config-driven provider selection
    let co_config = config.resolve_checkouts_config(repo_root);
    match co_config.provider.as_str() {
        "wt" => {
            if runner.exists("wt", &["--version"]).await {
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(WtCheckoutManager::new(Arc::clone(&runner))),
                );
                info!(%repo_name, "Checkout mgr → wt (forced)");
            } else {
                tracing::warn!(
                    %repo_name, "provider = \"wt\" but wt not found in PATH, falling back to git"
                );
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(GitCheckoutManager::new(co_config, Arc::clone(&runner))),
                );
            }
        }
        "git" => {
            registry.checkout_managers.insert(
                "git".to_string(),
                Arc::new(GitCheckoutManager::new(co_config, Arc::clone(&runner))),
            );
            info!(%repo_name, "Checkout mgr → git (forced)");
        }
        _ => {
            // Auto: try wt first, fall back to git
            if runner.exists("wt", &["--version"]).await {
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(WtCheckoutManager::new(Arc::clone(&runner))),
                );
                info!(%repo_name, "Checkout mgr → wt");
            } else {
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(GitCheckoutManager::new(co_config, Arc::clone(&runner))),
                );
                info!(%repo_name, "Checkout mgr → git (fallback)");
            }
        }
    }

    // 3. Remote host detection -> code review & issue tracker
    let remote_url = first_remote_url(repo_root, &*runner).await;
    let repo_slug = remote_url.as_deref().and_then(extract_repo_slug);
    if let Some(ref host) = remote_url.as_deref().and_then(detect_host_from_url) {
        if host == "github" && runner.exists("gh", &["--version"]).await {
            if let Some(slug) = repo_slug.clone() {
                let api: Arc<dyn crate::providers::github_api::GhApi> =
                    Arc::new(GhApiClient::new(Arc::clone(&runner)));
                registry.code_review.insert(
                    "github".to_string(),
                    Arc::new(GitHubCodeReview::new(
                        "github".to_string(),
                        slug.clone(),
                        Arc::clone(&api),
                        Arc::clone(&runner),
                    )),
                );
                registry.issue_trackers.insert(
                    "github".to_string(),
                    Arc::new(GitHubIssueTracker::new(
                        "github".to_string(),
                        slug,
                        api,
                        Arc::clone(&runner),
                    )),
                );
                info!(%repo_name, "Code review → GitHub");
                info!(%repo_name, "Issue tracker → GitHub");
            } else {
                warn!(%repo_name, "GitHub detected but could not determine repo slug — skipping GitHub providers");
            }
        }
        // TODO: GitLab support
    }

    // 4. Cloud agents: Cursor (gate on CURSOR_API_KEY to avoid false positives
    //    from unrelated binaries also named `agent`)
    if std::env::var("CURSOR_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        && runner.exists("agent", &["--version"]).await
    {
        registry.cloud_agents.insert(
            "cursor".to_string(),
            Arc::new(CursorCodingAgent::new(
                "cursor".to_string(),
                Arc::new(crate::providers::ReqwestHttpClient::new()),
            )),
        );
        info!(%repo_name, "Cloud agent → Cursor Cloud Agents");
    }

    // 4b. Cloud agent: Codex (gated on auth file, not binary — provider uses API directly)
    if super::coding_agent::codex::codex_auth_file_exists() {
        registry.cloud_agents.insert(
            "codex".to_string(),
            Arc::new(CodexCodingAgent::new(
                "codex".to_string(),
                Arc::new(crate::providers::ReqwestHttpClient::new()),
            )),
        );
        info!(%repo_name, "Cloud agent → Codex");
    }

    // 5. Cloud agent: Claude Code Web & AI utility
    if let Some(claude_bin) = resolve_claude_path(&*runner).await {
        registry.cloud_agents.insert(
            "claude".to_string(),
            Arc::new(ClaudeCodingAgent::new(
                "claude".to_string(),
                Arc::clone(&runner),
                Arc::new(crate::providers::ReqwestHttpClient::new()),
            )),
        );
        registry.ai_utilities.insert(
            "claude".to_string(),
            Arc::new(ClaudeAiUtility::new(claude_bin, Arc::clone(&runner))),
        );
        info!(%repo_name, "Cloud agent → Claude Code Web");
        info!(%repo_name, "AI utility → Claude");
    }

    // 6. Workspace manager: prefer env-var detection (proves we're *inside* the terminal)
    //    over binary-exists checks (just means the app is installed).
    if std::env::var("CMUX_SOCKET_PATH").is_ok() {
        let cmux_bin = Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux");
        if cmux_bin.exists() {
            registry.workspace_manager = Some((
                "cmux".to_string(),
                Arc::new(CmuxWorkspaceManager::new(Arc::clone(&runner))),
            ));
            info!(%repo_name, "Workspace mgr → cmux");
        }
    } else if std::env::var("ZELLIJ").is_ok() {
        if ZellijWorkspaceManager::check_version(&*runner)
            .await
            .is_ok()
        {
            registry.workspace_manager = Some((
                "zellij".to_string(),
                Arc::new(ZellijWorkspaceManager::new(Arc::clone(&runner))),
            ));
            info!(%repo_name, "Workspace mgr → zellij");
        }
    } else if std::env::var("TMUX").is_ok() {
        registry.workspace_manager = Some((
            "tmux".to_string(),
            Arc::new(TmuxWorkspaceManager::new(Arc::clone(&runner))),
        ));
        info!(%repo_name, "Workspace mgr → tmux");
    } else {
        // Fallback: cmux binary exists but not running inside cmux
        let cmux_bin = Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux");
        if cmux_bin.exists() {
            registry.workspace_manager = Some((
                "cmux".to_string(),
                Arc::new(CmuxWorkspaceManager::new(Arc::clone(&runner))),
            ));
            info!(%repo_name, "Workspace mgr → cmux (binary found, not running inside cmux)");
        }
    }

    // 7. Terminal pool: prefer shpool if available, fall back to passthrough
    if runner.exists("shpool", &["version"]).await {
        let shpool_socket = crate::config::flotilla_config_dir().join("shpool/shpool.socket");
        registry.terminal_pool = Some((
            "shpool".into(),
            Arc::new(crate::providers::terminal::shpool::ShpoolTerminalPool::new(
                Arc::clone(&runner),
                shpool_socket,
            )),
        ));
        info!(%repo_name, "Terminal pool → shpool");
    } else {
        registry.terminal_pool = Some((
            "passthrough".into(),
            Arc::new(crate::providers::terminal::passthrough::PassthroughTerminalPool),
        ));
        info!(%repo_name, "Terminal pool → passthrough (no persistence)");
    }

    if follower {
        info!(%repo_name, "follower mode — stripping external providers");
        registry.strip_external_providers();
    }

    (registry, repo_slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    type ResponseMap = HashMap<(String, String), Vec<Result<String, String>>>;

    struct DiscoveryMockRunnerBuilder {
        responses: ResponseMap,
        tool_exists: HashMap<String, bool>,
    }

    struct DiscoveryMockRunner {
        responses: Mutex<ResponseMap>,
        tool_exists: HashMap<String, bool>,
        seen_cwds: Mutex<Vec<PathBuf>>,
        exists_calls: Mutex<Vec<(String, String)>>,
    }

    impl DiscoveryMockRunner {
        fn builder() -> DiscoveryMockRunnerBuilder {
            DiscoveryMockRunnerBuilder {
                responses: HashMap::new(),
                tool_exists: HashMap::new(),
            }
        }
    }

    impl DiscoveryMockRunnerBuilder {
        fn on_run(mut self, cmd: &str, args: &[&str], response: Result<String, String>) -> Self {
            let key = (cmd.to_string(), args.join(" "));
            self.responses.entry(key).or_default().push(response);
            self
        }

        fn tool_exists(mut self, cmd: &str, exists: bool) -> Self {
            self.tool_exists.insert(cmd.to_string(), exists);
            self
        }

        fn build(self) -> DiscoveryMockRunner {
            DiscoveryMockRunner {
                responses: Mutex::new(self.responses),
                tool_exists: self.tool_exists,
                seen_cwds: Mutex::new(Vec::new()),
                exists_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl DiscoveryMockRunner {
        fn saw_cwd(&self, cwd: &Path) -> bool {
            self.seen_cwds.lock().unwrap().iter().any(|p| p == cwd)
        }

        fn exists_call_count(&self, cmd: &str) -> usize {
            self.exists_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(called, _)| called == cmd)
                .count()
        }
    }

    #[async_trait]
    impl CommandRunner for DiscoveryMockRunner {
        async fn run(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            _label: &super::super::ChannelLabel,
        ) -> Result<String, String> {
            self.seen_cwds.lock().unwrap().push(cwd.to_path_buf());
            let key = (cmd.to_string(), args.join(" "));
            let mut map = self.responses.lock().unwrap();
            if let Some(queue) = map.get_mut(&key) {
                if !queue.is_empty() {
                    return queue.remove(0);
                }
            }
            Err(format!(
                "DiscoveryMockRunner: no response for {cmd} {}",
                args.join(" ")
            ))
        }

        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &super::super::ChannelLabel,
        ) -> Result<super::super::CommandOutput, String> {
            match self.run(cmd, args, cwd, label).await {
                Ok(stdout) => Ok(super::super::CommandOutput {
                    stdout,
                    stderr: String::new(),
                    success: true,
                }),
                Err(stderr) => Ok(super::super::CommandOutput {
                    stdout: String::new(),
                    stderr,
                    success: false,
                }),
            }
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            self.exists_calls
                .lock()
                .unwrap()
                .push((cmd.to_string(), args.join(" ")));
            self.tool_exists.get(cmd).copied().unwrap_or(false)
        }
    }

    fn make_repo_with_git_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        (dir, repo)
    }

    fn make_repo_with_git_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        std::fs::write(repo.join(".git"), "gitdir: /some/path\n").unwrap();
        (dir, repo)
    }

    fn temp_config(dir: &tempfile::TempDir) -> ConfigStore {
        ConfigStore::with_base(dir.path().join("config-base"))
    }

    fn config_with_provider(
        dir: &tempfile::TempDir,
        repo_root: &Path,
        provider: &str,
    ) -> ConfigStore {
        let base = dir.path().join("config-base");
        let repos_dir = base.join("repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        let slug = crate::config::path_to_slug(repo_root);
        let repo_file = repos_dir.join(format!("{slug}.toml"));
        let content = format!(
            "path = \"{}\"\n[vcs.git.checkouts]\nprovider = \"{}\"\n",
            repo_root.display(),
            provider
        );
        std::fs::write(repo_file, content).unwrap();
        ConfigStore::with_base(base)
    }

    #[test]
    fn test_extract_repo_identity() {
        let id = extract_repo_identity("git@github.com:rjwittams/flotilla.git");
        assert_eq!(
            id,
            Some(flotilla_protocol::RepoIdentity {
                authority: "github.com".into(),
                path: "rjwittams/flotilla".into(),
            })
        );
    }

    #[test]
    fn extract_repo_slug_cases() {
        let cases: [(&str, Option<&str>); 12] = [
            ("git@github.com:owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo.git", Some("owner/repo")),
            ("http://github.com/owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo/", Some("owner/repo")),
            (
                "git@myserver.example.com:team/project.git",
                Some("team/project"),
            ),
            ("https://github.com/org/sub/repo.git", Some("org/sub/repo")),
            ("git@github.com:repo", None),
            ("https://github.com/", None),
            ("https://github.com/owner", None),
            ("ftp://github.com/owner/repo.git", None),
            ("   ", None),
        ];
        for (url, expected) in cases {
            let expected = expected.map(str::to_string);
            assert_eq!(
                extract_repo_slug(url),
                expected,
                "unexpected slug for: {url}"
            );
        }
    }

    #[test]
    fn detect_host_from_url_cases() {
        let cases: [(&str, Option<&str>); 6] = [
            ("git@github.com:owner/repo.git", Some("github")),
            ("https://GitHub.com/owner/repo", Some("github")),
            ("http://github.com/owner/repo", Some("github")),
            ("https://gitlab.mycompany.com/org/project", Some("gitlab")),
            ("https://bitbucket.org/owner/repo", None),
            ("", None),
        ];
        for (url, expected) in cases {
            let expected = expected.map(str::to_string);
            assert_eq!(
                detect_host_from_url(url),
                expected,
                "unexpected host for: {url}"
            );
        }
    }

    #[tokio::test]
    async fn first_remote_prefers_tracking_remote() {
        let repo_root = Path::new("/tmp/repo-root");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Ok("upstream/main\n".to_string()),
            )
            .on_run("git", &["remote"], Ok("origin\nupstream\n".to_string()))
            .on_run(
                "git",
                &["remote", "get-url", "upstream"],
                Ok("  https://github.com/upstream/repo.git  \n".to_string()),
            )
            .build();
        let url = first_remote_url(repo_root, &runner).await;
        assert_eq!(
            url,
            Some("https://github.com/upstream/repo.git".to_string())
        );
        assert!(runner.saw_cwd(repo_root));
    }

    #[tokio::test]
    async fn first_remote_prefers_tracking_remote_with_slash_in_name_over_origin() {
        let repo_root = Path::new("/tmp/repo-root");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Ok("team/origin/main\n".to_string()),
            )
            .on_run("git", &["remote"], Ok("origin\nteam/origin\n".to_string()))
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("https://github.com/wrong/repo.git\n".to_string()),
            )
            .on_run(
                "git",
                &["remote", "get-url", "team/origin"],
                Ok("https://github.com/team/repo.git\n".to_string()),
            )
            .build();
        let url = first_remote_url(repo_root, &runner).await;
        assert_eq!(url, Some("https://github.com/team/repo.git".to_string()));
    }

    #[tokio::test]
    async fn first_remote_falls_back_to_origin_when_no_tracking_branch() {
        let repo_root = Path::new("/tmp/repo-root");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream configured".to_string()),
            )
            .on_run(
                "git",
                &["remote"],
                Ok("fork\norigin\nupstream\n".to_string()),
            )
            .on_run(
                "git",
                &["remote", "get-url", "origin"],
                Ok("  https://github.com/owner/repo.git  \n".to_string()),
            )
            .build();
        let url = first_remote_url(repo_root, &runner).await;
        assert_eq!(url, Some("https://github.com/owner/repo.git".to_string()));
    }

    #[tokio::test]
    async fn first_remote_falls_back_to_first_when_no_origin() {
        let repo_root = Path::new("/tmp/repo-root");
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream configured".to_string()),
            )
            .on_run("git", &["remote"], Ok("fork\nupstream\n".to_string()))
            .on_run(
                "git",
                &["remote", "get-url", "fork"],
                Ok("https://github.com/fork/repo.git\n".to_string()),
            )
            .build();
        let url = first_remote_url(repo_root, &runner).await;
        assert_eq!(url, Some("https://github.com/fork/repo.git".to_string()));
    }

    #[tokio::test]
    async fn first_remote_skips_failed_remote_and_uses_next() {
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "git",
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                Err("fatal: no upstream".to_string()),
            )
            .on_run(
                "git",
                &["remote"],
                Ok("bad-remote\ngood-remote\n".to_string()),
            )
            .on_run(
                "git",
                &["remote", "get-url", "bad-remote"],
                Err("no such remote".to_string()),
            )
            .on_run(
                "git",
                &["remote", "get-url", "good-remote"],
                Ok("https://github.com/owner/repo.git\n".to_string()),
            )
            .build();
        let url = first_remote_url(Path::new("/tmp"), &runner).await;
        assert_eq!(url, Some("https://github.com/owner/repo.git".to_string()));
    }

    #[tokio::test]
    async fn first_remote_returns_none_when_listing_fails_or_empty() {
        let cases = [
            DiscoveryMockRunner::builder()
                .on_run(
                    "git",
                    &["rev-parse", "--abbrev-ref", "@{upstream}"],
                    Err("fatal: no upstream".to_string()),
                )
                .on_run(
                    "git",
                    &["remote"],
                    Err("fatal: not a git repository".to_string()),
                )
                .build(),
            DiscoveryMockRunner::builder()
                .on_run(
                    "git",
                    &["rev-parse", "--abbrev-ref", "@{upstream}"],
                    Err("fatal: no upstream".to_string()),
                )
                .on_run("git", &["remote"], Ok(String::new()))
                .build(),
            DiscoveryMockRunner::builder()
                .on_run(
                    "git",
                    &["rev-parse", "--abbrev-ref", "@{upstream}"],
                    Err("fatal: no upstream".to_string()),
                )
                .on_run("git", &["remote"], Ok("\n\n".to_string()))
                .build(),
        ];
        for runner in cases {
            assert_eq!(first_remote_url(Path::new("/tmp"), &runner).await, None);
        }
    }

    fn discovery_runner() -> DiscoveryMockRunnerBuilder {
        DiscoveryMockRunner::builder()
    }

    #[tokio::test]
    async fn detect_providers_without_git_has_no_vcs_but_has_checkout_manager() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("not a repo".to_string()))
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.vcs.is_empty());
        assert!(!registry.checkout_managers.is_empty());
        assert_eq!(slug, None);
    }

    #[tokio::test]
    async fn detect_providers_git_file_counts_as_vcs() {
        let (dir, repo) = make_repo_with_git_file();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let (registry, _) = detect_providers(&repo, &config, runner).await;
        assert!(registry.vcs.contains_key("git"));
    }

    #[tokio::test]
    async fn detect_providers_github_with_gh_registers_review_and_issues() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Ok("origin\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("git@github.com:owner/repo.git\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", true)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.contains_key("github"));
        assert!(registry.issue_trackers.contains_key("github"));
        assert_eq!(slug, Some("owner/repo".to_string()));
    }

    #[tokio::test]
    async fn detect_providers_github_without_gh_skips_review_and_issues() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Ok("origin\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("git@github.com:owner/repo.git\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.is_empty());
        assert!(registry.issue_trackers.is_empty());
        assert_eq!(slug, Some("owner/repo".to_string()));
    }

    #[tokio::test]
    async fn detect_providers_github_with_unparseable_slug_skips_review_and_issues() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Ok("origin\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("https://github.com/\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", true)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.is_empty());
        assert!(registry.issue_trackers.is_empty());
        assert_eq!(slug, None);
    }

    #[tokio::test]
    async fn detect_providers_non_github_remote_skips_github_providers() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Ok("origin\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("https://bitbucket.org/owner/repo.git\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", true)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.is_empty());
        assert!(registry.issue_trackers.is_empty());
        assert_eq!(slug, Some("owner/repo".to_string()));
    }

    #[tokio::test]
    async fn detect_providers_claude_registration_depends_on_binary() {
        for (has_claude, should_register) in [(true, true), (false, false)] {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("agent", false)
                    .tool_exists("claude", has_claude)
                    .build(),
            );

            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert_eq!(
                registry.cloud_agents.contains_key("claude"),
                should_register
            );
            assert_eq!(
                registry.ai_utilities.contains_key("claude"),
                should_register
            );
        }
    }

    #[tokio::test]
    async fn detect_providers_cursor_registration_depends_on_binary_and_api_key() {
        // With CURSOR_API_KEY set and agent binary present → registered
        std::env::set_var("CURSOR_API_KEY", "test-key");
        {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("agent", true)
                    .tool_exists("claude", false)
                    .build(),
            );
            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert!(registry.cloud_agents.contains_key("cursor"));
        }
        std::env::remove_var("CURSOR_API_KEY");

        // Without CURSOR_API_KEY, agent binary alone is not enough
        {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("agent", true)
                    .tool_exists("claude", false)
                    .build(),
            );
            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert!(!registry.cloud_agents.contains_key("cursor"));
        }

        // Without agent binary → not registered regardless of env var
        {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("agent", false)
                    .tool_exists("claude", false)
                    .build(),
            );
            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert!(!registry.cloud_agents.contains_key("cursor"));
        }
    }

    #[tokio::test]
    async fn detect_providers_config_git_skips_wt_probe() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = config_with_provider(&dir, &repo, "git");
        let runner = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", true)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let runner_dyn: Arc<dyn CommandRunner> = runner.clone();
        let (registry, _) = detect_providers(&repo, &config, runner_dyn).await;
        assert!(!registry.checkout_managers.is_empty());
        assert_eq!(runner.exists_call_count("wt"), 0);
    }

    #[tokio::test]
    async fn detect_providers_config_wt_probes_wt_binary() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = config_with_provider(&dir, &repo, "wt");
        let runner = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let runner_dyn: Arc<dyn CommandRunner> = runner.clone();
        let (registry, _) = detect_providers(&repo, &config, runner_dyn).await;
        assert!(!registry.checkout_managers.is_empty());
        assert_eq!(runner.exists_call_count("wt"), 1);
    }

    #[tokio::test]
    async fn detect_providers_config_auto_probes_wt_binary() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", true)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let runner_dyn: Arc<dyn CommandRunner> = runner.clone();
        let (registry, _) = detect_providers(&repo, &config, runner_dyn).await;
        assert!(!registry.checkout_managers.is_empty());
        assert_eq!(runner.exists_call_count("wt"), 1);
    }

    #[tokio::test]
    async fn detect_providers_codex_registration_depends_on_auth_file() {
        let _lock = crate::providers::coding_agent::codex::CODEX_TEST_LOCK
            .lock()
            .await;

        // With auth.json present → registered
        let codex_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            codex_dir.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"t","account_id":"a"}}"#,
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", codex_dir.path());
        {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("claude", false)
                    .build(),
            );
            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert!(registry.cloud_agents.contains_key("codex"));
        }

        // Without auth.json → not registered
        std::fs::remove_file(codex_dir.path().join("auth.json")).unwrap();
        {
            let (dir, repo) = make_repo_with_git_dir();
            let config = temp_config(&dir);
            let runner: Arc<dyn CommandRunner> = Arc::new(
                discovery_runner()
                    .on_run("git", &["remote"], Err("no remotes".to_string()))
                    .tool_exists("wt", false)
                    .tool_exists("gh", false)
                    .tool_exists("claude", false)
                    .build(),
            );
            let (registry, _) = detect_providers(&repo, &config, runner).await;
            assert!(!registry.cloud_agents.contains_key("codex"));
        }

        std::env::remove_var("CODEX_HOME");
    }

    #[tokio::test]
    async fn detect_providers_prefers_origin_over_alphabetical_first() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        // "fork" sorts before "origin" alphabetically, but origin should win
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Ok("fork\norigin\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "origin"],
                    Ok("https://github.com/owner/repo.git\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", true)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.contains_key("github"));
        assert!(registry.issue_trackers.contains_key("github"));
        assert_eq!(slug, Some("owner/repo".to_string()));
    }

    #[tokio::test]
    async fn detect_providers_prefers_tracking_remote() {
        let (dir, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run(
                    "git",
                    &["rev-parse", "--abbrev-ref", "@{upstream}"],
                    Ok("upstream/main\n".to_string()),
                )
                .on_run("git", &["remote"], Ok("origin\nupstream\n".to_string()))
                .on_run(
                    "git",
                    &["remote", "get-url", "upstream"],
                    Ok("https://github.com/upstream/repo.git\n".to_string()),
                )
                .tool_exists("wt", false)
                .tool_exists("gh", true)
                .tool_exists("claude", false)
                .build(),
        );

        let (registry, slug) = detect_providers(&repo, &config, runner).await;
        assert!(registry.code_review.contains_key("github"));
        assert!(registry.issue_trackers.contains_key("github"));
        assert_eq!(slug, Some("upstream/repo".to_string()));
    }
}
