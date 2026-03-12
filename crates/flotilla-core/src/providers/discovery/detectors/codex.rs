//! Codex auth file repo detector.
//!
//! Checks whether the Codex auth file (`auth.json`) exists under `$CODEX_HOME`
//! (or `~/.codex` by default), indicating that the user has authenticated with
//! the Codex CLI.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, RepoDetector};
use crate::providers::CommandRunner;

/// Returns the Codex home directory: `$CODEX_HOME` or `~/.codex`.
fn codex_home() -> PathBuf {
    if let Ok(val) = std::env::var("CODEX_HOME") {
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
impl RepoDetector for CodexAuthDetector {
    fn name(&self) -> &str {
        "codex-auth"
    }

    async fn detect(
        &self,
        _repo_root: &Path,
        _runner: &dyn CommandRunner,
    ) -> Vec<EnvironmentAssertion> {
        let auth_path = codex_home().join("auth.json");
        if auth_path.exists() {
            vec![EnvironmentAssertion::AuthFileExists {
                provider: "codex".into(),
                path: auth_path,
            }]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    #[tokio::test]
    async fn codex_auth_detector_found() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tmp.path().join("auth.json"), r#"{"auth_mode":"api-key"}"#)
            .expect("write auth.json");

        unsafe {
            std::env::set_var("CODEX_HOME", tmp.path());
        }

        let runner = DiscoveryMockRunner::builder().build();
        let assertions = CodexAuthDetector
            .detect(Path::new("/unused"), &runner)
            .await;

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

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
        // Empty dir — no auth.json

        unsafe {
            std::env::set_var("CODEX_HOME", tmp.path());
        }

        let runner = DiscoveryMockRunner::builder().build();
        let assertions = CodexAuthDetector
            .detect(Path::new("/unused"), &runner)
            .await;

        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

        assert!(assertions.is_empty());
    }
}
