//! Codex auth file host detector.
//!
//! Checks whether the Codex auth file (`auth.json`) exists under `$CODEX_HOME`
//! (or `~/.codex` by default), indicating that the user has authenticated with
//! the Codex CLI.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::providers::discovery::{EnvVars, EnvironmentAssertion, HostDetector};
use crate::providers::CommandRunner;

/// Returns the Codex home directory: `$CODEX_HOME` or `~/.codex`.
fn codex_home(env: &dyn EnvVars) -> PathBuf {
    if let Some(val) = env.get("CODEX_HOME") {
        PathBuf::from(val)
    } else {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".codex")
    }
}

/// Detects whether a Codex auth file exists.
pub struct CodexAuthDetector;

#[async_trait]
impl HostDetector for CodexAuthDetector {
    async fn detect(
        &self,
        _runner: &dyn CommandRunner,
        env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        let auth_path = codex_home(env).join("auth.json");
        if auth_path.exists() {
            vec![EnvironmentAssertion::auth_file("codex", auth_path)]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars};

    #[tokio::test]
    async fn codex_auth_detector_found() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tmp.path().join("auth.json"), r#"{"auth_mode":"api-key"}"#)
            .expect("write auth.json");

        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([(
            "CODEX_HOME",
            tmp.path()
                .to_str()
                .expect("temp path should be valid utf-8"),
        )]);
        let assertions = CodexAuthDetector.detect(&runner, &env).await;

        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::AuthFileExists { provider, path } => {
                assert_eq!(provider, "codex");
                assert_eq!(path, &tmp.path().join("auth.json"));
            }
            other => panic!("expected AuthFileExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_auth_detector_not_found() {
        let tmp = tempfile::tempdir().expect("create tempdir");

        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([(
            "CODEX_HOME",
            tmp.path()
                .to_str()
                .expect("temp path should be valid utf-8"),
        )]);
        let assertions = CodexAuthDetector.detect(&runner, &env).await;

        assert!(assertions.is_empty());
    }
}
