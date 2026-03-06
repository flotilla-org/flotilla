pub mod github_api;
pub mod types;
pub mod vcs;
pub mod code_review;
pub mod issue_tracker;
pub mod coding_agent;
pub mod ai_utility;
pub mod workspace;
pub mod registry;
pub mod correlation;
pub mod discovery;

use std::path::Path;

/// Shared helper: run a command directly and return stdout on success,
/// stderr on failure. Stdin is detached so subprocesses cannot interfere
/// with the parent terminal.
pub async fn run_cmd(cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

/// Check if a command is available by running it directly.
pub fn command_exists(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve the path to the `claude` CLI binary.
/// Checks PATH first, then known installation locations.
pub fn resolve_claude_path() -> Option<String> {
    // Try PATH directly
    if command_exists("claude", &["--version"]) {
        return Some("claude".to_string());
    }
    // Known installation paths
    let known_paths = [
        dirs::home_dir().map(|h| h.join(".claude/local/claude")),
    ];
    for path in known_paths.into_iter().flatten() {
        if path.is_file() && command_exists(path.to_str().unwrap_or(""), &["--version"]) {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}
