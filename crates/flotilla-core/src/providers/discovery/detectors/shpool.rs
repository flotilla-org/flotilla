//! Shpool session manager host detector.
//!
//! Checks whether the `shpool` binary is available on the host.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::CommandRunner;

/// Detects the shpool session manager binary.
pub struct ShpoolDetector;

#[async_trait]
impl HostDetector for ShpoolDetector {
    fn name(&self) -> &str {
        "shpool"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        if runner.exists("shpool", &["version"]).await {
            vec![EnvironmentAssertion::BinaryAvailable {
                name: "shpool".into(),
                path: PathBuf::from("shpool"),
                version: None,
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
    async fn shpool_detector_found() {
        let runner = DiscoveryMockRunner::builder()
            .tool_exists("shpool", true)
            .build();
        let assertions = ShpoolDetector.detect(&runner).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, path, version }
            if name == "shpool" && path == &PathBuf::from("shpool") && version.is_none()
        ));
    }

    #[tokio::test]
    async fn shpool_detector_not_found() {
        let runner = DiscoveryMockRunner::builder()
            .tool_exists("shpool", false)
            .build();
        let assertions = ShpoolDetector.detect(&runner).await;

        assert!(assertions.is_empty());
    }
}
