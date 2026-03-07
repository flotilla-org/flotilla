pub mod ai_utility;
pub mod code_review;
pub mod coding_agent;
pub mod correlation;
pub mod discovery;
pub mod github_api;
pub mod issue_tracker;
pub mod registry;
pub mod types;
pub mod vcs;
pub mod workspace;

use std::path::Path;

use async_trait::async_trait;

/// Raw output from a command, preserving stdout/stderr regardless of exit status.
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Trait abstracting command execution so providers can be tested without
/// spawning real processes.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run a command and return stdout on success, stderr on failure.
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String>;

    /// Run a command and return full output regardless of exit status.
    /// `Err` only if the process could not be spawned at all.
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<CommandOutput, String>;

    /// Check if a command is available by running it.
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool;
}

/// Production implementation that delegates to `tokio::process::Command`.
pub struct ProcessCommandRunner;

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
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

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<CommandOutput, String> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        })
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Resolve the path to the `claude` CLI binary.
/// Checks PATH first, then known installation locations.
pub async fn resolve_claude_path(runner: &dyn CommandRunner) -> Option<String> {
    if runner.exists("claude", &["--version"]).await {
        return Some("claude".to_string());
    }
    let known_paths = [dirs::home_dir().map(|h| h.join(".claude/local/claude"))];
    for path in known_paths.into_iter().flatten() {
        if path.is_file()
            && runner
                .exists(path.to_str().unwrap_or(""), &["--version"])
                .await
        {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn process_runner_echo() {
        let runner = ProcessCommandRunner;
        let result = runner.run("echo", &["hello"], &PathBuf::from("/")).await;
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn process_runner_exists_true() {
        let runner = ProcessCommandRunner;
        assert!(runner.exists("echo", &["test"]).await);
    }

    #[tokio::test]
    async fn process_runner_exists_false() {
        let runner = ProcessCommandRunner;
        assert!(!runner.exists("nonexistent-binary-xyz", &[]).await);
    }
}
