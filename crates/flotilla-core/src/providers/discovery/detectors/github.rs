//! GitHub CLI (`gh`) host detector.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::{run, CommandRunner};

/// Detects whether the `gh` CLI is available on the host.
pub struct GhCliDetector;

#[async_trait]
impl HostDetector for GhCliDetector {
    fn name(&self) -> &str {
        "gh-cli"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        // Single call: proves binary exists and captures version
        let Ok(output) = run!(runner, "gh", &["--version"], Path::new(".")) else {
            return vec![];
        };
        let version = output
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("gh version "))
            .map(|rest| {
                // Take only the version number, strip build date etc.
                rest.split_whitespace().next().unwrap_or(rest).to_string()
            });
        vec![EnvironmentAssertion::BinaryAvailable {
            name: "gh".into(),
            path: PathBuf::from("gh"),
            version,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    #[tokio::test]
    async fn gh_cli_detector_found() {
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "gh",
                &["--version"],
                Ok("gh version 2.49.0 (2024-05-13)\nhttps://github.com/cli/cli/releases/tag/v2.49.0\n".into()),
            )
            .build();
        let assertions = GhCliDetector.detect(&runner).await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::BinaryAvailable {
                name,
                path,
                version,
            } => {
                assert_eq!(name, "gh");
                assert_eq!(path, &PathBuf::from("gh"));
                assert_eq!(version.as_deref(), Some("2.49.0"));
            }
            other => panic!("expected BinaryAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gh_cli_detector_not_found() {
        // No on_run configured → run! returns Err → empty assertions
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = GhCliDetector.detect(&runner).await;
        assert!(assertions.is_empty());
    }

    #[tokio::test]
    async fn gh_cli_detector_command_error_returns_empty() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("gh", &["--version"], Err("failed".into()))
            .build();
        let assertions = GhCliDetector.detect(&runner).await;
        assert!(assertions.is_empty());
    }
}
