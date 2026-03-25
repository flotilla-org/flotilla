//! Codex auth file host detector.
//!
//! Checks whether the Codex auth file (`auth.json`) exists under `$CODEX_HOME`
//! (or `~/.codex` by default), indicating that the user has authenticated with
//! the Codex CLI.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::providers::{
    discovery::{EnvVars, EnvironmentAssertion, HostDetector},
    CommandRunner,
};

/// Returns the Codex home directory: `$CODEX_HOME` or `$HOME/.codex`.
/// Returns `None` when neither env var is available.
fn codex_home(env: &dyn EnvVars) -> Option<PathBuf> {
    if let Some(val) = env.get("CODEX_HOME") {
        Some(PathBuf::from(val))
    } else {
        env.get("HOME").map(|h| PathBuf::from(h).join(".codex"))
    }
}

/// Detects whether a Codex auth file exists.
pub struct CodexAuthDetector;

#[async_trait]
impl HostDetector for CodexAuthDetector {
    async fn detect(&self, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        let Some(home) = codex_home(env) else {
            return vec![];
        };
        let auth_path = home.join("auth.json");
        // Check existence via runner (test -f) rather than local filesystem
        let Some(path_str) = auth_path.to_str() else {
            return vec![]; // non-UTF-8 path — can't pass to runner
        };
        if runner.exists("test", &["-f", path_str]).await {
            vec![EnvironmentAssertion::auth_file("codex", auth_path)]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars};

    #[tokio::test]
    async fn codex_auth_detector_found_via_codex_home() {
        let runner = DiscoveryMockRunner::builder().tool_exists("test", true).build();
        let env = TestEnvVars::new([("CODEX_HOME", "/mock/codex")]);
        let assertions = CodexAuthDetector.detect(&runner, &env).await;

        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::AuthFileExists { provider, path } => {
                assert_eq!(provider, "codex");
                assert_eq!(path.as_path(), Path::new("/mock/codex/auth.json"));
            }
            other => panic!("expected AuthFileExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_auth_detector_found_via_home() {
        let runner = DiscoveryMockRunner::builder().tool_exists("test", true).build();
        let env = TestEnvVars::new([("HOME", "/mock/home")]);
        let assertions = CodexAuthDetector.detect(&runner, &env).await;

        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::AuthFileExists { provider, path } => {
                assert_eq!(provider, "codex");
                assert_eq!(path.as_path(), Path::new("/mock/home/.codex/auth.json"));
            }
            other => panic!("expected AuthFileExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_auth_detector_not_found() {
        // test -f returns false
        let runner = DiscoveryMockRunner::builder().tool_exists("test", false).build();
        let env = TestEnvVars::new([("CODEX_HOME", "/mock/codex")]);
        let assertions = CodexAuthDetector.detect(&runner, &env).await;

        assert!(assertions.is_empty());
    }

    #[tokio::test]
    async fn codex_auth_detector_no_home() {
        // Neither CODEX_HOME nor HOME set
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = CodexAuthDetector.detect(&runner, &TestEnvVars::default()).await;

        assert!(assertions.is_empty());
    }
}
