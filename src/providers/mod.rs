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

/// Shared helper: run a command and return stdout on success, stderr on failure.
pub(crate) async fn run_cmd(cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}
