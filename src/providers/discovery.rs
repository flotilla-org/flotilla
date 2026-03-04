use std::path::Path;
use std::process::Stdio;

use crate::providers::ai_utility::claude::ClaudeAiUtility;
use crate::providers::code_review::github::GitHubCodeReview;
use crate::providers::coding_agent::claude::ClaudeCodingAgent;
use crate::providers::issue_tracker::github::GitHubIssueTracker;
use crate::providers::registry::ProviderRegistry;
use crate::providers::vcs::git::GitVcs;
use crate::providers::vcs::wt::WtCheckoutManager;
use crate::providers::workspace::cmux::CmuxWorkspaceManager;

/// Run a command silently, returning true if it exits successfully.
fn command_exists(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `git remote get-url origin` and check if the URL contains a known host.
/// Returns "github" or "gitlab" if matched, None otherwise.
fn detect_remote_host(repo_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).to_string();
    let url_lower = url.to_lowercase();

    if url_lower.contains("github.com") {
        Some("github".to_string())
    } else if url_lower.contains("gitlab") {
        Some("gitlab".to_string())
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

    // 1. VCS: .git can be a directory (normal repo) or a file (worktree)
    if repo_root.join(".git").exists() {
        registry
            .vcs
            .insert("git".to_string(), Box::new(GitVcs::new()));
    }

    // 2. Checkout manager: wt
    if command_exists("wt", &["--version"]) {
        registry
            .checkout_managers
            .insert("git".to_string(), Box::new(WtCheckoutManager::new()));
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
        }
        // TODO: GitLab support
    }

    // 4. Coding agent: claude
    if command_exists("claude", &["--version"]) {
        registry.coding_agents.insert(
            "claude".to_string(),
            Box::new(ClaudeCodingAgent::new("claude".to_string())),
        );

        // 5. AI utility: claude (same binary check)
        registry
            .ai_utilities
            .insert("claude".to_string(), Box::new(ClaudeAiUtility::new()));
    }

    // 6. Workspace manager: cmux
    // Check for the cmux binary at the known path
    let cmux_bin = Path::new("/Applications/cmux.app/Contents/Resources/bin/cmux");
    if cmux_bin.exists() {
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Box::new(CmuxWorkspaceManager::new("cmux".to_string())),
        ));
    }
    // TODO: check $ZELLIJ env var for Zellij workspace manager
    // TODO: check $TMUX env var for tmux workspace manager

    registry
}
