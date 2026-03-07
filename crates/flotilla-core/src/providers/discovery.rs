use std::path::Path;
use std::sync::Arc;

use super::{resolve_claude_path, CommandRunner};
use crate::config::ConfigStore;
use crate::providers::ai_utility::claude::ClaudeAiUtility;
use crate::providers::code_review::github::GitHubCodeReview;
use crate::providers::coding_agent::claude::ClaudeCodingAgent;
use crate::providers::github_api::GhApiClient;
use crate::providers::issue_tracker::github::GitHubIssueTracker;
use crate::providers::registry::ProviderRegistry;
use crate::providers::vcs::git::GitVcs;
use crate::providers::vcs::git_worktree::GitCheckoutManager;
use crate::providers::vcs::wt::WtCheckoutManager;
use crate::providers::workspace::cmux::CmuxWorkspaceManager;
use crate::providers::workspace::tmux::TmuxWorkspaceManager;
use crate::providers::workspace::zellij::ZellijWorkspaceManager;
use tracing::{info, warn};

/// Extract the first git remote URL for this repo.
pub async fn first_remote_url(repo_root: &Path, runner: &dyn CommandRunner) -> Option<String> {
    let remotes_output = runner.run("git", &["remote"], repo_root).await.ok()?;
    for remote in remotes_output.lines() {
        let remote = remote.trim();
        if remote.is_empty() {
            continue;
        }
        if let Ok(url) = runner
            .run("git", &["remote", "get-url", remote], repo_root)
            .await
        {
            return Some(url.trim().to_string());
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
/// 4. Coding agent: check for `claude` CLI
/// 5. AI utility: check for `claude` CLI
/// 6. Workspace manager: check for cmux binary
pub async fn detect_providers(
    repo_root: &Path,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
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
        info!("{repo_name}: VCS → git");
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
                info!("{repo_name}: Checkout mgr → wt (forced)");
            } else {
                tracing::warn!(
                    "{repo_name}: provider = \"wt\" but wt not found in PATH, falling back to git"
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
            info!("{repo_name}: Checkout mgr → git (forced)");
        }
        _ => {
            // Auto: try wt first, fall back to git
            if runner.exists("wt", &["--version"]).await {
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(WtCheckoutManager::new(Arc::clone(&runner))),
                );
                info!("{repo_name}: Checkout mgr → wt");
            } else {
                registry.checkout_managers.insert(
                    "git".to_string(),
                    Arc::new(GitCheckoutManager::new(co_config, Arc::clone(&runner))),
                );
                info!("{repo_name}: Checkout mgr → git (fallback)");
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
                info!("{repo_name}: Code review → GitHub");
                info!("{repo_name}: Issue tracker → GitHub");
            } else {
                warn!("{repo_name}: GitHub detected but could not determine repo slug — skipping GitHub providers");
            }
        }
        // TODO: GitLab support
    }

    // 4. Coding agent & AI utility: claude
    if let Some(claude_bin) = resolve_claude_path(&*runner).await {
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(ClaudeCodingAgent::new(
                "claude".to_string(),
                Arc::clone(&runner),
            )),
        );
        registry.ai_utilities.insert(
            "claude".to_string(),
            Arc::new(ClaudeAiUtility::new(claude_bin, Arc::clone(&runner))),
        );
        info!("{repo_name}: Coding agent → Claude Sessions");
        info!("{repo_name}: AI utility → Claude");
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
            info!("{repo_name}: Workspace mgr → cmux");
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
            info!("{repo_name}: Workspace mgr → zellij");
        }
    } else if std::env::var("TMUX").is_ok() {
        registry.workspace_manager = Some((
            "tmux".to_string(),
            Arc::new(TmuxWorkspaceManager::new(Arc::clone(&runner))),
        ));
        info!("{repo_name}: Workspace mgr → tmux");
    } else {
        // Fallback: cmux binary exists but not running inside cmux
        let cmux_bin = Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux");
        if cmux_bin.exists() {
            registry.workspace_manager = Some((
                "cmux".to_string(),
                Arc::new(CmuxWorkspaceManager::new(Arc::clone(&runner))),
            ));
            info!("{repo_name}: Workspace mgr → cmux (binary found, not running inside cmux)");
        }
    }

    (registry, repo_slug)
}
