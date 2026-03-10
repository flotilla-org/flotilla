pub mod ai_utility;
pub mod code_review;
pub mod coding_agent;
pub mod correlation;
pub mod discovery;
pub mod github_api;
pub mod issue_tracker;
pub mod registry;
pub mod terminal;
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

/// Trait abstracting HTTP request execution so providers can be tested
/// without making real network calls.
///
/// Uses reqwest::Request as input (callers build with the reqwest builder API)
/// and returns http::Response<bytes::Bytes> (the standard Rust HTTP type that
/// reqwest is built on, trivially constructable in tests).
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String>;
}

/// Production implementation that delegates to `reqwest::Client`.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(|e| e.to_string())
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
pub mod replay;

#[cfg(test)]
pub mod testing {
    use super::*;
    use async_trait::async_trait;
    use std::collections::VecDeque;

    /// A mock command runner that returns canned responses in order.
    /// Each call to `run()` pops the next response from the queue.
    pub struct MockRunner {
        responses: std::sync::Mutex<VecDeque<Result<String, String>>>,
    }

    impl MockRunner {
        pub fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path) -> Result<String, String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockRunner: no more responses")
        }

        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
        ) -> Result<CommandOutput, String> {
            match self.run(cmd, args, cwd).await {
                Ok(stdout) => Ok(CommandOutput {
                    stdout,
                    stderr: String::new(),
                    success: true,
                }),
                Err(stderr) => Ok(CommandOutput {
                    stdout: String::new(),
                    stderr,
                    success: false,
                }),
            }
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }
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
