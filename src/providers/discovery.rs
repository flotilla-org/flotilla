use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use super::{command_exists, resolve_claude_path};
use tracing::info;
use crate::providers::ai_utility::claude::ClaudeAiUtility;
use crate::providers::code_review::github::GitHubCodeReview;
use crate::providers::coding_agent::claude::ClaudeCodingAgent;
use crate::providers::issue_tracker::github::GitHubIssueTracker;
use crate::providers::registry::ProviderRegistry;
use crate::providers::vcs::git::GitVcs;
use crate::providers::vcs::wt::WtCheckoutManager;
use crate::providers::workspace::cmux::CmuxWorkspaceManager;
use crate::providers::workspace::zellij::ZellijWorkspaceManager;

/// Extract the first git remote URL for this repo.
pub fn first_remote_url(repo_root: &Path) -> Option<String> {
    let remotes_output = std::process::Command::new("git")
        .args(["remote"])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !remotes_output.status.success() {
        return None;
    }

    let remotes = String::from_utf8_lossy(&remotes_output.stdout);
    for remote in remotes.lines() {
        let remote = remote.trim();
        if remote.is_empty() {
            continue;
        }
        let url_output = std::process::Command::new("git")
            .args(["remote", "get-url", remote])
            .current_dir(repo_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok();

        if let Some(output) = url_output {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }
    }
    None
}

/// Check all git remotes for known hosts.
/// Returns "github" or "gitlab" if matched, None otherwise.
fn detect_remote_host(repo_root: &Path) -> Option<String> {
    let url = first_remote_url(repo_root)?;
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
pub fn detect_providers(repo_root: &Path) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo_root.to_string_lossy().to_string());

    // 1. VCS: .git can be a directory (normal repo) or a file (worktree)
    if repo_root.join(".git").exists() {
        registry
            .vcs
            .insert("git".to_string(), Box::new(GitVcs::new()));
        info!("{repo_name}: VCS → git");
    }

    // 2. Checkout manager: wt
    if command_exists("wt", &["--version"]) {
        registry
            .checkout_managers
            .insert("git".to_string(), Box::new(WtCheckoutManager::new()));
        info!("{repo_name}: Checkout mgr → wt");
    }
    // TODO: fallback to plain git worktree manager when wt is not available

    // 3. Remote host detection -> code review & issue tracker
    if let Some(ref host) = detect_remote_host(repo_root) {
        if host == "github" && command_exists("gh", &["--version"]) {
            registry.code_review.insert(
                "github".to_string(),
                Box::new(GitHubCodeReview::new("github".to_string())),
            );
            registry.issue_trackers.insert(
                "github".to_string(),
                Box::new(GitHubIssueTracker::new("github".to_string())),
            );
            info!("{repo_name}: Code review → GitHub");
            info!("{repo_name}: Issue tracker → GitHub");
        }
        // TODO: GitLab support
    }

    // 4. Coding agent & AI utility: claude
    if let Some(claude_bin) = resolve_claude_path() {
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(ClaudeCodingAgent::new("claude".to_string())),
        );
        registry
            .ai_utilities
            .insert("claude".to_string(), Box::new(ClaudeAiUtility::new(claude_bin)));
        info!("{repo_name}: Coding agent → Claude Sessions");
        info!("{repo_name}: AI utility → Claude");
    }

    // 6. Workspace manager: cmux
    // Check for the cmux binary at the known path
    let cmux_bin = Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux");
    if cmux_bin.exists() {
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Box::new(CmuxWorkspaceManager::new()),
        ));
        info!("{repo_name}: Workspace mgr → cmux");
    }
    // 7. Workspace manager: zellij (if cmux not already registered)
    if registry.workspace_manager.is_none() && std::env::var("ZELLIJ").is_ok() {
        if ZellijWorkspaceManager::check_version().is_ok() {
            registry.workspace_manager = Some((
                "zellij".to_string(),
                Box::new(ZellijWorkspaceManager::new()),
            ));
            info!("{repo_name}: Workspace mgr → zellij");
        }
    }
    // TODO: check $TMUX env var for tmux workspace manager

    registry
}
