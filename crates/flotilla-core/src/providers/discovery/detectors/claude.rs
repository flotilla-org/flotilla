//! Claude CLI host detector.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::{
    discovery::{detectors::generic::parse_first_dotted_version, EnvVars, EnvironmentAssertion, HostDetector},
    run, CommandRunner,
};

/// Detects the `claude` CLI, checking PATH first, then known install locations.
pub struct ClaudeDetector;

#[async_trait]
impl HostDetector for ClaudeDetector {
    async fn detect(&self, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        // 1. Check PATH — single call proves existence and captures version
        if let Ok(output) = run!(runner, "claude", &["--version"], Path::new(".")) {
            return match parse_first_dotted_version(&output) {
                Some(version) => {
                    vec![EnvironmentAssertion::versioned_binary("claude", "claude", version)]
                }
                None => vec![EnvironmentAssertion::binary("claude", "claude")],
            };
        }

        // 2. Check known installation locations via runner (not local filesystem)
        let known_paths: Vec<PathBuf> = env.get("HOME").into_iter().map(|h| PathBuf::from(h).join(".claude/local/claude")).collect();
        for path in known_paths {
            let path_str = path.to_str().unwrap_or("");
            if let Ok(output) = run!(runner, path_str, &["--version"], Path::new(".")) {
                return match parse_first_dotted_version(&output) {
                    Some(version) => {
                        vec![EnvironmentAssertion::versioned_binary("claude", &path, version)]
                    }
                    None => vec![EnvironmentAssertion::binary("claude", &path)],
                };
            }
        }

        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars},
    };

    #[tokio::test]
    async fn claude_detector_found_on_path() {
        let runner = DiscoveryMockRunner::builder().on_run("claude", &["--version"], Ok("1.0.20 (Claude Code)\n".into())).build();
        let assertions = ClaudeDetector.detect(&runner, &TestEnvVars::default()).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::BinaryAvailable { name, path, version } => {
                assert_eq!(name, "claude");
                assert_eq!(*path, ExecutionEnvironmentPath::new("claude"));
                assert_eq!(version.as_deref(), Some("1.0.20"));
            }
            other => panic!("expected BinaryAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claude_detector_fallback_to_home() {
        // Not on PATH, but found at $HOME/.claude/local/claude
        let runner = DiscoveryMockRunner::builder()
            .on_run("/test/home/.claude/local/claude", &["--version"], Ok("1.2.3 (Claude Code)\n".into()))
            .build();
        let env = TestEnvVars::new([("HOME", "/test/home")]);
        let assertions = ClaudeDetector.detect(&runner, &env).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::BinaryAvailable { name, path, version } => {
                assert_eq!(name, "claude");
                assert_eq!(*path, ExecutionEnvironmentPath::new("/test/home/.claude/local/claude"));
                assert_eq!(version.as_deref(), Some("1.2.3"));
            }
            other => panic!("expected BinaryAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claude_detector_not_found() {
        // No on_run configured → run! returns Err for PATH and HOME fallback
        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([("HOME", "/nonexistent")]);
        let assertions = ClaudeDetector.detect(&runner, &env).await;
        assert!(assertions.is_empty());
    }

    #[tokio::test]
    async fn claude_detector_no_home_env() {
        // No HOME env var set, not on PATH → empty
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = ClaudeDetector.detect(&runner, &TestEnvVars::default()).await;
        assert!(assertions.is_empty());
    }
}
